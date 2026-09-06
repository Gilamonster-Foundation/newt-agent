use super::*;

#[tokio::test]
async fn generation_probe_requires_an_authenticated_valid_chat_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer secret-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "hi"}}]
        })))
        .mount(&server)
        .await;

    let result = verify_generation(
        &reqwest::Client::new(),
        BackendKind::Openai,
        Some(OpenAiApiSurface::ChatCompletions),
        &server.uri(),
        "selected-model",
        Some("secret-token"),
    )
    .await;

    assert_eq!(
        result,
        GenerationCheck::Accepted(Some(OpenAiApiSurface::ChatCompletions))
    );
    let requests = server.received_requests().await.expect("journal");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "selected-model");
    assert_eq!(body["messages"][0]["content"], "Reply with OK.");
    assert_eq!(body["max_tokens"], 8);
    assert!(body.get("max_completion_tokens").is_none());
    assert_eq!(body["stream"], false);
}

#[tokio::test]
async fn generation_probe_rejects_auth_and_malformed_success_envelopes() {
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&auth)
        .await;
    assert_eq!(
        verify_generation(
            &reqwest::Client::new(),
            BackendKind::Openai,
            Some(OpenAiApiSurface::ChatCompletions),
            &auth.uri(),
            "m",
            None,
        )
        .await,
        GenerationCheck::Rejected(403)
    );

    let malformed = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "chat.completion"
        })))
        .mount(&malformed)
        .await;
    assert!(matches!(
        verify_generation(
            &reqwest::Client::new(),
            BackendKind::Openai,
            Some(OpenAiApiSurface::ChatCompletions),
            &malformed.uri(),
            "m",
            None,
        )
        .await,
        GenerationCheck::Unverified(GenerationFailure::InvalidEnvelope)
    ));
}

#[tokio::test]
async fn generation_probe_negotiates_the_responses_surface_after_chat_404() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "hi"}]
            }]
        })))
        .mount(&server)
        .await;

    assert_eq!(
        verify_generation(
            &reqwest::Client::new(),
            BackendKind::Openai,
            None,
            &server.uri(),
            "responses-model",
            None,
        )
        .await,
        GenerationCheck::Accepted(Some(OpenAiApiSurface::Responses))
    );
    let requests = server.received_requests().await.expect("journal");
    let responses: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(responses["store"], false);
}

#[test]
fn incomplete_responses_probe_requires_clean_recognized_partial_output() {
    let partial = serde_json::json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": "partial"}]
        }]
    });
    assert_eq!(
        classify_responses_generation(
            reqwest::StatusCode::OK,
            &serde_json::to_vec(&partial).unwrap(),
        ),
        GenerationCheck::Accepted(Some(OpenAiApiSurface::Responses))
    );

    let mut with_error = partial.clone();
    with_error["error"] = serde_json::json!({"message": "provider failure"});
    assert!(matches!(
        classify_responses_generation(
            reqwest::StatusCode::OK,
            &serde_json::to_vec(&with_error).unwrap(),
        ),
        GenerationCheck::Unverified(GenerationFailure::InvalidResponsesPayload)
    ));

    let refusal = serde_json::json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [{
            "type": "message",
            "content": [{"type": "refusal", "refusal": "declined"}]
        }]
    });
    assert!(matches!(
        classify_responses_generation(
            reqwest::StatusCode::OK,
            &serde_json::to_vec(&refusal).unwrap(),
        ),
        GenerationCheck::Unverified(GenerationFailure::InvalidResponsesPayload)
    ));

    let unrecognized = serde_json::json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [{"type": "reasoning", "summary": []}]
    });
    assert!(matches!(
        classify_responses_generation(
            reqwest::StatusCode::OK,
            &serde_json::to_vec(&unrecognized).unwrap(),
        ),
        GenerationCheck::Unverified(GenerationFailure::InvalidResponsesPayload)
    ));
}

#[tokio::test]
async fn responses_probe_never_returns_provider_text_or_bearer_material() {
    const BEARER_SENTINEL: &str = "probe-secret-must-not-escape";
    const BODY_SENTINEL: &str = "provider-body-must-not-escape";
    let escape = char::from(27);
    let bell = char::from(7);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", format!("Bearer {BEARER_SENTINEL}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "completed",
            "error": {"message": format!(
                "{BEARER_SENTINEL} {BODY_SENTINEL} {escape}[31mred{bell}"
            )},
            "output": [{
                "type": "message",
                "content": [{
                    "type": "refusal",
                    "refusal": format!("{BODY_SENTINEL} {escape}[2J")
                }]
            }]
        })))
        .mount(&server)
        .await;

    let result = verify_generation(
        &reqwest::Client::new(),
        BackendKind::Openai,
        Some(OpenAiApiSurface::Responses),
        &server.uri(),
        "model",
        Some(BEARER_SENTINEL),
    )
    .await;
    let GenerationCheck::Unverified(reason) = result else {
        panic!("provider error/refusal must fail closed: {result:?}");
    };
    let rendered = reason.to_string();

    assert_eq!(reason, GenerationFailure::InvalidResponsesPayload);
    assert!(!rendered.contains(BEARER_SENTINEL));
    assert!(!rendered.contains(BODY_SENTINEL));
    assert!(!rendered.contains('\u{1b}'));
    assert!(!rendered.chars().any(char::is_control));
}
