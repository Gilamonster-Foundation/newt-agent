use super::*;

// --- AnthropicApi ---

#[tokio::test]
async fn anthropic_list_models_sends_required_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", ANTHROPIC_VERSION))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5"},
                {"id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5"}
            ],
            "has_more": false
        })))
        .mount(&server)
        .await;

    let models = AnthropicApi
        .list_models(&reqwest::Client::new(), &server.uri(), Some("sk-ant-test"))
        .await
        .unwrap();
    assert_eq!(models, vec!["claude-sonnet-4-5", "claude-haiku-4-5"]);
}

#[tokio::test]
async fn anthropic_list_models_follows_pagination_capped() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(wiremock::matchers::query_param("after_id", "claude-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "claude-b"}],
            "has_more": false,
            "last_id": "claude-b"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "claude-a"}],
            "has_more": true,
            "last_id": "claude-a"
        })))
        .mount(&server)
        .await;

    let models = AnthropicApi
        .list_models(&reqwest::Client::new(), &server.uri(), Some("k"))
        .await
        .unwrap();
    assert_eq!(models, vec!["claude-a", "claude-b"]);
}

#[tokio::test]
async fn anthropic_context_window_reads_max_input_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "claude-sonnet-4-5", "max_input_tokens": 200_000},
                {"id": "claude-legacy"}
            ],
            "has_more": false
        })))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let window = AnthropicApi
        .context_window(&client, &server.uri(), "claude-sonnet-4-5", Some("k"))
        .await;
    assert_eq!(window, Some(200_000));
    // Absent field → None (caller keeps its default).
    let none = AnthropicApi
        .context_window(&client, &server.uri(), "claude-legacy", Some("k"))
        .await;
    assert_eq!(none, None);
}

#[tokio::test]
async fn anthropic_list_models_401_is_a_typed_probe_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = AnthropicApi
        .list_models(&reqwest::Client::new(), &server.uri(), Some("bad-key"))
        .await
        .unwrap_err();
    let failure = ProbeFailure::from_error(err);
    assert!(failure.is_auth_failure(), "401 classifies as auth");
}

#[test]
fn anthropic_serving_is_always_multiplexer() {
    assert_eq!(AnthropicApi.serving(1), Serving::Multiplexer);
    assert_eq!(AnthropicApi.serving(30), Serving::Multiplexer);
}
