use super::*;

// --- warm models ---

#[test]
fn parse_ollama_ps_reads_names_sizes_and_expiry() {
    // Fixture ported from the retired newt-tui parse_loaded_models and
    // newt-cli extract_ps copies — this parser is now the ONE home.
    let json = serde_json::json!({
        "models": [
            {
                "name": "nemotron3:33b",
                "size": 35_000_000_000u64,
                "size_vram": 35_631_112_192u64,
                "expires_at": "2026-06-06T12:00:00Z"
            },
            {"name": "tiny:1b"},
            {"x": 1}
        ]
    });
    let ps = parse_ollama_ps(&json);
    assert_eq!(ps.len(), 2, "nameless entries skipped");
    assert_eq!(ps[0].name, "nemotron3:33b");
    assert_eq!(ps[0].size_bytes, Some(35_000_000_000));
    assert_eq!(ps[0].size_vram_bytes, Some(35_631_112_192));
    assert!(ps[0].expires_at.is_some());
    assert_eq!(ps[1].name, "tiny:1b");
    assert_eq!(ps[1].size_bytes, None);
    assert!(parse_ollama_ps(&serde_json::json!({"models": []})).is_empty());
    assert!(parse_ollama_ps(&serde_json::json!(null)).is_empty());
}

#[tokio::test]
async fn ollama_warm_models_reads_api_ps_with_bearer() {
    // The Ollama Cloud warmth contract: /api/ps carries the bearer when a
    // key is supplied.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "warm-a"}, {"name": "warm-b"}]
        })))
        .mount(&server)
        .await;
    let warm = OllamaApi
        .warm_models(&reqwest::Client::new(), &server.uri(), Some("tok"))
        .await;
    assert_eq!(warm, Some(vec!["warm-a".to_string(), "warm-b".to_string()]));
}

#[test]
fn parse_llamacpp_models_warm_filters_by_load_state() {
    let json = serde_json::json!({"data": [
        {"id": "cold-model", "state": "unloaded"},
        {"id": "warm-model", "state": "loaded"},
        {"id": "other-warm", "status": "LOADED"}
    ]});
    assert_eq!(
        parse_llamacpp_models_warm(&json),
        Some(vec!["warm-model".to_string(), "other-warm".to_string()])
    );
}

#[test]
fn parse_llamacpp_models_warm_none_when_no_state_fields() {
    // No entry carries a state field → capability absent (None), never a
    // guessed empty-warm claim.
    let json = serde_json::json!({"data": [{"id": "a"}, {"id": "b"}]});
    assert_eq!(parse_llamacpp_models_warm(&json), None);
}

#[test]
fn parse_llamacpp_models_warm_reads_object_shaped_status() {
    // The live dgx1 llama-swap router reports `status` as an OBJECT
    // (`{"value":"loaded", "args":[…], "preset":"…"}`), not a bare string.
    // Regression: this build's warm model must be detected so a Managed
    // Shared backend can adopt-warm on it. Would return None before the fix.
    let json = serde_json::json!({"data": [
        {"id": "ornith-1.0-35b-q8", "status": {"value": "loaded", "args": ["--x"]}},
        {"id": "ornith_35b", "status": {"value": "unloaded"}}
    ]});
    assert_eq!(
        parse_llamacpp_models_warm(&json),
        Some(vec!["ornith-1.0-35b-q8".to_string()])
    );
}

#[tokio::test]
async fn vllm_warm_models_is_the_served_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "resident-model"}]
        })))
        .mount(&server)
        .await;
    // /api/ps and /models must never be touched by the vLLM impl.
    Mock::given(method("GET"))
        .and(path("/api/ps"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let warm = VllmApi
        .warm_models(&reqwest::Client::new(), &server.uri(), None)
        .await;
    assert_eq!(warm, Some(vec!["resident-model".to_string()]));
    server.verify().await;
}
