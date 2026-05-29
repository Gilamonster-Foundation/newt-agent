//! Integration tests for `LocalVllmBackend`.
//!
//! vLLM exposes the OpenAI-compatible HTTP API. These tests mirror the
//! Ollama suite structurally (`ollama_tests.rs`), but exercise the
//! OpenAI-flavoured request/response shape (`/v1/chat/completions`,
//! `/v1/models`) instead.

use std::time::Duration;

use newt_inference::backend::{ChatReply, ChatRequest};
use newt_inference::local::LocalVllmBackend;
use newt_inference::InferenceBackend;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// --- complete() ---

#[tokio::test]
async fn complete_happy_path() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [
                { "message": { "role": "assistant", "content": "hello" } }
            ],
            "model": "x"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "x");
    let req = ChatRequest::new().user("hi");
    let reply: ChatReply = backend.complete(req).await.unwrap();

    assert_eq!(reply.content, "hello");
    assert_eq!(reply.model_id, "x");
}

#[tokio::test]
async fn complete_model_id_echoed_from_server() {
    // The server echoes back a different model id than what we configured
    // (e.g. an alias resolved to its canonical name) — we should surface
    // the server's value so callers can audit it.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [
                { "message": { "role": "assistant", "content": "ok" } }
            ],
            "model": "llama3.1:8b-instruct-q4"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "llama3.1:8b");
    let req = ChatRequest::new().user("hi");
    let reply = backend.complete(req).await.unwrap();

    assert_eq!(reply.model_id, "llama3.1:8b-instruct-q4");
}

#[tokio::test]
async fn complete_4xx_fails_immediately() {
    let server = MockServer::start().await;

    // 400 is NOT retryable — exactly one request must hit the server.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let err = backend.complete(req).await.unwrap_err();

    assert!(
        err.to_string().contains("400"),
        "error should mention 400: {err}"
    );
}

#[tokio::test]
async fn complete_non_200_returns_error() {
    let server = MockServer::start().await;

    // 500 is retryable, so complete() will attempt 1 + 3 retries = 4 requests.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
        .expect(4)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let err = backend.complete(req).await.unwrap_err();

    assert!(
        err.to_string().contains("500"),
        "error should mention status code: {err}"
    );
}

#[tokio::test]
async fn complete_malformed_json_returns_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("not json")
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let result = backend.complete(req).await;

    assert!(result.is_err(), "malformed JSON should produce an error");
}

#[tokio::test]
async fn complete_empty_choices_returns_empty_content() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [],
            "model": "x"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let reply = backend.complete(req).await.unwrap();

    assert_eq!(reply.content, "");
}

#[tokio::test]
async fn complete_retries_on_503() {
    let server = MockServer::start().await;

    // The success mock is mounted FIRST (lower priority); the 503 mock is
    // capped to two responses via up_to_n_times so the third attempt hits
    // the success path. Same pattern as the Ollama suite.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{ "message": { "content": "recovered" } }],
            "model": "x"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("temporarily unavailable"))
        .up_to_n_times(2)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let reply = backend.complete(req).await.unwrap();

    assert_eq!(reply.content, "recovered");
}

#[tokio::test]
async fn complete_gives_up_after_max_retries() {
    let server = MockServer::start().await;

    // Always return 503 — expect 4 total attempts (1 initial + 3 retries).
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("always down"))
        .expect(4)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let err = backend.complete(req).await.unwrap_err();

    assert!(
        err.to_string().contains("503"),
        "final error should mention 503: {err}"
    );
}

#[tokio::test]
async fn complete_timeout() {
    let server = MockServer::start().await;

    // Timeout errors are retryable, so expect 4 total attempts.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "slow" } }],
                    "model": "x"
                }))
                .set_delay(Duration::from_secs(5)),
        )
        .expect(4)
        .mount(&server)
        .await;

    let backend =
        LocalVllmBackend::new(server.uri(), "test-model").with_timeout(Duration::from_millis(50));
    let req = ChatRequest::new().user("hi");
    let err = backend.complete(req).await.unwrap_err();

    assert!(
        err.to_string().contains("request failed")
            || err.to_string().contains("timed out")
            || err.to_string().contains("timeout"),
        "error should indicate a timeout: {err}"
    );
}

// --- InferenceBackend trait surface ---

#[tokio::test]
async fn name_and_model_id() {
    let backend = LocalVllmBackend::new("http://127.0.0.1:0", "llama3.1:70b");
    assert_eq!(backend.name(), "vllm-local");
    assert_eq!(backend.model_id(), "llama3.1:70b");
    assert_eq!(backend.endpoint(), "http://127.0.0.1:0");
}
