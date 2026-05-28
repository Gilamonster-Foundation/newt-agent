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

// --- Discovery tests (Step 3.2) ---

#[tokio::test]
async fn discover_first_wins() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": []
        })))
        .mount(&server1)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": []
        })))
        .mount(&server2)
        .await;

    let candidates = vec![server1.uri(), server2.uri()];
    let backend = LocalOllamaBackend::discover_with_candidates("test-model", &candidates)
        .await
        .unwrap();

    assert_eq!(backend.model_id(), "test-model");
    // The first reachable candidate should win.
    assert_eq!(backend.endpoint(), server1.uri());
}

#[tokio::test]
async fn discover_fallthrough_on_failure() {
    let server1 = MockServer::start().await;
    let server2 = MockServer::start().await;

    // server1 returns 500 for /api/tags — probe treats non-2xx as unreachable.
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server1)
        .await;

    // server2 returns 200.
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": []
        })))
        .mount(&server2)
        .await;

    let candidates = vec![server1.uri(), server2.uri()];
    let backend = LocalOllamaBackend::discover_with_candidates("test-model", &candidates)
        .await
        .unwrap();

    // server1 failed probe, so server2 should be chosen.
    assert_eq!(backend.endpoint(), server2.uri());
}

#[tokio::test]
async fn discover_all_down_returns_error() {
    // Use endpoints that are guaranteed not to be listening.
    let candidates = vec![
        "http://127.0.0.1:19999".to_string(),
        "http://127.0.0.1:19998".to_string(),
    ];
    let result = LocalOllamaBackend::discover_with_candidates("test-model", &candidates).await;

    assert!(
        result.is_err(),
        "discover should fail when no endpoints are reachable"
    );
    assert!(
        result.unwrap_err().to_string().contains("no reachable"),
        "error message should mention 'no reachable'"
    );
}

#[tokio::test]
async fn env_var_override() {
    let server = MockServer::start().await;
    let other_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": []
        })))
        .mount(&other_server)
        .await;

    // SAFETY: test binary is single-threaded for this test.
    unsafe { std::env::set_var("OLLAMA_HOST", server.uri()) };

    // Even though other_server is in the candidate list, OLLAMA_HOST wins.
    let candidates = vec![other_server.uri()];
    let backend = LocalOllamaBackend::discover_with_candidates("my-model", &candidates)
        .await
        .unwrap();

    unsafe { std::env::remove_var("OLLAMA_HOST") };

    assert_eq!(backend.endpoint(), server.uri());
    assert_eq!(backend.model_id(), "my-model");
}
