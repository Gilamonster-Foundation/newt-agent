use super::*;

// --- engine fingerprinting ---

#[test]
fn fingerprint_matches_shapes() {
    use FingerprintMarker::*;
    let props = serde_json::json!({"default_generation_settings": {}, "total_slots": 4});
    assert!(fingerprint_matches(
        &HasAnyKey(&["default_generation_settings", "model_path"]),
        &props
    ));
    let old_props = serde_json::json!({"model_path": "/models/x.gguf"});
    assert!(fingerprint_matches(
        &HasAnyKey(&["default_generation_settings", "model_path"]),
        &old_props
    ));
    let version = serde_json::json!({"version": "0.6.3"});
    assert!(fingerprint_matches(&HasKey("version"), &version));
    assert!(!fingerprint_matches(&HasKey("version"), &props));
    // llama-server /models: entries with load-state fields.
    let models = serde_json::json!({"data": [
        {"id": "a", "state": "loaded"},
        {"id": "b", "status": "unloaded"}
    ]});
    assert!(fingerprint_matches(&ModelsArrayWithState, &models));
    // OpenAI-shaped /models (no state fields) must NOT match.
    let openai = serde_json::json!({"data": [{"id": "a"}, {"id": "b"}]});
    assert!(!fingerprint_matches(&ModelsArrayWithState, &openai));
    // Empty array proves nothing.
    assert!(!fingerprint_matches(
        &ModelsArrayWithState,
        &serde_json::json!({"data": []})
    ));
}

#[tokio::test]
async fn detect_engine_identifies_llamacpp_via_props() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "default_generation_settings": {}, "total_slots": 1
        })))
        .mount(&server)
        .await;
    let engine = detect_engine(
        &reqwest::Client::new(),
        &server.uri(),
        BackendKind::Openai,
        None,
    )
    .await;
    assert_eq!(engine, Some(Engine::LlamaCpp));
}

#[tokio::test]
async fn detect_engine_identifies_vllm_via_version() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/version"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"version": "0.8.5.post1"})),
        )
        .mount(&server)
        .await;
    let engine = detect_engine(
        &reqwest::Client::new(),
        &server.uri(),
        BackendKind::Openai,
        None,
    )
    .await;
    assert_eq!(engine, Some(Engine::Vllm));
}

#[tokio::test]
async fn detect_engine_old_llamacpp_falls_back_to_models_route() {
    // Older llama.cpp builds lack /props — the non-/v1 /models route with
    // load states is the terminal fingerprint in the fallback chain.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/version"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "qwen3-32b", "state": "loaded"}]
        })))
        .mount(&server)
        .await;
    let engine = detect_engine(
        &reqwest::Client::new(),
        &server.uri(),
        BackendKind::Openai,
        None,
    )
    .await;
    assert_eq!(engine, Some(Engine::LlamaCpp));
}

#[tokio::test]
async fn detect_engine_unknown_for_generic_gateway() {
    // No fingerprint answers → None: the endpoint stays a fully usable
    // generic OpenAI backend, it merely reports no warmth.
    let server = MockServer::start().await;
    let engine = detect_engine(
        &reqwest::Client::new(),
        &server.uri(),
        BackendKind::Openai,
        None,
    )
    .await;
    assert_eq!(engine, None);
}

#[tokio::test]
async fn detect_engine_short_circuits_for_ollama_kind() {
    // kind=Ollama needs zero HTTP — the /api/tags race already proved it.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let engine = detect_engine(
        &reqwest::Client::new(),
        &server.uri(),
        BackendKind::Ollama,
        None,
    )
    .await;
    assert_eq!(engine, Some(Engine::Ollama));
    server.verify().await;
}
