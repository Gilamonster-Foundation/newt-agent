//! Local streaming grounds the shared SSE decoder in real HTTP responses.

use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use newt_inference::local::{LocalOllamaBackend, LocalVllmBackend};
use newt_inference::{ChatRequest, InferenceBackend, RetryPolicy};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn local_stream_preserves_answer_usage_and_model_without_exposing_reasoning() {
    let server = MockServer::start().await;
    let data = concat!(
        "data: {\"id\":\"stream-1\",\"model\":\"served-model\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"private reasoning\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"<thi\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"nk>inline reasoning</think>Hell\"}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"o 🌍\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":15,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(data),
        )
        .expect(1)
        .mount(&server)
        .await;
    let backend = LocalVllmBackend::new(server.uri(), "requested-model")
        .with_retry_policy(RetryPolicy::immediate(0));
    let reply = backend
        .complete(ChatRequest::new().user("hello").max_tokens(32))
        .await
        .expect("the local transport must decode streamed completions");
    assert_eq!(reply.content, "Hello 🌍");
    assert_eq!(reply.model_id, "served-model");
    let usage = reply.usage.unwrap();
    assert_eq!(usage.input_tokens, 15);
    assert_eq!(usage.output_tokens, 4);
    let requests = server.received_requests().await.unwrap();
    let request: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(request["stream"], true);
    assert_eq!(request["stream_options"]["include_usage"], true);
    assert_eq!(request["max_tokens"], 32);
}

#[tokio::test]
async fn local_ollama_stream_assembles_content_and_final_usage() {
    let server = MockServer::start().await;
    let data = concat!(
        "{\"model\":\"served-model\",\"message\":{\"role\":\"assistant\",\"content\":\"<think>private\"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\" reasoning</think>Hello \"},\"done\":false}\n",
        "{\"message\":{\"role\":\"assistant\",\"content\":\"world\"},\"done\":true,\"prompt_eval_count\":12,\"eval_count\":3}\n",
    );
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(data, "application/x-ndjson"))
        .expect(1)
        .mount(&server)
        .await;
    let backend = LocalOllamaBackend::new(server.uri(), "requested-model")
        .with_retry_policy(RetryPolicy::immediate(0));
    let reply = backend
        .complete(ChatRequest::new().user("hello").max_tokens(32))
        .await
        .expect("the native local transport must decode streamed completions");
    assert_eq!(reply.content, "Hello world");
    assert_eq!(reply.model_id, "requested-model");
    let usage = reply.usage.unwrap();
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 3);
    let requests = server.received_requests().await.unwrap();
    let request: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(request["stream"], true);
    assert_eq!(request["options"]["num_predict"], 32);
}

#[tokio::test]
async fn truncated_local_ollama_stream_is_not_reported_as_a_complete_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "{\"message\":{\"content\":\"partial\"},\"done\":false}\n",
            "application/x-ndjson",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let backend =
        LocalOllamaBackend::new(server.uri(), "test").with_retry_policy(RetryPolicy::immediate(0));
    let error = backend
        .complete(ChatRequest::new().user("hello"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("ended before done"), "{error}");
}

#[tokio::test]
async fn local_ollama_stream_requires_ndjson_record_boundaries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "{\"message\":{\"content\":\"first\"},\"done\":false}",
                "{\"message\":{\"content\":\"second\"},\"done\":true}",
            ),
            "application/x-ndjson",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let backend =
        LocalOllamaBackend::new(server.uri(), "test").with_retry_policy(RetryPolicy::immediate(0));
    let error = backend
        .complete(ChatRequest::new().user("hello"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("invalid Ollama stream"),
        "{error}"
    );
}

#[tokio::test]
async fn local_ollama_stream_rejects_a_non_object_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "{\"message\":\"corrupt\",\"done\":false}\n",
                "{\"message\":{\"content\":\"answer\"},\"done\":true}\n",
            ),
            "application/x-ndjson",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let backend =
        LocalOllamaBackend::new(server.uri(), "test").with_retry_policy(RetryPolicy::immediate(0));
    let error = backend
        .complete(ChatRequest::new().user("hello"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("message is not an object"),
        "{error}"
    );
}

