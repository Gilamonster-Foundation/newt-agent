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

// --- served context window (#2248) ---

/// The number is in the body the fingerprint probe already fetches.
#[test]
fn reads_n_ctx_from_a_llamacpp_props_body() {
    // Shape observed on dgx1's llama.cpp, trimmed to the keys that matter.
    let props = serde_json::json!({
        "default_generation_settings": {"n_ctx": 65536, "n_predict": -1},
        "model_path": "/home/u/models/gguf/Ornith-1.5-35B-Q8_0.gguf",
        "build_info": "b623-511f9c1",
    });
    assert_eq!(served_context_window(&props), Some(65536));

    // Older builds report it at the top level.
    let flat = serde_json::json!({"model_path": "/m.gguf", "n_ctx": 32768});
    assert_eq!(served_context_window(&flat), Some(32768));
}

/// **Zero is not an answer.** A llama-swap style router with nothing loaded
/// reports `n_ctx: 0` / `model_path: "none"`. Reading that as a ceiling would
/// clamp every request to nothing — strictly worse than the over-large
/// declaration this feature exists to catch.
#[test]
fn an_unloaded_router_reports_nothing_not_zero() {
    let unloaded = serde_json::json!({
        "default_generation_settings": {"n_ctx": 0},
        "model_path": "none",
    });
    assert_eq!(
        served_context_window(&unloaded),
        None,
        "n_ctx 0 means 'ask again with ?model=<id>', never a real window"
    );
}

/// A body with no window at all (vLLM's `/version`, a generic OpenAI proxy)
/// yields `None`. Silence is not evidence of a small window.
#[test]
fn a_body_without_a_window_yields_none() {
    assert_eq!(
        served_context_window(&serde_json::json!({"version": "0.6.3"})),
        None
    );
    assert_eq!(served_context_window(&serde_json::json!({})), None);
    // Present but not a number.
    assert_eq!(
        served_context_window(&serde_json::json!({"n_ctx": "65536"})),
        None
    );
}

/// The reconciliation rule, which is the whole point: a declaration LARGER than
/// the served window is a misconfiguration and gets capped + flagged; a
/// declaration SMALLER is an operator tightening deliberately and is left alone.
#[test]
fn a_declaration_larger_than_the_served_window_is_capped_and_flagged() {
    // The failure this feature exists to prevent: declared 200k, served 64k.
    assert_eq!(
        reconcile_context_window(Some(200_000), Some(65_536)),
        (Some(65_536), true),
        "over-declaration must be capped AND reported"
    );
}

#[test]
fn a_smaller_declaration_is_respected_and_not_flagged() {
    // Deliberate tightening — leaving room for output, or testing under
    // pressure. Not a mistake, so no flag and no widening.
    assert_eq!(
        reconcile_context_window(Some(8_192), Some(65_536)),
        (Some(8_192), false)
    );
    // Equal is not "wrong high".
    assert_eq!(
        reconcile_context_window(Some(65_536), Some(65_536)),
        (Some(65_536), false)
    );
}

#[test]
fn a_missing_half_falls_back_without_flagging() {
    // No server signal: keep the declaration, say nothing.
    assert_eq!(
        reconcile_context_window(Some(200_000), None),
        (Some(200_000), false)
    );
    // No declaration: adopt what the server reports.
    assert_eq!(
        reconcile_context_window(None, Some(65_536)),
        (Some(65_536), false)
    );
    assert_eq!(reconcile_context_window(None, None), (None, false));
}
