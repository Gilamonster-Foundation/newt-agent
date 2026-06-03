//! Tests for bearer-token authentication on the OpenAI-compatible
//! (`LocalVllmBackend`) backend.
//!
//! Local vLLM/llama.cpp servers are unauthenticated, but hosted
//! OpenAI-compatible endpoints require `Authorization: Bearer <token>`.
//! These tests assert the header is sent when (and only when) a key is
//! configured, and that `from_config` wires the key in from a
//! `BackendConfig`.

use newt_core::router::Tier;
use newt_core::{BackendConfig, BackendKind};
use newt_inference::backend::ChatRequest;
use newt_inference::local::LocalVllmBackend;
use newt_inference::InferenceBackend;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn chat_ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "choices": [{ "message": { "role": "assistant", "content": "hi" } }],
        "model": "m"
    }))
}

#[tokio::test]
async fn complete_sends_bearer_token_when_api_key_set() {
    let server = MockServer::start().await;
    // The mock only matches if the Authorization header is exactly right;
    // a missing/incorrect header falls through to a 404 and fails the call.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer secret-abc"))
        .respond_with(chat_ok())
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "m").with_api_key("secret-abc".to_string());
    let reply = backend
        .complete(ChatRequest::new().user("hi"))
        .await
        .unwrap();
    assert_eq!(reply.content, "hi");
}

#[tokio::test]
async fn complete_omits_authorization_when_unauthenticated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(chat_ok())
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "m");
    backend
        .complete(ChatRequest::new().user("hi"))
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].headers.get("authorization").is_none(),
        "unauthenticated backend must not send an Authorization header"
    );
}

#[tokio::test]
async fn empty_api_key_is_treated_as_unauthenticated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(chat_ok())
        .mount(&server)
        .await;

    // An empty token (e.g. an empty key file) must not produce a
    // `Bearer ` header — it would be a confusing 401 on the wire.
    let backend = LocalVllmBackend::new(server.uri(), "m").with_api_key(Some(String::new()));
    backend
        .complete(ChatRequest::new().user("hi"))
        .await
        .unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert!(reqs[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn list_models_sends_bearer_token_when_api_key_set() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer key-123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{ "id": "m", "object": "model" }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let backend = LocalVllmBackend::new(server.uri(), "m").with_api_key("key-123".to_string());
    let models = backend.list_models().await.unwrap();
    assert_eq!(models.len(), 1);
}

#[tokio::test]
async fn from_config_wires_bearer_auth_from_env() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer env-token-9"))
        .respond_with(chat_ok())
        .expect(1)
        .mount(&server)
        .await;

    let var = "NEWT_TEST_OPENAI_FROM_CONFIG_KEY";
    std::env::set_var(var, "env-token-9");
    let cfg = BackendConfig {
        name: "remote".into(),
        endpoint: server.uri(),
        model: "m".into(),
        tiers: vec![Tier::Fast],
        kind: BackendKind::Openai,
        api_key_file: None,
        api_key_env: Some(var.into()),
    };
    let backend = LocalVllmBackend::from_config(&cfg);
    std::env::remove_var(var);

    let reply = backend
        .complete(ChatRequest::new().user("hi"))
        .await
        .unwrap();
    assert_eq!(reply.content, "hi");
}
