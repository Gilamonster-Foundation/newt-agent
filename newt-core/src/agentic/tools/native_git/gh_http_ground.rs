//! Real-resource ground for the pure destination/redirect policy and HTTP
//! framing guards. Both upstream servers are local stand-ins, never a forge.
use super::*;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt;

#[test]
#[ignore = "real HTTP, Unix socket and optional real gh; weekly/release serial lane"]
fn gh_http_real_redirect_never_reaches_ungranted_listener() {
    let _env = crate::process_env::lock();
    let real_gh = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|p| p.join("gh"))
            .find(|p| p.is_file())
            .and_then(|p| p.canonicalize().ok())
    });
    // Always ground with a real HTTP client; also exercise gh's real transport
    // when installed. HTTPS redirects with gh require no TLS fixture: go-gh
    // sends plaintext HTTP even for TLS destinations through its Unix socket.
    for gh in std::iter::once(None).chain(real_gh.as_deref().map(Some)) {
        let dir = tempfile::Builder::new()
            .prefix("gh-http-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")
            .unwrap();
        let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let origin_addr = origin.local_addr().unwrap();
        origin.set_nonblocking(true).unwrap();
        let ungranted = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ungranted_addr = ungranted.local_addr().unwrap();
        ungranted.set_nonblocking(true).unwrap();
        let scheme = if gh.is_some() { "https" } else { "http" };
        let location = format!("{scheme}://{ungranted_addr}/redirected");
        let upstream = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match origin.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "approved origin was never contacted"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // macOS hands an accepted socket its listener's non-blocking mode,
            // so a read ahead of the client's first byte fails at once.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some((_, head, body)) = request_head(&bytes).unwrap() {
                    if bytes.len() >= head + body {
                        break;
                    }
                }
            }
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let result = with_upstream(
            dir.path(),
            &["forge.example".into()],
            || {
                for file in ["http.sock", "config.yml"] {
                    assert_eq!(
                        std::fs::metadata(dir.path().join(file))
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o777,
                        0o600
                    );
                }
                assert_eq!(
                    std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                    0o700
                );
                if let Some(gh) = gh {
                    let output = std::process::Command::new(gh)
                        .args([
                            "issue",
                            "view",
                            "1",
                            "-R",
                            "forge.example/o/r",
                            "--json",
                            "title",
                        ])
                        .env_clear()
                        .env("HOME", dir.path())
                        .env("GH_CONFIG_DIR", dir.path())
                        .env("GH_HOST", "forge.example")
                        .env("GH_ENTERPRISE_TOKEN", "fixture-token")
                        .env("GH_PROMPT_DISABLED", "1")
                        .env("GH_NO_UPDATE_NOTIFIER", "1")
                        .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
                        .env("GH_TELEMETRY", "0")
                        .env("DO_NOT_TRACK", "1")
                        .current_dir(dir.path())
                        .output()
                        .unwrap();
                    assert!(!output.status.success());
                    println!("real gh followed the stand-in redirect through the Unix socket");
                } else {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async {
                            let client = reqwest::Client::builder()
                                .no_proxy()
                                .timeout(Duration::from_secs(10))
                                .unix_socket(dir.path().join("http.sock"))
                                .build()
                                .unwrap();
                            let reply = client
                                .get("http://forge.example/read")
                                .send()
                                .await
                                .unwrap();
                            assert_eq!(reply.status(), 403);
                        });
                    println!(
                        "real HTTP client followed the stand-in redirect through the Unix socket"
                    );
                }
                Ok(())
            },
            |mut request, policy| {
                // Only the upstream address/TLS is a stand-in. The production
                // policy checks Host before this seam, and send uses the real
                // reqwest client with the supplied redirect policy.
                let address = if request.url().host_str() == Some("forge.example") {
                    origin_addr
                } else {
                    ungranted_addr
                };
                let path = request.url().path().to_string();
                *request.url_mut() = format!("http://{address}{path}").parse().unwrap();
                send(request, policy)
            },
        );
        upstream.join().unwrap();
        assert_eq!(
            result.unwrap_err(),
            format!("gh was redirected to '{ungranted_addr}', which is not granted")
        );
        assert_eq!(
            ungranted.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "an unapproved redirect reached the second listener"
        );
    }
}
