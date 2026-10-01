//! #2665 — the spawned `newt` reaches no peer but the fixture's own.
//!
//! The purity and PTY suites run the real binary. Under the developer's
//! environment that binary used to reach three live host services CI never
//! has: the local Ollama on `127.0.0.1:11434` (the test's `OLLAMA_HOST` was
//! ignored by the chat path), the Herdr pane socket (`HERDR_*` inherited from
//! the pane the tests ran in — it sent `pane.release_agent` to the operator's
//! own session), and the host `~/.gitconfig`. This file is the proof that the
//! shared setup in `common` closes all three, measured against resources the
//! test owns rather than inferred from a scrub list:
//!
//! * the loopback inference peer saw the startup probe — so `OLLAMA_HOST`
//!   routed the fallback backend there (red on the pre-#2665 config: the
//!   peer sees nothing and the real Ollama answers instead);
//! * a Herdr socket in the root, advertised through this process's OWN
//!   environment, saw no connection — so `env_clear()` dropped the triple
//!   (the negative control re-pins it and watches the connection arrive);
//! * `/byline` names the fixture git identity — so `git config --get` read
//!   the fixture's configuration, not the host's file.
//!
//! Real-resource tier: a real process on a pipe, a real Unix socket. Unix
//! only for the socket; the PTY suites it grounds are Unix too.

#![cfg(unix)]

mod common;

use std::os::unix::net::UnixListener;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// The model the bare-install fallback declares, so adoption finds it served.
const MODEL: &str = "llama3.1:8b";

/// `HERDR_*` as a Herdr pane exports it, pointed at a socket in `root`.
fn herdr_triple(socket: &std::path::Path) -> [(&'static str, String); 3] {
    [
        ("HERDR_ENV", "1".to_string()),
        ("HERDR_PANE_ID", "w0:p0".to_string()),
        ("HERDR_SOCKET_PATH", socket.display().to_string()),
    ]
}

struct Session {
    peer: wiremock::MockServer,
    pane: UnixListener,
    stdout: String,
}

/// One piped lean session: empty config (the fallback backend), `OLLAMA_HOST`
/// at the loopback peer, a pane socket the test owns, `/byline` then EOF.
/// `rig` runs after the shared setup, so it can re-pin what the setup drops.
async fn session(rig: impl FnOnce(&mut Command)) -> Session {
    let root = common::isolated_root();
    std::fs::create_dir_all(root.path().join(".newt")).expect("config dir");
    std::fs::write(root.path().join(".newt/config.toml"), "").expect("empty config");
    let socket = root.path().join("herdr.sock");
    let pane = UnixListener::bind(&socket).expect("bind the fixture's pane socket");
    pane.set_nonblocking(true).expect("non-blocking accept");
    let peer = common::inference_peer(MODEL).await;

    // The ambient hazard, reproduced in THIS process: the tests ran inside a
    // Herdr pane, so the triple was in the parent environment. Serialized
    // below; this binary owns no other tests.
    for (key, value) in herdr_triple(&socket) {
        std::env::set_var(key, value);
    }

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_newt"));
    common::isolate(&mut cmd, root.path());
    cmd.args(["--no-splash", "--plain"])
        .env("OLLAMA_HOST", peer.uri())
        .env("NEWT_NO_MODEL_PULL", "1")
        .env("TERM", "dumb")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    rig(&mut cmd);

    let mut child = cmd.spawn().expect("spawn newt");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // Drain both pipes while waiting, so a stalled child is reported with
    // whatever it said rather than as a bare timeout.
    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::try_join!(
            async {
                stdin.write_all(b"/byline\n").await?;
                drop(stdin); // the EOF that ends the session
                Ok::<(), std::io::Error>(())
            },
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            child.wait()
        )
    })
    .await;
    let status = match completed {
        Ok(Ok(((), _, _, status))) => status,
        failure => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
            panic!(
                "newt did not end on EOF within its budget: {failure:?}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
        }
    };
    assert!(
        status.success(),
        "newt exited {status}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    Session {
        peer,
        pane,
        stdout: String::from_utf8_lossy(&out).into_owned(),
    }
}

/// `accept` on the non-blocking listener: a connection the child made is
/// queued in the backlog whether or not anyone read it, so `WouldBlock` is
/// "nobody connected", not "nobody was listening".
fn pane_was_contacted(pane: &UnixListener) -> bool {
    match pane.accept() {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(e) => panic!("pane socket accept failed: {e}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(hermetic_spawn)]
async fn the_spawned_newt_reaches_only_the_fixtures_peers() {
    let s = session(|_| {}).await;

    let requests = s.peer.received_requests().await.expect("recorded requests");
    assert!(
        requests
            .iter()
            .any(|r| r.method == "GET" && r.url.path() == "/api/tags"),
        "the startup probe never reached the fixture's inference peer, so the \
         fallback backend was not routed by OLLAMA_HOST (#2665)\nrequests: {:?}\nstdout:\n{}",
        requests
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect::<Vec<_>>(),
        s.stdout
    );
    assert!(
        !pane_was_contacted(&s.pane),
        "the child connected to the pane socket: HERDR_* leaked through the setup"
    );
    let byline = format!(
        "Co-authored-by: {} <{}>",
        common::GIT_FIXTURE_NAME,
        common::GIT_FIXTURE_EMAIL
    );
    assert!(
        s.stdout.contains(&byline),
        "/byline does not carry the fixture git identity, so `git config` was \
         not answered by the fixture\nstdout:\n{}",
        s.stdout
    );
}

/// Negative control, so the silence above is evidence: with the triple
/// re-pinned after the setup (what inheriting it looked like), the same session
/// does connect to the pane socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(hermetic_spawn)]
async fn an_inherited_herdr_triple_would_reach_the_pane_socket() {
    let s = session(|cmd| {
        let socket = std::env::var("HERDR_SOCKET_PATH").expect("set by session()");
        for (key, value) in herdr_triple(std::path::Path::new(&socket)) {
            cmd.env(key, value);
        }
    })
    .await;
    assert!(
        pane_was_contacted(&s.pane),
        "the detector never saw a connection even with HERDR_* pinned; the \
         hermetic assertion would be vacuous"
    );
}