#[tokio::test]
async fn local_ollama_error_frame_suppresses_partial_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "{\"message\":{\"content\":\"partial\"},\"done\":false}\n",
                "{\"error\":\"Context size has been exceeded\"}\n",
            ),
            "application/x-ndjson",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let backend =
        LocalOllamaBackend::new(server.uri(), "test").with_retry_policy(RetryPolicy::immediate(0));
    let error = backend
        .complete(ChatRequest::new().user("hello"))
        .await
        .unwrap_err();
    assert_eq!(
        newt_core::retry::classify(&error),
        newt_core::retry::Retryability::ContextExceeded,
        "{error}"
    );
}

#[tokio::test]
async fn truncated_local_stream_is_not_reported_as_a_complete_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let backend =
        LocalVllmBackend::new(server.uri(), "test").with_retry_policy(RetryPolicy::immediate(3));
    assert!(backend
        .complete(ChatRequest::new().user("hello"))
        .await
        .is_err());
}

#[tokio::test]
async fn local_stream_context_error_keeps_its_classification_without_unchanged_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"error\":{\"message\":\"Context size has been exceeded\"}}\n\n",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let backend =
        LocalVllmBackend::new(server.uri(), "test").with_retry_policy(RetryPolicy::immediate(3));
    let error = backend
        .complete(ChatRequest::new().user("hello"))
        .await
        .unwrap_err();
    assert_eq!(
        newt_core::retry::classify(&error),
        newt_core::retry::Retryability::ContextExceeded,
        "the observed SSE error must survive response decoding: {error}"
    );
}

#[tokio::test]
async fn changing_local_timeout_preserves_the_injected_client_configuration() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(header("x-client-fixture", "preserved"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "{\"message\":{\"content\":\"ollama\"},\"done\":true}\n",
            "application/x-ndjson",
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("x-client-fixture", "preserved"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"openai\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            ),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-client-fixture", "preserved".parse().unwrap());
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    let backends: [Box<dyn InferenceBackend>; 2] = [
        Box::new(
            LocalOllamaBackend::new(server.uri(), "test")
                .with_client(client.clone())
                .with_timeout(Duration::from_secs(5))
                .with_retry_policy(RetryPolicy::immediate(0)),
        ),
        Box::new(
            LocalVllmBackend::new(server.uri(), "test")
                .with_client(client)
                .with_timeout(Duration::from_secs(5))
                .with_retry_policy(RetryPolicy::immediate(0)),
        ),
    ];
    for backend in backends {
        backend
            .complete(ChatRequest::new().user("hello"))
            .await
            .expect("changing the deadline must retain caller-owned client settings");
    }
}

async fn request_body(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before its headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|slice| slice == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let content_length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .expect("the JSON request has a bounded content length");
    while bytes.len() - header_end < content_length {
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "request ended before its JSON body");
        bytes.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap()
}

#[tokio::test]
async fn progressing_local_stream_resets_its_idle_timeout() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (advance_tx, mut advance_rx) = tokio::sync::mpsc::unbounded_channel();
    let (written_tx, mut written_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = request_body(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n").await.unwrap();
        started_tx.send(request).unwrap();
        for frame in [
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"still progressing\"},\"finish_reason\":\"stop\"}]}\n\n".as_slice(),
            b"data: [DONE]\n\n".as_slice(),
        ] {
            advance_rx.recv().await.expect("test advances the stream");
            let _ = socket.write_all(frame).await;
            written_tx.send(()).unwrap();
        }
        let _ = socket.shutdown().await;
    });
    let backend = LocalVllmBackend::new(endpoint, "stream-fixture")
        .with_client(reqwest::Client::new())
        .with_timeout(Duration::from_secs(5))
        .with_retry_policy(RetryPolicy::immediate(0));
    let mut completion = Box::pin(backend.complete(ChatRequest::new().user("keep generating")));
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = started_rx => request.unwrap(),
            result = &mut completion => panic!("stream ended before its first frame: {result:?}"),
        }
    })
    .await
    .expect("the first streamed frame must arrive");
    assert_eq!(request["stream"], true);

    tokio::time::pause();
    for index in 0..2 {
        tokio::time::advance(Duration::from_secs(3)).await;
        advance_tx.send(()).unwrap();
        written_rx.recv().await.unwrap();
        if index == 0 {
            tokio::select! {
                biased;
                result = &mut completion => panic!("stream ended before its terminal frame: {result:?}"),
                _ = tokio::task::yield_now() => {}
            }
        }
    }
    tokio::time::resume();
    let reply = tokio::time::timeout(Duration::from_secs(5), &mut completion)
        .await
        .expect("the completed stream must return")
        .expect("progress must reset the idle read timeout");
    server.await.unwrap();
    assert_eq!(reply.content, "still progressing");
}

