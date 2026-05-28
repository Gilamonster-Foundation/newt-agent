use std::time::Duration;

use newt_inference::backend::{ChatReply, ChatRequest};
use newt_inference::local::LocalOllamaBackend;
use newt_inference::InferenceBackend;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn happy_path() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "hello" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalOllamaBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let reply: ChatReply = backend.complete(req).await.unwrap();

    assert_eq!(reply.content, "hello");
}

#[tokio::test]
async fn non_200_returns_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalOllamaBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let err = backend.complete(req).await.unwrap_err();

    assert!(
        err.to_string().contains("500"),
        "error should mention status code: {err}"
    );
}

#[tokio::test]
async fn malformed_json_returns_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("not json")
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalOllamaBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let result = backend.complete(req).await;

    assert!(result.is_err(), "malformed JSON should produce an error");
}

#[tokio::test]
async fn empty_content_returns_empty_string() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalOllamaBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let reply = backend.complete(req).await.unwrap();

    assert_eq!(reply.content, "");
}

#[tokio::test]
async fn model_id_correctly_returned() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "ok" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalOllamaBackend::new(server.uri(), "llama3.1:8b");
    let req = ChatRequest::new().user("hi");
    let reply = backend.complete(req).await.unwrap();

    assert_eq!(reply.model_id, "llama3.1:8b");
}

#[tokio::test]
async fn timeout_returns_error() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "message": { "content": "slow" }
                }))
                .set_delay(Duration::from_secs(5)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let backend =
        LocalOllamaBackend::new(server.uri(), "test-model").with_timeout(Duration::from_millis(50));
    let req = ChatRequest::new().user("hi");
    let err = backend.complete(req).await.unwrap_err();

    assert!(
        err.to_string().contains("request failed")
            || err.to_string().contains("timed out")
            || err.to_string().contains("timeout"),
        "error should indicate a timeout: {err}"
    );
}
