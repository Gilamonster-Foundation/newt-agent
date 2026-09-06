use super::*;

#[tokio::test]
async fn detect_openai_api_probe_carries_a_tool_and_no_legacy_max_tokens() {
    // The probe must look like a real agent request (tools present,
    // `max_completion_tokens` not the deprecated `max_tokens`) or
    // tools-require-responses models pass it and 400 on real turns.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "ok"}}]
        })))
        .mount(&server)
        .await;
    detect_openai_api(&reqwest::Client::new(), &server.uri(), "m", None)
        .await
        .unwrap();
    let reqs = server.received_requests().await.expect("journal");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(body["tools"].is_array() && !body["tools"].as_array().unwrap().is_empty());
    assert_eq!(body["max_completion_tokens"], serde_json::json!(1));
    assert!(body.get("max_tokens").is_none());
}

#[tokio::test]
async fn detect_openai_api_adopts_responses_on_gpt56_tools_rejection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "message": "Function tools with reasoning_effort are not supported for gpt-5.6-sol in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.",
                "type": "invalid_request_error",
                "param": "reasoning_effort",
            }
        })))
        .mount(&server)
        .await;
    let api = detect_openai_api(&reqwest::Client::new(), &server.uri(), "gpt-5.6-sol", None)
        .await
        .unwrap();
    assert_eq!(api, OpenAiApiSurface::Responses);
}

#[tokio::test]
async fn detect_openai_api_keeps_chat_when_completions_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "ok"}}]
        })))
        .mount(&server)
        .await;

    let api = detect_openai_api(&reqwest::Client::new(), &server.uri(), "m", None)
        .await
        .unwrap();
    assert_eq!(api, OpenAiApiSurface::ChatCompletions);
}

#[tokio::test]
async fn detect_openai_api_does_not_report_a_surface_after_auth_rejection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let error = detect_openai_api(&reqwest::Client::new(), &server.uri(), "m", None)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("authentication rejected"));
}

#[tokio::test]
async fn detect_openai_api_selects_responses_on_responses_only_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": {
                "message": "This model is only supported in v1/responses",
                "code": "unsupported_api"
            }
        })))
        .mount(&server)
        .await;

    let api = detect_openai_api(&reqwest::Client::new(), &server.uri(), "gpt-5-codex", None)
        .await
        .unwrap();
    assert_eq!(api, OpenAiApiSurface::Responses);
}

#[tokio::test]
async fn detect_openai_api_falls_through_to_responses_on_bare_chat_404() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": []
        })))
        .mount(&server)
        .await;

    let api = detect_openai_api(&reqwest::Client::new(), &server.uri(), "m", None)
        .await
        .unwrap();
    assert_eq!(api, OpenAiApiSurface::Responses);
}
