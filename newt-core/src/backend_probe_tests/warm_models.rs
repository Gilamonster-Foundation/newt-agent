use super::*;

#[tokio::test]
async fn router_load_and_unload_use_explicit_routes_and_require_success() {
    use wiremock::matchers::{body_json, header};
    let server = MockServer::start().await;
    for action in ["load", "unload"] {
        Mock::given(method("POST"))
            .and(path(format!("/models/{action}")))
            .and(header("authorization", "Bearer test-key"))
            .and(body_json(serde_json::json!({"model": "chosen"})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": true})),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let client = reqwest::Client::new();
    for loaded in [true, false] {
        set_llamacpp_model_loaded(&client, &server.uri(), Some("test-key"), "chosen", loaded)
            .await
            .unwrap();
    }
    Mock::given(method("POST"))
        .and(path("/models/load"))
        .and(body_json(serde_json::json!({"model": "refused"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"success": false})),
        )
        .mount(&server)
        .await;
    assert!(
        set_llamacpp_model_loaded(&client, &server.uri(), None, "refused", true)
            .await
            .is_err()
    );
    server.verify().await;
}

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
#[test]
fn router_states_keep_loaded_and_unloaded_distinct_from_unknown() {
    let states = parse_llamacpp_model_states(&serde_json::json!({"data": [
        {"id": "resident", "status": {"value": "loaded"}},
        {"id": "cold", "status": {"value": "unloaded"}},
        {"id": "starting", "status": {"value": "loading"}},
        {"id": "broken", "status": {"value": "unloaded", "failed": true}},
        {"id": "unknown"}
    ]}))
    .unwrap();
    assert_eq!(
        states,
        vec![
            ("resident".into(), "loaded".into()),
            ("cold".into(), "unloaded".into()),
            ("starting".into(), "loading".into()),
            ("broken".into(), "failed".into()),
            ("unknown".into(), "unknown".into()),
        ]
    );
    assert!(parse_llamacpp_model_states(&serde_json::json!({"data": [{"id":"hosted"}]})).is_none());
}

/// #2567: a router `/models` entry carries the argv it spawned the model with
/// and its preset block. The shape is the live router's (verified 2026-09-24);
/// the paths here are placeholders.
#[test]
fn launch_declaration_reads_args_and_preset_for_the_named_model() {
    let body = serde_json::json!({"data": [
        {"id": "other", "status": {"value": "unloaded", "args": ["llama-server", "--ctx-size", "8192"]}},
        {"id": "m-35b", "status": {
            "value": "loaded",
            "args": ["/opt/llama/llama-server", "--chat-template-kwargs", "{\"enable_thinking\": false}",
                     "--jinja", "--ctx-size", "131072", "--model", "/models/m.gguf"],
            "preset": "[m-35b]\nctx-size = 131072\n"
        }}
    ]});
    let launch = parse_llamacpp_launch(&body, "m-35b").expect("declared");
    assert_eq!(
        launch.args.first().map(String::as_str),
        Some("--chat-template-kwargs"),
        "binary dropped"
    );
    assert_eq!(launch.flag("--ctx-size"), Some("131072"));
    assert_eq!(
        launch.flag("--chat-template-kwargs"),
        Some("{\"enable_thinking\": false}")
    );
    assert_eq!(
        launch.flag("--jinja"),
        Some("--ctx-size"),
        "flag() is positional; callers ask for valued flags"
    );
    assert_eq!(
        launch.preset.as_deref(),
        Some("[m-35b]\nctx-size = 131072\n")
    );
    assert!(parse_llamacpp_launch(&body, "absent").is_none());
    assert!(
        parse_llamacpp_launch(&serde_json::json!({"data": [{"id": "bare"}]}), "bare").is_none()
    );
}

/// #2572 review: a launch probe's failure modes stay distinct, each against a
/// real HTTP exchange (wiremock), so the Inference view never calls a timeout
/// or a refused key "not a router".
#[tokio::test]
async fn launch_probe_distinguishes_absent_unsupported_refused_and_timed_out() {
    use std::time::Duration;
    let probe = |server: &MockServer, timeout: Duration| {
        let url = server.uri();
        async move {
            let client = reqwest::Client::builder().timeout(timeout).build().unwrap();
            LaunchProbe::from_result(fetch_llamacpp_launch(&client, &url, None, "m").await)
        }
    };
    let long = Duration::from_secs(5);
    let respond = |template: ResponseTemplate| async move {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(template)
            .mount(&server)
            .await;
        server
    };
    let declared = respond(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [
        {"id": "m", "status": {"value": "loaded", "args": ["llama-server", "--ctx-size", "8192"]}}
    ]})))
    .await;
    assert!(matches!(
        probe(&declared, long).await,
        LaunchProbe::Declared(_)
    ));
    let absent = respond(
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"data": [{"id": "m"}]})),
    )
    .await;
    assert_eq!(probe(&absent, long).await, LaunchProbe::Absent);
    let missing = respond(ResponseTemplate::new(404)).await;
    assert_eq!(probe(&missing, long).await, LaunchProbe::Unsupported);
    let not_json = respond(ResponseTemplate::new(200).set_body_string("<html>")).await;
    assert_eq!(probe(&not_json, long).await, LaunchProbe::Unsupported);
    let refused = respond(ResponseTemplate::new(401)).await;
    assert_eq!(probe(&refused, long).await, LaunchProbe::Refused(401));
    let broken = respond(ResponseTemplate::new(500)).await;
    assert_eq!(
        probe(&broken, long).await,
        LaunchProbe::Failed("HTTP 500".into())
    );
    let slow = respond(ResponseTemplate::new(200).set_delay(Duration::from_millis(500))).await;
    assert_eq!(
        probe(&slow, Duration::from_millis(50)).await,
        LaunchProbe::TimedOut
    );
}
