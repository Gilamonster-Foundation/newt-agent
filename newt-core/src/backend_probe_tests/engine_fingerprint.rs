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

/// **The wiring, which is the whole point.** `served_context_window` is a pure
/// parser; what makes it matter is that the ONE per-model window method every
/// production path already calls now reaches it.
///
/// Both live callers go through `BackendApi::context_window`: session-start
/// adopt (`newt-tui/src/lib.rs`, which sets `choice.context_window` from the
/// SELECTED model) and the cache-side probe (`newt-tui/src/probe.rs`
/// `fetch_context_window` → `ensure_context_window`). From there the discovered
/// window takes precedence over a tunings-file declaration and becomes
/// `safe_context`. Before this, llama.cpp got `None` from both and the window
/// stayed whatever a human typed.
///
/// `?model=` is load-bearing: this server answers only when asked about a
/// named model, exactly as a llama-swap router does.
#[tokio::test]
async fn openai_context_window_reads_a_llamacpp_served_window_per_model() {
    let server = MockServer::start().await;
    // No `max_model_len` anywhere — llama.cpp's `/v1/models` declares nothing,
    // which is why the window was previously unknowable.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "Ornith-1.5-35B-Q8_0"}, {"id": "qwen3-coder"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .and(query_param("model", "Ornith-1.5-35B-Q8_0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "default_generation_settings": {"n_ctx": 65536},
            "model_path": "/models/Ornith-1.5-35B-Q8_0.gguf",
        })))
        .mount(&server)
        .await;
    // The router's answer when asked about nothing in particular: not a window.
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "default_generation_settings": {"n_ctx": 0}, "model_path": "none"
        })))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    assert_eq!(
        api_for(BackendKind::Openai)
            .context_window(&client, &server.uri(), "Ornith-1.5-35B-Q8_0", None)
            .await,
        Some(65_536),
        "the served window must reach the method adopt and the cache probe call"
    );
    // Context is per-MODEL: a sibling on the same endpoint is a separate
    // question, and an unloaded answer is unknown rather than a zero ceiling.
    assert_eq!(
        api_for(BackendKind::Openai)
            .context_window(&client, &server.uri(), "qwen3-coder", None)
            .await,
        None,
        "one endpoint serves many models; a window is never endpoint-wide"
    );
}

/// vLLM's `max_model_len` still wins outright, and `/props` is never consulted
/// when it does — the new read is a fallback that fills a `None`, so no
/// endpoint that answered before can start answering differently.
#[tokio::test]
async fn a_declared_max_model_len_still_wins_without_a_props_fetch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "m", "max_model_len": 262_144}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "default_generation_settings": {"n_ctx": 4096}
        })))
        .expect(0)
        .mount(&server)
        .await;
    assert_eq!(
        api_for(BackendKind::Openai)
            .context_window(&reqwest::Client::new(), &server.uri(), "m", None)
            .await,
        Some(262_144)
    );
}

/// A server with no `/props` at all (a plain OpenAI proxy) stays `None` rather
/// than erroring — the whole path is fail-soft.
#[tokio::test]
async fn no_props_route_yields_no_window_not_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "m"}]
        })))
        .mount(&server)
        .await;
    assert_eq!(
        api_for(BackendKind::Openai)
            .context_window(&reqwest::Client::new(), &server.uri(), "m", None)
            .await,
        None
    );
}