struct SlotCase {
    request: Value,
    released_before_generation_finished: bool,
    write_observed_close: bool,
}

async fn local_ollama_slot_case(use_streaming_backend: bool) -> SlotCase {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let slot = Arc::new(tokio::sync::Semaphore::new(1));
    let server_slot = slot.clone();
    let write_observed_close = Arc::new(AtomicBool::new(false));
    let server_observed_close = write_observed_close.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let permit = server_slot.acquire_owned().await.unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = request_body(&mut socket).await;
        let streaming = request["stream"] == true;
        if streaming {
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n{\"message\":{\"role\":\"assistant\",\"content\":\"generating\"},\"done\":false}\n").await.unwrap();
            started_tx.send(request.clone()).unwrap();
            let token = format!(
                "{{\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"}},\"done\":false}}\n",
                "x".repeat(64 * 1024)
            );
            while socket.write_all(token.as_bytes()).await.is_ok() {}
            server_observed_close.store(true, Ordering::SeqCst);
        } else {
            started_tx.send(request.clone()).unwrap();
            let _ = finish_rx.await;
        }
        drop(permit);
        request
    });

    let retained_client = reqwest::Client::new();
    let request_client = retained_client.clone();
    let mut completion: Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        if use_streaming_backend {
            let backend = LocalOllamaBackend::new(endpoint, "slot-fixture")
                .with_client(request_client)
                .with_retry_policy(RetryPolicy::immediate(0));
            Box::pin(async move {
                let _ = backend
                    .complete(ChatRequest::new().user("keep generating"))
                    .await;
            })
        } else {
            Box::pin(async move {
                let _ = request_client
                    .post(endpoint)
                    .json(&json!({"stream":false}))
                    .send()
                    .await;
            })
        };
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = started_rx => request.unwrap(),
            () = &mut completion => panic!("generation ended before the server barrier"),
        }
    })
    .await
    .expect("generation must reach the server");
    drop(completion);

    let released_before_generation_finished = if use_streaming_backend {
        let acquired =
            tokio::time::timeout(Duration::from_secs(2), slot.clone().acquire_owned()).await;
        let released = acquired.is_ok();
        drop(acquired);
        released
    } else {
        let released = slot.clone().try_acquire_owned().is_ok();
        assert!(!released, "the non-streaming server must retain its slot");
        released
    };
    let _ = finish_tx.send(());
    let server_request = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the fixture server must finish")
        .unwrap();
    drop(retained_client);
    assert_eq!(server_request, request);
    SlotCase {
        request,
        released_before_generation_finished,
        write_observed_close: write_observed_close.load(Ordering::SeqCst),
    }
}

#[tokio::test]
async fn dropping_local_ollama_future_releases_the_server_generation_slot() {
    let streamed = local_ollama_slot_case(true).await;
    let non_streamed = local_ollama_slot_case(false).await;
    assert_eq!(streamed.request["stream"], true);
    assert!(
        streamed.released_before_generation_finished,
        "the server must release its generation slot when its streamed write observes disconnect"
    );
    assert!(streamed.write_observed_close);
    assert_eq!(non_streamed.request["stream"], false);
    assert!(!non_streamed.released_before_generation_finished);
    assert!(!non_streamed.write_observed_close);
}

async fn truncated_response(
    status: &'static str,
    content_type: &'static str,
    body: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        request_body(&mut socket).await;
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len() + 128);
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    (endpoint, server)
}

