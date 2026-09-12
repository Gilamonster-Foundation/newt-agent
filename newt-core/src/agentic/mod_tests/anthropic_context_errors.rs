//! Streaming errors must return overflow evidence to the projection owner.
use super::*;

async fn dispatch(url: &str) -> anyhow::Result<Option<(anthropic_wire::AnthropicRound, bool)>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let retry = RetryPolicy::immediate(2);
    let dispatcher = AnthropicDispatch {
        smart_harness: None,
        client: &client,
        stream_client: &client,
        messages_url: url,
        api_key: None,
        retry: &retry,
        color: false,
        markdown: false,
        retain: None,
    };
    anthropic_dispatch_round(
        &dispatcher,
        &serde_json::json!({"model":"test","messages":[],"stream":true}),
        &[],
        true,
        None,
    )
    .await
}

#[tokio::test]
async fn anthropic_stream_context_exceeded_never_retries_unchanged() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[serde_json::json!({
            "type":"error", "error":{"type":"api_error", "message":"Context size has been exceeded"}
        })]))
        .mount(&server)
        .await;
    let error = dispatch(&server.uri()).await.unwrap_err();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn anthropic_stream_genuine_transient_error_still_retries() {
    let server = MockServer::start().await;
    let calls = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                sse(&[serde_json::json!({
                    "type":"error", "error":{"type":"overloaded_error", "message":"Overloaded"}
                })])
            } else {
                sse_text_reply(&["recovered"], 10, 1)
            }
        })
        .mount(&server)
        .await;
    let (reply, _) = dispatch(&server.uri()).await.unwrap().unwrap();
    assert_eq!(reply.text, "recovered");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

/// Grounds mocked HTTP classification in a real premature EOF. Keep accepting
/// until dispatch returns so an incorrect retry is observed, not replaced by a
/// connection-refused error after the fixture exits.
async fn dispatch_partial_http(status: u16, body: &str) -> (anyhow::Error, usize) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status} Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len() + 16
    );
    let (done, mut completed) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut calls = 0;
        loop {
            let (mut socket, _) = tokio::select! {
                _ = &mut completed => return calls,
                accepted = listener.accept() => accepted.unwrap(),
            };
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let len = socket.read(&mut chunk).await.unwrap();
                assert!(len > 0);
                request.extend_from_slice(&chunk[..len]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            calls += 1;
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let error = dispatch(&url).await.unwrap_err();
    done.send(()).unwrap();
    (error, peer.await.unwrap())
}

/// The complete overflow phrase is observed before the premature EOF.
#[tokio::test]
async fn anthropic_stream_partial_http_body_retains_context_exceeded() {
    let (error, calls) = dispatch_partial_http(
        500,
        r#"{"error":{"message":"Context size has been exceeded"}}"#,
    )
    .await;
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded,
        "observed overflow must survive the subsequent connection drop: {error}"
    );
    assert_eq!(calls, 1);
}

async fn assert_partial_http_fatal(status: u16) {
    // The body ends before any server message could establish overflow; the
    // observed permanent HTTP status still forbids unchanged-request backoff.
    let body = r#"{"type":"error","error":{"type":"invalid_"#;
    let (error, calls) = dispatch_partial_http(status, body).await;
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::Fatal,
        "truncated HTTP {status} must retain its permanent classification: {error}"
    );
    assert_eq!(calls, 1, "HTTP {status} must not replay the request");
    assert_eq!(
        observability::error_class(&error),
        Some(observability::ErrorClass::Model)
    );
    let diagnostic = error.to_string();
    assert!(diagnostic.contains(&format!("inference endpoint {status}")));
    assert!(diagnostic.contains(body), "partial body was lost: {error}");
    assert!(
        diagnostic.contains("body read failed:")
            && diagnostic.contains("error decoding response body"),
        "the body-read failure was lost: {error}"
    );
}

#[tokio::test]
async fn anthropic_stream_truncated_400_is_fatal_without_retry() {
    assert_partial_http_fatal(400).await;
}

#[tokio::test]
async fn anthropic_stream_truncated_401_is_fatal_without_retry() {
    assert_partial_http_fatal(401).await;
}

#[tokio::test]
async fn anthropic_stream_truncated_403_is_fatal_without_retry() {
    assert_partial_http_fatal(403).await;
}

#[tokio::test]
async fn anthropic_stream_truncated_503_remains_retryable() {
    let (error, calls) = dispatch_partial_http(503, r#"{"error":{"type":"overloaded_"#).await;
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::Retry
    );
    assert_eq!(
        calls, 3,
        "a genuine transient error still spends its retry budget"
    );
}
