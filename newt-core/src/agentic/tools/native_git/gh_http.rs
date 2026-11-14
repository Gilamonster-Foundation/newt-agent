//! Per-request authority for gh's configured HTTP Unix socket. Each redirect
//! returns to gh and its next request is checked here before any upstream I/O.
use reqwest::{header::HeaderMap, redirect::Policy, Request, Response};
use std::{future::Future, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Reply = (u16, HeaderMap, Vec<u8>);
const HEAD_LIMIT: usize = 32 * 1024;
const REQUEST_LIMIT: usize = 1024 * 1024;
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

async fn forward<F: Future<Output = Result<Response, String>>>(
    approved: &[String],
    refused: &mut Option<String>,
    request: Request,
    send: impl FnOnce(Request, Policy) -> F,
) -> Result<Reply, String> {
    let host = request.url().host_str().ok_or("invalid gh request host")?;
    if request.url().scheme() != "https"
        || !request.url().username().is_empty()
        || request.url().password().is_some()
        || request.url().fragment().is_some()
    {
        return Err("invalid gh request destination".into());
    }
    if !approved.iter().any(|h| h == host) || request.url().port_or_known_default() != Some(443) {
        refused.get_or_insert_with(|| request.url().authority().to_string());
        return Ok((
            403,
            HeaderMap::new(),
            b"destination is not granted".to_vec(),
        ));
    }
    // Same reqwest transport and redirect discipline as web_fetch. The next
    // hop belongs to gh, which sends it back through this checked socket.
    let mut response = send(request, Policy::none()).await?;
    let mut headers = response.headers().clone();
    if headers
        .iter()
        .map(|(k, v)| k.as_str().len() + v.len() + 4)
        .sum::<usize>()
        > HEAD_LIMIT
    {
        return Err("gh response headers exceed the limit".into());
    }
    strip_hop_headers(&mut headers);
    let status = response.status().as_u16();
    if response
        .content_length()
        .is_some_and(|n| n > RESPONSE_LIMIT as u64)
    {
        return Err("gh response exceeds the limit".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "gh upstream body failed")?
    {
        if body.len() + chunk.len() > RESPONSE_LIMIT {
            return Err("gh response exceeds the limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, headers, body))
}

async fn send(request: Request, policy: Policy) -> Result<Response, String> {
    reqwest::Client::builder()
        .redirect(policy)
        .no_proxy()
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| "cannot construct gh upstream client")?
        .execute(request)
        .await
        .map_err(|_| "gh upstream request failed".into())
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all("connection")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_owned())
        .collect();
    for name in nominated.iter().map(String::as_str).chain([
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
    ]) {
        headers.remove(name);
    }
}

// httparse already parses HTTP for reqwest. This endpoint accepts only one
// origin-form HTTP/1.1 request with a Content-Length body: no chunked requests,
// CONNECT, upgrades, pipelining or ambiguous framing.
fn request_head(bytes: &[u8]) -> Result<Option<(Request, usize, usize)>, String> {
    let mut slots = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut slots);
    let length = match parsed
        .parse(bytes)
        .map_err(|_| "malformed gh HTTP request")?
    {
        httparse::Status::Partial if bytes.len() < HEAD_LIMIT => return Ok(None),
        httparse::Status::Partial => return Err("gh request headers exceed the limit".into()),
        httparse::Status::Complete(n) if n <= HEAD_LIMIT => n,
        _ => return Err("gh request headers exceed the limit".into()),
    };
    let path = parsed.path.ok_or("missing gh request path")?;
    if parsed.version != Some(1) || !path.starts_with('/') || path.starts_with("//") {
        return Err("gh requires an origin-form HTTP/1.1 request".into());
    }
    let mut headers = HeaderMap::new();
    for header in parsed.headers {
        let name = reqwest::header::HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| "invalid gh header name")?;
        let value = reqwest::header::HeaderValue::from_bytes(header.value)
            .map_err(|_| "invalid gh header value")?;
        headers.append(name, value);
    }
    if headers.get_all("host").iter().count() != 1
        || headers.get_all("content-length").iter().count() > 1
        || ["transfer-encoding", "expect", "upgrade"]
            .iter()
            .any(|h| headers.contains_key(*h))
    {
        return Err("unsupported gh request framing".into());
    }
    let host = headers["host"]
        .to_str()
        .map_err(|_| "invalid gh request host")?;
    // Validate the authority independently: no userinfo, fragments, paths or
    // backslashes can turn the checked Host into a different destination.
    let authority: reqwest::Url = format!("https://{host}/")
        .parse()
        .map_err(|_| "invalid gh request host")?;
    if authority.path() != "/"
        || authority.query().is_some()
        || authority.fragment().is_some()
        || !authority.username().is_empty()
        || authority.password().is_some()
        || host.contains(['/', '\\', '@', '#', '?'])
    {
        return Err("invalid gh request host".into());
    }
    let url: reqwest::Url = format!("https://{host}{path}")
        .parse()
        .map_err(|_| "invalid gh request path")?;
    if url.host_str() != authority.host_str()
        || url.port() != authority.port()
        || url.fragment().is_some()
    {
        return Err("invalid gh request target".into());
    }
    let method = reqwest::Method::from_bytes(parsed.method.ok_or("missing gh method")?.as_bytes())
        .map_err(|_| "invalid gh method")?;
    if !matches!(
        method,
        reqwest::Method::GET | reqwest::Method::POST | reqwest::Method::HEAD
    ) {
        return Err("unsupported gh read HTTP method".into());
    }
    let body_length = headers
        .get("content-length")
        .map(|v| {
            v.to_str()
                .ok()
                .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or("invalid gh body length")
        })
        .transpose()?
        .unwrap_or(0);
    if body_length > REQUEST_LIMIT {
        return Err("gh request exceeds the limit".into());
    }
    strip_hop_headers(&mut headers);
    headers.remove("host");
    let mut request = Request::new(method, url);
    *request.headers_mut() = headers;
    Ok(Some((request, length, body_length)))
}

async fn read_request<R: tokio::io::AsyncRead + Unpin>(stream: &mut R) -> Result<Request, String> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        if let Some((mut request, start, length)) = request_head(&bytes)? {
            let end = start + length;
            let received = bytes.len();
            if received > end {
                return Err("pipelined gh requests are unsupported".into());
            }
            bytes.resize(end, 0);
            stream
                .read_exact(&mut bytes[received..end])
                .await
                .map_err(|_| "incomplete gh body")?;
            if length > 0 {
                *request.body_mut() = Some(bytes[start..end].to_vec().into());
            }
            return Ok(request);
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|_| "cannot read gh request")?;
        if n == 0 {
            return Err("incomplete gh request".into());
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

async fn write_reply(stream: &mut tokio::net::UnixStream, reply: Reply) -> Result<(), String> {
    let (status, headers, body) = reply;
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\n",
        reqwest::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("Response")
    )
    .into_bytes();
    for (name, value) in &headers {
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(
        format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    stream
        .write_all(&head)
        .await
        .map_err(|_| "cannot write gh response")?;
    stream
        .write_all(&body)
        .await
        .map_err(|_| "cannot write gh response")?;
    Ok(())
}

/// Run a child under the per-call approved-host socket. Only the read broker
/// adopts this today; PR-create can use the same boundary in a later change.
pub(super) fn with_approved_http<T>(
    dir: &Path,
    approved: &[String],
    child: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    with_upstream(dir, approved, child, send)
}

fn with_upstream<T, F: Future<Output = Result<Response, String>>>(
    dir: &Path,
    approved: &[String],
    child: impl FnOnce() -> Result<T, String>,
    upstream: impl Fn(Request, Policy) -> F + Send,
) -> Result<T, String> {
    use std::io::Write;
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{OpenOptionsExt, PermissionsExt},
    };
    let socket = dir.join("http.sock");
    if socket.as_os_str().as_bytes().len() >= 104 {
        return Err("gh socket path is too long".into());
    }
    let listener = std::os::unix::net::UnixListener::bind(&socket)
        .map_err(|_| "cannot create private gh socket")?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot protect gh socket")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "cannot configure gh socket")?;
    let mut config = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("config.yml"))
        .map_err(|_| "cannot create private gh config")?;
    let yaml = serde_yaml::to_string(&std::collections::BTreeMap::from([(
        "http_unix_socket",
        socket.to_str().ok_or("invalid gh socket path")?,
    )]))
    .map_err(|_| "cannot encode private gh config")?;
    config
        .write_all(yaml.as_bytes())
        .map_err(|_| "cannot write private gh config")?;
    std::thread::scope(|scope| {
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel();
        let worker = scope.spawn(move || -> Result<Option<String>, String> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()
                .map_err(|_| "cannot start gh HTTP boundary")?;
            runtime.block_on(async {
                let listener = tokio::net::UnixListener::from_std(listener)
                    .map_err(|_| "cannot start gh HTTP listener")?;
                let _ = ready.send(());
                let mut refused = None;
                loop {
                    let (mut stream, _) = tokio::select! {
                        _ = &mut stopped => return Ok(refused),
                        accepted = listener.accept() => accepted.map_err(|_| "gh socket accept failed")?,
                    };
                    let exchange = async {
                        let reply = match read_request(&mut stream).await {
                            Ok(request) => forward(approved, &mut refused, request, &upstream).await,
                            Err(reason) => Err(reason),
                        };
                        match reply {
                            Ok(reply) => write_reply(&mut stream, reply).await,
                            Err(reason) => {
                                let _ = write_reply(&mut stream, (502, HeaderMap::new(), b"gh request refused".to_vec())).await;
                                Err(reason)
                            }
                        }
                    };
                    let completed = tokio::select! {
                        _ = &mut stopped => None,
                        result = tokio::time::timeout(TIMEOUT, exchange) => Some(result),
                    };
                    match completed {
                        None => return Ok(refused),
                        Some(result) => if let Err(reason) = result
                            .map_err(|_| "gh HTTP request timed out".to_string()).and_then(|r| r) {
                            return if refused.is_some() { Ok(refused) } else { Err(reason) };
                        },
                    }
                }
            })
        });
        started
            .recv()
            .map_err(|_| "gh HTTP boundary did not start")?;
        let result = child();
        let _ = stop.send(());
        let refused = worker
            .join()
            .map_err(|_| "gh HTTP boundary stopped unexpectedly")??;
        if let Some(host) = refused {
            return Err(format!(
                "gh was redirected to '{host}', which is not granted"
            ));
        }
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(host: &str) -> Request {
        Request::new(
            reqwest::Method::GET,
            format!("https://{host}/read").parse().unwrap(),
        )
    }

    #[tokio::test]
    async fn gh_http_only_approved_destinations_reach_upstream() {
        for (host, expected) in [("forge.example", 200), ("ungranted.example", 403)] {
            let mut refused = None;
            let sent = std::cell::Cell::new(0);
            let reply = forward(
                &["forge.example".into()],
                &mut refused,
                request(host),
                |_, _| {
                    sent.set(sent.get() + 1);
                    std::future::ready(Ok(http::Response::new("read body").into()))
                },
            )
            .await
            .unwrap();
            assert_eq!(
                reply.0, expected,
                "unapproved request crossed the HTTP boundary"
            );
            assert_eq!(sent.get(), usize::from(expected == 200));
            assert_eq!(refused.as_deref(), (expected == 403).then_some(host));
        }
    }

    #[tokio::test]
    async fn gh_http_upstream_redirects_are_disabled() {
        let mut refused = None;
        let reply = forward(
            &["forge.example".into()],
            &mut refused,
            request("forge.example"),
            |_, policy| {
                assert_eq!(
                    format!("{policy:?}"),
                    "Policy(None)",
                    "upstream must return redirects to the checked socket boundary"
                );
                std::future::ready(Ok(http::Response::builder()
                    .status(302)
                    .header("location", "https://ungranted.example/read")
                    .body("")
                    .unwrap()
                    .into()))
            },
        )
        .await
        .unwrap();
        assert_eq!(reply.0, 302);
        assert!(refused.is_none());
    }

    #[tokio::test]
    async fn gh_http_reads_a_split_post_body_without_resources() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        let (_, request) = tokio::join!(
            async {
                writer.write_all(b"POST /graphql HTTP/1.1\r\nHost: FORGE.EXAMPLE\r\nContent-Length: 2\r\nConnection: close, x-private\r\nX-Private: stripped\r\nAuthorization: fixture\r\n\r\n{").await.unwrap();
                tokio::task::yield_now().await;
                writer.write_all(b"}").await.unwrap();
            },
            read_request(&mut reader)
        );
        let request = request.unwrap();
        assert_eq!(request.url().as_str(), "https://forge.example/graphql");
        assert_eq!(request.body().unwrap().as_bytes().unwrap(), b"{}");
        assert_eq!(request.headers()["authorization"], "fixture");
        for key in ["host", "content-length", "connection", "x-private"] {
            assert!(!request.headers().contains_key(key));
        }
    }

    #[test]
    fn gh_http_refuses_ambiguous_or_unbounded_request_framing() {
        for raw in [
            "GET / HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: forge.example\r\nHost: other.example\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: forge.example\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: forge.example\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: forge.example\r\nContent-Length: -1\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: forge.example\r\nContent-Length: 1048577\r\n\r\n",
            "GET / HTTP/1.0\r\nHost: forge.example\r\n\r\n",
            "CONNECT forge.example:443 HTTP/1.1\r\nHost: forge.example\r\n\r\n",
            "GET https://other.example/ HTTP/1.1\r\nHost: forge.example\r\n\r\n",
            "GET //other.example/ HTTP/1.1\r\nHost: forge.example\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: forge.example@other.example\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: forge.example/other\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: forge.example\r\nUpgrade: websocket\r\n\r\n",
            "POST / HTTP/1.1\r\nHost: forge.example\r\nExpect: 100-continue\r\n\r\n",
            "DELETE / HTTP/1.1\r\nHost: forge.example\r\n\r\n",
        ] { assert!(request_head(raw.as_bytes()).is_err(), "{raw}"); }
        assert!(request_head(&vec![b'x'; HEAD_LIMIT + 1]).is_err());
    }

    #[tokio::test]
    async fn gh_http_response_bounds_and_redirect_headers_are_enforced() {
        for body in [Vec::new(), vec![b'x'; RESPONSE_LIMIT + 1]] {
            let too_large = !body.is_empty();
            let reply = forward(
                &["forge.example".into()],
                &mut None,
                request("forge.example"),
                |_, _| {
                    std::future::ready(Ok(http::Response::builder()
                        .status(302)
                        .header("location", "https://other.example/")
                        .header("connection", "x-private")
                        .header("x-private", "stripped")
                        .body(body)
                        .unwrap()
                        .into()))
                },
            )
            .await;
            if too_large {
                assert!(reply.unwrap_err().contains("exceeds"));
            } else {
                let reply = reply.unwrap();
                assert_eq!(reply.1["location"], "https://other.example/");
                assert!(!reply.1.contains_key("x-private"));
                assert!(!reply.1.contains_key("connection"));
            }
        }
        let mut refused = None;
        assert_eq!(
            forward(
                &["forge.example".into()],
                &mut refused,
                request("forge.example:8443"),
                |_, _| async { panic!("unapproved port must not dial") }
            )
            .await
            .unwrap()
            .0,
            403
        );
        assert_eq!(refused.as_deref(), Some("forge.example:8443"));
    }

    #[tokio::test]
    async fn gh_http_invalid_destinations_and_large_headers_never_escape() {
        for url in [
            "https://user:secret@other.example/",
            "http://forge.example/",
            "https://forge.example/#fragment",
        ] {
            let mut refused = None;
            assert!(forward(
                &["forge.example".into()],
                &mut refused,
                Request::new(reqwest::Method::GET, url.parse().unwrap()),
                |_, _| async { panic!("invalid destination must not dial") }
            )
            .await
            .is_err());
            assert!(
                refused.is_none(),
                "userinfo must not be exposed in a host refusal"
            );
        }
        let reply = forward(
            &["forge.example".into()],
            &mut None,
            request("forge.example"),
            |_, _| async {
                Ok(http::Response::builder()
                    .header("x-large", "x".repeat(HEAD_LIMIT))
                    .body("")
                    .unwrap()
                    .into())
            },
        )
        .await;
        assert!(reply.unwrap_err().contains("headers exceed"));
        let header = format!(
            "GET / HTTP/1.1\r\nHost: forge.example\r\nX-Large: {}\r\n\r\n",
            "x".repeat(HEAD_LIMIT)
        );
        assert!(request_head(header.as_bytes())
            .unwrap_err()
            .contains("headers exceed"));
    }
}

#[cfg(test)]
#[path = "gh_http_ground.rs"]
mod ground;
