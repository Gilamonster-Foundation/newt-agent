//! #2268 regression evidence for overflow classification and HTTP body retention.

use newt_core::agentic::{error_class, DispatchError, ErrorClass};
use newt_core::retry::{classify, read_response_bytes, with_backoff, RetryPolicy, Retryability};
use std::cell::Cell;
use std::time::Duration;

fn err(message: &str) -> anyhow::Error {
    anyhow::anyhow!("{message}")
}

/// #2268: grounds the classifier's preserved-error-body assumption in a
/// real socket. The peer sends a complete error JSON chunk then closes
/// without the terminating HTTP chunk, as in the overflow cascade.
#[tokio::test]
async fn context_error_body_survives_a_subsequent_connection_drop() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("localhost", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let body = br#"{"error":{"message":"Context size has been exceeded."}}"#;
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        write!(
            stream,
            "HTTP/1.1 500 Internal Server Error\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.write_all(b"\r\n").unwrap();
        stream.flush().unwrap();
    });
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .get(format!("http://{address}/"))
        .send()
        .await
        .unwrap();
    let (observed, error) = read_response_bytes(response).await;
    peer.join().unwrap();
    assert!(
        error.is_some(),
        "the incomplete HTTP body must remain an error"
    );
    assert_eq!(observed, body, "observed server evidence must survive EOF");
}

/// #2268: an observed overflow is evidence to re-project context, never
/// permission for transport backoff to send the same request again.
#[test]
fn context_exceeded_precedes_transient_status_and_transport() {
    for message in [
        "inference endpoint 500: Context size has been exceeded.",
        "vLLM request failed: Context size has been exceeded. Connection handling canceled",
        "inference endpoint 500: prompt is too long: 1500 tokens > 1000 maximum",
        "inference endpoint 400: This model's maximum context length is 1000 tokens, your prompt contains 1500 tokens",
    ] {
        assert_eq!(classify(&err(message)), Retryability::ContextExceeded);
    }
    let wrapped = err("Context size has been exceeded.").context("request failed");
    assert_eq!(classify(&wrapped), Retryability::ContextExceeded);
}

/// The shared reader does not invent a body error for a complete response or
/// decide that content quoting an overflow is itself an overflow response.
#[tokio::test]
async fn complete_response_body_retains_content_without_an_error() {
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let body = "A diagnostic quotes Context size has been exceeded.";
    Mock::given(wiremock::matchers::method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let response = reqwest::get(server.uri()).await.unwrap();
    let (observed, error) = read_response_bytes(response).await;
    assert!(error.is_none());
    assert_eq!(observed, body.as_bytes());
}

/// #2268: the overflow exception must not turn ordinary transport
/// failures into terminal errors or suppress their existing retry budget.
#[tokio::test]
async fn context_exceeded_does_not_disable_genuine_transient_retry() {
    let attempts = Cell::new(0);
    let result = with_backoff(&RetryPolicy::immediate(2), || {
        attempts.set(attempts.get() + 1);
        async {
            if attempts.get() == 1 {
                Err(err("vLLM request failed: connection reset"))
            } else {
                Ok("recovered")
            }
        }
    })
    .await;
    assert_eq!(result.unwrap(), "recovered");
    assert_eq!(attempts.get(), 2);
}

/// #2268: even a generous transport policy must return an overflow to
/// the owner of context projection after exactly one unchanged attempt.
#[tokio::test]
async fn context_exceeded_never_retries_the_unchanged_request() {
    let attempts = Cell::new(0);
    let result: anyhow::Result<()> = with_backoff(&RetryPolicy::immediate(10), || {
        attempts.set(attempts.get() + 1);
        async {
            Err(err(
                "inference endpoint 500: Context size has been exceeded.",
            ))
        }
    })
    .await;
    assert!(result.unwrap_err().to_string().contains("Context size"));
    assert_eq!(attempts.get(), 1);
}

/// #2268: the server's capacity rejection has its own outcome, retained
/// through contextual wrapping for the solve events-file emitter.
#[test]
fn context_exceeded_http_failure_keeps_its_own_outcome() {
    let error = anyhow::Error::new(DispatchError::http_status(
        "inference endpoint 500: Context size has been exceeded.".into(),
    ))
    .context("dispatch failed");
    assert_eq!(error_class(&error), Some(ErrorClass::ContextExceeded));
}
