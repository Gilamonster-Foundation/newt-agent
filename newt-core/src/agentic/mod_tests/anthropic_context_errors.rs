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

/// Grounds HTTP error-body classification in a real truncated socket response:
/// the complete overflow phrase is observed before the premature EOF.
#[tokio::test]
async fn anthropic_stream_partial_http_body_retains_context_exceeded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let url = format!("http://{}/v1/messages", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
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
        let body = r#"{"error":{"message":"Context size has been exceeded"}}"#;
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len() + 16
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let error = dispatch(&url).await.unwrap_err();
    peer.await.unwrap();
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::ContextExceeded,
        "observed overflow must survive the subsequent connection drop: {error}"
    );
}
