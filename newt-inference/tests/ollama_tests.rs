use std::time::Duration;

use newt_inference::backend::{ChatReply, ChatRequest};
use newt_inference::local::LocalOllamaBackend;
use newt_inference::{InferenceBackend, RetryPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Zero-delay, 3-retry policy so retry tests exercise the loop (1 initial + 3
/// retries = 4 attempts) without sleeping through the production backoff.
fn test_retry() -> RetryPolicy {
    RetryPolicy::immediate(3)
}

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

    // 500 is retryable, so complete() will attempt 1 + 3 retries = 4 requests.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500).set_body_string("server error"))
        .expect(4)
        .mount(&server)
        .await;

    let backend =
        LocalOllamaBackend::new(server.uri(), "test-model").with_retry_policy(test_retry());
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

    // Timeout errors are retryable, so expect 4 total attempts.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "message": { "content": "slow" }
                }))
                .set_delay(Duration::from_secs(5)),
        )
        .expect(4)
        .mount(&server)
        .await;

    let backend = LocalOllamaBackend::new(server.uri(), "test-model")
        .with_timeout(Duration::from_millis(50))
        .with_retry_policy(test_retry());
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
    let backend = LocalOllamaBackend::discover_with_env("test-model", None, &candidates)
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
    let backend = LocalOllamaBackend::discover_with_env("test-model", None, &candidates)
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
    let result = LocalOllamaBackend::discover_with_env("test-model", None, &candidates).await;

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
    // OLLAMA_HOST is honored verbatim — no probe, no fallback.
    // We don't even need a wiremock here; the env-host wins
    // unconditionally and discover() never makes a network call.
    let other_server = MockServer::start().await;

    let candidates = vec![other_server.uri()];
    let backend = LocalOllamaBackend::discover_with_env(
        "my-model",
        Some("http://envhost.example:11434"),
        &candidates,
    )
    .await
    .unwrap();

    assert_eq!(backend.endpoint(), "http://envhost.example:11434");
    assert_eq!(backend.model_id(), "my-model");
}

#[tokio::test]
async fn env_host_used_verbatim_no_probe() {
    // Even if the env host is provably unreachable, discover() must
    // return it verbatim. User intent wins. Use discover_strict() to
    // get the old probe-then-fall-through behavior.
    let candidates = vec!["http://127.0.0.1:11434".to_string()];
    let backend = LocalOllamaBackend::discover_with_env(
        "verbatim-model",
        Some("http://nonexistent.invalid:9999"),
        &candidates,
    )
    .await
    .expect("discover() must succeed when env host is set (verbatim contract)");

    assert_eq!(backend.endpoint(), "http://nonexistent.invalid:9999");
}

#[tokio::test]
async fn discover_strict_errors_if_env_host_unreachable() {
    // discover_strict() probes every candidate (including env host)
    // and errors if none answer. This is the test-only variant — it's
    // what you want when you're asserting "this specific endpoint is
    // up", not "trust whatever OLLAMA_HOST says".
    let candidates = vec!["http://127.0.0.1:19997".to_string()];
    let result = LocalOllamaBackend::discover_strict_with_env(
        "strict-model",
        Some("http://nonexistent.invalid:9999"),
        &candidates,
    )
    .await;

    assert!(
        result.is_err(),
        "discover_strict should fail when nothing answers"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("no reachable") || err.contains("discover_strict"),
        "error should explain the failure mode: {err}"
    );
}

#[tokio::test]
async fn discover_strict_accepts_reachable_env_host() {
    // When the env host is up, discover_strict() returns it.
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": []
        })))
        .mount(&server)
        .await;

    let candidates = vec!["http://127.0.0.1:19996".to_string()];
    let backend = LocalOllamaBackend::discover_strict_with_env(
        "strict-model",
        Some(&server.uri()),
        &candidates,
    )
    .await
    .unwrap();

    assert_eq!(backend.endpoint(), server.uri());
}

// --- Retry tests (Step 3.3) ---

#[tokio::test]
async fn retries_on_503() {
    let server = MockServer::start().await;

    // First two calls return 503 (retryable), third returns 200.
    // wiremock matches the most recently mounted mock first, so mount the
    // success mock first (lower priority) then the 503 mock with up_to_n_times.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "recovered" }
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(503).set_body_string("temporarily unavailable"))
        .up_to_n_times(2)
        .mount(&server)
        .await;

    let backend =
        LocalOllamaBackend::new(server.uri(), "test-model").with_retry_policy(test_retry());
    let req = ChatRequest::new().user("hi");
    let reply = backend.complete(req).await.unwrap();

    assert_eq!(reply.content, "recovered");
}

#[tokio::test]
async fn retries_on_429() {
    // Regression: 429 Too Many Requests must be retryable (a hosted endpoint
    // returns it under load). It previously surfaced as a hard error.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "recovered" }
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
        .up_to_n_times(2)
        .mount(&server)
        .await;

    let backend =
        LocalOllamaBackend::new(server.uri(), "test-model").with_retry_policy(test_retry());
    let req = ChatRequest::new().user("hi");
    let reply = backend.complete(req).await.unwrap();

    assert_eq!(reply.content, "recovered");
}

#[tokio::test]
async fn gives_up_after_max_retries() {
    let server = MockServer::start().await;

    // Always return 503 — expect 4 total attempts (1 initial + 3 retries).
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(503).set_body_string("always down"))
        .expect(4)
        .mount(&server)
        .await;

    let backend =
        LocalOllamaBackend::new(server.uri(), "test-model").with_retry_policy(test_retry());
    let req = ChatRequest::new().user("hi");
    let err = backend.complete(req).await.unwrap_err();

    assert!(
        err.to_string().contains("503"),
        "final error should mention 503: {err}"
    );
}

#[tokio::test]
async fn non_retryable_4xx_fails_immediately() {
    let server = MockServer::start().await;

    // 400 is not retryable — should get exactly 1 request.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalOllamaBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let err = backend.complete(req).await.unwrap_err();

    assert!(
        err.to_string().contains("400"),
        "error should mention 400: {err}"
    );
}

#[tokio::test]
async fn success_on_first_try_no_retry() {
    let server = MockServer::start().await;

    // Exactly 1 request expected — no retries needed.
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "first try" }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalOllamaBackend::new(server.uri(), "test-model");
    let req = ChatRequest::new().user("hi");
    let reply = backend.complete(req).await.unwrap();

    assert_eq!(reply.content, "first try");
}