/// Grounds error precedence in an actual incomplete HTTP frame. The server's
/// observed capacity rejection must survive the connection failure after it.
#[tokio::test]
async fn observed_local_context_error_survives_a_truncated_http_frame() {
    for (openai, status, content_type, body) in [
        (
            true,
            "400 Bad Request",
            "application/json",
            "Context size has been exceeded",
        ),
        (
            false,
            "400 Bad Request",
            "application/json",
            "Context size has been exceeded",
        ),
        (
            true,
            "200 OK",
            "text/event-stream",
            "data: {\"error\":{\"message\":\"Context size has been exceeded\"}}\n\n",
        ),
        (
            false,
            "200 OK",
            "application/x-ndjson",
            "{\"error\":\"Context size has been exceeded\"}\n",
        ),
    ] {
        let (endpoint, server) = truncated_response(status, content_type, body).await;
        let backend: Box<dyn InferenceBackend> = if openai {
            Box::new(
                LocalVllmBackend::new(endpoint, "test")
                    .with_retry_policy(RetryPolicy::immediate(3)),
            )
        } else {
            Box::new(
                LocalOllamaBackend::new(endpoint, "test")
                    .with_retry_policy(RetryPolicy::immediate(3)),
            )
        };
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            backend.complete(ChatRequest::new().user("hello")),
        )
        .await
        .unwrap()
        .unwrap_err();
        server.await.unwrap();
        assert_eq!(
            newt_core::retry::classify(&error),
            newt_core::retry::Retryability::ContextExceeded,
            "{error}"
        );
        assert!(
            error.to_string().contains("response body read failure"),
            "retain the transport failure alongside the observed error: {error}"
        );
    }
}

/// The same broken socket cannot turn an observed authorization failure into a
/// retry, or turn a genuinely transient server overload into a terminal stop.
#[tokio::test]
async fn streamed_provider_status_survives_the_following_transport_failure() {
    use newt_core::retry::Retryability;
    for (body, expected, diagnostic) in [
        (
            "data: {\"error\":{\"code\":503,\"message\":\"fixture overloaded\"}}\n\n",
            Retryability::Retry,
            "fixture overloaded",
        ),
        (
            "data: {\"error\":{\"code\":401,\"message\":\"fixture unauthorized\"}}\n\n",
            Retryability::Fatal,
            "fixture unauthorized",
        ),
    ] {
        let (endpoint, server) = truncated_response("200 OK", "text/event-stream", body).await;
        let backend =
            LocalVllmBackend::new(endpoint, "test").with_retry_policy(RetryPolicy::immediate(0));
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            backend.complete(ChatRequest::new().user("hello")),
        )
        .await
        .unwrap()
        .unwrap_err();
        server.await.unwrap();
        assert_eq!(newt_core::retry::classify(&error), expected, "{error}");
        assert!(
            error.to_string().contains(diagnostic),
            "retain the observed provider error: {error}"
        );
        assert!(
            error.to_string().contains("response body read failure"),
            "retain the following transport failure: {error}"
        );
    }
}

async fn stalled_stream() -> (
    String,
    tokio::sync::oneshot::Receiver<Value>,
    tokio::task::JoinHandle<()>,
) {
    stalled_provider_response(
        "200 OK",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    )
    .await
}

async fn stalled_provider_response(
    status: &str,
    body: &str,
) -> (
    String,
    tokio::sync::oneshot::Receiver<Value>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (started, reading) = tokio::sync::oneshot::channel();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
    );
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let body = request_body(&mut socket).await;
        socket.write_all(response.as_bytes()).await.unwrap();
        started.send(body).unwrap();
        let mut byte = [0u8; 1];
        match socket.read(&mut byte).await {
            Ok(0) => (),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) => {}
            other => panic!("dropping the client must close its active socket: {other:?}"),
        }
    });
    (endpoint, reading, server)
}

/// Grounds cancellation in the real socket: a barrier confirms the server has
/// begun a response before the client future is dropped.
#[tokio::test]
async fn dropping_local_stream_future_closes_the_server_socket() {
    let (endpoint, reading, server) = stalled_stream().await;
    let backend = LocalVllmBackend::new(endpoint, "stream-fixture")
        .with_retry_policy(RetryPolicy::immediate(0));
    let mut completion =
        Box::pin(backend.complete(ChatRequest::new().user("wait for cancellation")));
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = reading => request.expect("server began its unfinished response"),
            result = &mut completion => panic!("unfinished stream resolved before cancellation: {result:?}"),
        }
    }).await.expect("the first request must reach the mock server");
    drop(completion);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server must observe cancellation")
        .unwrap();
    assert_eq!(
        request["stream"], true,
        "cancellation must use the streaming protocol"
    );
    assert_eq!(request["stream_options"]["include_usage"], true);
}

