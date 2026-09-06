use super::*;

#[tokio::test]
async fn detect_endpoint_prefers_ollama_when_both_protocols_answer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "qwen3:30b"}, {"name": "llama3.1:8b"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "openai-shim-model"}]
        })))
        .mount(&server)
        .await;

    let result = detect_endpoint(&reqwest::Client::new(), &format!("{}/", server.uri()), None)
        .await
        .unwrap();

    assert_eq!(result.endpoint, server.uri());
    assert_eq!(result.kind, BackendKind::Ollama);
    assert_eq!(result.models, vec!["qwen3:30b", "llama3.1:8b"]);
    assert_eq!(result.serving, Serving::Multiplexer);
}

#[tokio::test]
async fn detect_endpoint_prefers_the_nonempty_openai_surface() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "served-model"}]
        })))
        .mount(&server)
        .await;

    let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
        .await
        .unwrap();

    assert_eq!(result.kind, BackendKind::Openai);
    assert_eq!(result.models, vec!["served-model"]);
    assert_eq!(result.serving, Serving::Instance);
}

#[tokio::test]
async fn detect_endpoint_finds_authenticated_openai_instance() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "ornith-35b"}]
        })))
        .mount(&server)
        .await;

    let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), Some("secret-token"))
        .await
        .unwrap();

    assert_eq!(result.kind, BackendKind::Openai);
    assert_eq!(result.models, vec!["ornith-35b"]);
    assert_eq!(result.serving, Serving::Instance);
}

#[tokio::test]
async fn detect_endpoint_derives_openai_gateway_as_multiplexer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "model-a"}, {"id": "model-b"}]
        })))
        .mount(&server)
        .await;

    let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
        .await
        .unwrap();

    assert_eq!(result.kind, BackendKind::Openai);
    assert_eq!(result.models, vec!["model-a", "model-b"]);
    assert_eq!(result.serving, Serving::Multiplexer);
}

#[tokio::test]
async fn detect_endpoint_ignores_success_with_the_wrong_protocol_shape() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "ok"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "real-model"}]
        })))
        .mount(&server)
        .await;

    let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
        .await
        .unwrap();

    assert_eq!(result.kind, BackendKind::Openai);
    assert_eq!(result.models, vec!["real-model"]);
}

#[tokio::test]
async fn detect_endpoint_reports_authentication_required_without_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let error = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("authentication required"), "{error}");
    assert!(error.contains("401"), "{error}");
    assert!(error.contains("bearer token"), "{error}");
    assert!(error.contains(&server.uri()), "{error}");
}

#[tokio::test]
async fn detect_endpoint_reports_openai_auth_when_ollama_lists_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let error = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("authentication required"), "{error}");
}

#[tokio::test]
async fn detect_endpoint_sends_the_bearer_to_both_probes() {
    // The Ollama Cloud contract (deliberate inversion of the former
    // `detect_endpoint_never_sends_the_openai_token_to_ollama`):
    // https://ollama.com 401s `/api/tags` without a bearer, so the key —
    // sent only when the operator configured one — goes to BOTH probes.
    // /api/tags answers ONLY with the bearer; both surfaces answering
    // also proves the tie-break stays native-Ollama.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"name": "gpt-oss:120b"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "gpt-oss:120b"}]
        })))
        .mount(&server)
        .await;

    let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), Some("secret-token"))
        .await
        .unwrap();

    assert_eq!(result.kind, BackendKind::Ollama);
    assert_eq!(result.models, vec!["gpt-oss:120b"]);
    server.verify().await;
}

#[tokio::test]
async fn detect_endpoint_reports_ollama_auth_rejected_with_token() {
    // Ollama Cloud with a wrong token: /api/tags 401s while /v1/models
    // 404s — the error must name the bearer token, not claim the endpoint
    // is "unsupported".
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = detect_endpoint(&reqwest::Client::new(), &server.uri(), Some("wrong-token"))
        .await
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("authentication rejected by Ollama"),
        "auth-classified, got: {err}"
    );
    assert!(err.contains("check the bearer token"), "actionable: {err}");
}

#[tokio::test]
async fn detect_endpoint_reports_authentication_rejected_with_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer wrong-token"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let error = detect_endpoint(&reqwest::Client::new(), &server.uri(), Some("wrong-token"))
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("authentication rejected"), "{error}");
    assert!(error.contains("403"), "{error}");
    assert!(error.contains("check the bearer token"), "{error}");
}

#[tokio::test]
async fn detect_endpoint_reports_unsupported_http_service() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let error = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("unsupported inference endpoint"), "{error}");
    assert!(error.contains("/api/tags"), "{error}");
    assert!(error.contains("/v1/models"), "{error}");
    assert!(error.contains("HTTP 404"), "{error}");
}

#[tokio::test]
async fn detect_endpoint_reports_unreachable_when_probes_time_out() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(250))
                .set_body_json(serde_json::json!({"models": []})),
        )
        .mount(&server)
        .await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(20))
        .build()
        .unwrap();

    let error = detect_endpoint(&client, &server.uri(), None)
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("unreachable inference endpoint"), "{error}");
    assert!(error.contains(&server.uri()), "{error}");
}
// --- detect_endpoint: engine + warm population ---

#[tokio::test]
async fn detect_endpoint_populates_engine_and_warm() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "warm-model"}, {"id": "cold-model"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "default_generation_settings": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "warm-model", "state": "loaded"},
                {"id": "cold-model", "state": "unloaded"}
            ]
        })))
        .mount(&server)
        .await;

    let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
        .await
        .unwrap();

    assert_eq!(result.kind, BackendKind::Openai);
    assert_eq!(result.engine, Some(Engine::LlamaCpp));
    assert_eq!(result.warm, vec!["warm-model"]);
}

#[tokio::test]
async fn detect_endpoint_engine_and_warm_fail_soft() {
    // Fingerprints all 404 → engine None, warm empty, result still Ok.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "m1"}, {"id": "m2"}]
        })))
        .mount(&server)
        .await;

    let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
        .await
        .unwrap();

    assert_eq!(result.engine, None);
    assert!(result.warm.is_empty());
    assert_eq!(result.models, vec!["m1", "m2"]);
}
