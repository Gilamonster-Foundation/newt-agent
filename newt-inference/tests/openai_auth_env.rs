//! `from_config` wires a bearer token in from the environment variable the
//! config names (`api_key_env`).
//!
//! This test changes the process environment, so it is its own test
//! executable rather than a module of `it`. C code reads the environment
//! without Rust's lock (the system resolver does, on a thread of its own),
//! and changing it while another thread does that is undefined behavior; a
//! separate process has no such thread. The other bearer-token tests are in
//! `it/openai_auth_tests.rs`.

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
        model: Some("m".into()),
        model_path: None,
        tiers: vec![Tier::Fast],
        kind: Some(BackendKind::Openai),
        api: Default::default(),
        api_key_file: None,
        api_key_env: Some(var.into()),
        ..Default::default()
    };
    let backend = LocalVllmBackend::from_config(&cfg);
    std::env::remove_var(var);

    let reply = backend
        .complete(ChatRequest::new().user("hi"))
        .await
        .unwrap();
    assert_eq!(reply.content, "hi");
}