/// Grounds the configurable idle timeout in the same real unfinished response.
/// An unbounded injected client must not erase the backend's explicit bound.
#[tokio::test]
async fn injected_client_cannot_remove_the_local_idle_timeout() {
    let (endpoint, reading, server) = stalled_stream().await;
    let backend = LocalVllmBackend::new(endpoint, "stream-fixture")
        .with_timeout(Duration::from_millis(100))
        .with_client(reqwest::Client::new())
        .with_retry_policy(RetryPolicy::immediate(0));
    let mut completion = Box::pin(backend.complete(ChatRequest::new().user("wait for timeout")));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = reading => request.expect("server began its unfinished response"),
            result = &mut completion => panic!("unfinished stream resolved before the response barrier: {result:?}"),
        }
    }).await.expect("the request must reach the mock server");
    let result = tokio::time::timeout(Duration::from_secs(5), &mut completion).await;
    drop(completion);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server must observe the deadline closing the socket")
        .unwrap();
    let error = result
        .expect("injecting a client must retain the configured deadline")
        .unwrap_err();
    assert!(error.to_string().contains("request failed"), "{error}");
}

/// Grounds error precedence when the server leaves its rejected response open:
/// the configured deadline must not erase already-received rejection evidence.
#[tokio::test]
async fn local_context_error_survives_the_following_body_timeout() {
    for (openai, body) in [
        (
            true,
            "data: {\"error\":{\"message\":\"Context size has been exceeded\"}}\n\n",
        ),
        (false, "{\"error\":\"Context size has been exceeded\"}\n"),
    ] {
        rejected_body_timeout(
            openai,
            "200 OK",
            body,
            newt_core::retry::Retryability::ContextExceeded,
            "Context size has been exceeded",
        )
        .await;
    }
}

#[tokio::test]
async fn local_authorization_error_survives_the_following_body_timeout() {
    for openai in [true, false] {
        rejected_body_timeout(
            openai,
            "401 Unauthorized",
            "fixture unauthorized",
            newt_core::retry::Retryability::Fatal,
            "fixture unauthorized",
        )
        .await;
    }
}

async fn rejected_body_timeout(
    openai: bool,
    status: &str,
    body: &str,
    expected: newt_core::retry::Retryability,
    diagnostic: &str,
) {
    let (endpoint, reading, server) = stalled_provider_response(status, body).await;
    let backend: Box<dyn InferenceBackend> = if openai {
        Box::new(
            LocalVllmBackend::new(endpoint, "timeout-fixture")
                .with_timeout(Duration::from_secs(2))
                .with_retry_policy(RetryPolicy::immediate(0)),
        )
    } else {
        Box::new(
            LocalOllamaBackend::new(endpoint, "timeout-fixture")
                .with_timeout(Duration::from_secs(2))
                .with_retry_policy(RetryPolicy::immediate(0)),
        )
    };
    let mut completion = Box::pin(backend.complete(ChatRequest::new().user("reject then stall")));
    // The server publishes its complete error frame and then holds the socket
    // open. Its EOF observation is the release barrier; no sleep races a close.
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = reading => request.expect("server began its rejected response"),
            result = &mut completion => panic!("response ended before the rejection barrier: {result:?}"),
        }
    }).await.expect("the request must reach the mock server");
    let result = tokio::time::timeout(Duration::from_secs(5), &mut completion).await;
    drop(completion);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("the deadline must close the client socket")
        .unwrap();
    let error = result
        .expect("the configured deadline must expire")
        .unwrap_err();
    assert_eq!(newt_core::retry::classify(&error), expected, "{error}");
    let detail = error.to_string();
    assert!(
        detail.contains(diagnostic),
        "retain observed server evidence: {detail}"
    );
    assert!(
        detail.contains("response body read failure"),
        "retain the following read failure: {detail}"
    );
}
