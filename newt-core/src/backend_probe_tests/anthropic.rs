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

/// #2874 C1 §1.5 test 1: the parser reads one `/v1/models` entry.
#[test]
fn anthropic_entry_limits_parses_declared_max_tokens() {
    let limits = anthropic_entry_limits(&serde_json::json!({
        "id": "claude-sonnet-4-5",
        "max_input_tokens": 200_000,
        "max_tokens": 64_000
    }));
    assert_eq!(limits.input_window, Some(200_000));
    assert_eq!(limits.output_ceiling, Some(64_000));
}

/// #2874 C1 §1.5 test 1 (R1): `0`, `null`, and absent are UNKNOWN, not a
/// ceiling of zero — a non-positive declaration must never clamp requests.
#[test]
fn anthropic_entry_limits_treats_zero_null_and_missing_as_unknown() {
    for entry in [
        serde_json::json!({"id": "m", "max_tokens": 0}),
        serde_json::json!({"id": "m", "max_tokens": serde_json::Value::Null}),
        serde_json::json!({"id": "m"}),
    ] {
        let limits = anthropic_entry_limits(&entry);
        assert_eq!(
            limits.output_ceiling, None,
            "{entry} declares no usable ceiling"
        );
    }
}

/// #2874 C1 §1.5 test 1: a partially present `capabilities.effort` object
/// reads only the supported levels in increasing order; a missing level is
/// unsupported, not an error; an absent object is unknown (None).
#[test]
fn anthropic_entry_limits_reads_partial_effort_ladders() {
    let partial = anthropic_entry_limits(&serde_json::json!({
        "capabilities": {"effort": {
            "low": {"supported": true},
            "medium": {"supported": false},
            "high": {"supported": true},
            "xhigh": {"supported": false},
            "max": {"supported": false}
        }}
    }));
    assert_eq!(
        partial.effort_ladder,
        Some(vec![AnthropicEffort::Low, AnthropicEffort::High])
    );
    // Absent `capabilities` (or `effort`) → unknown, never an empty ladder.
    let absent = anthropic_entry_limits(&serde_json::json!({"id": "m"}));
    assert_eq!(absent.effort_ladder, None);
    let no_effort = anthropic_entry_limits(&serde_json::json!({"capabilities": {}}));
    assert_eq!(no_effort.effort_ladder, None);
    // Present but nothing supported is a real, EMPTY answer.
    let none_supported = anthropic_entry_limits(&serde_json::json!({
        "capabilities": {"effort": {"low": {"supported": false}}}
    }));
    assert_eq!(none_supported.effort_ladder, Some(vec![]));
    // The full ladder keeps probe order, `xhigh` and `max` included.
    let full = anthropic_entry_limits(&serde_json::json!({
        "capabilities": {"effort": {
            "low": {"supported": true},
            "medium": {"supported": true},
            "high": {"supported": true},
            "xhigh": {"supported": true},
            "max": {"supported": true}
        }}
    }));
    assert_eq!(
        full.effort_ladder,
        Some(vec![
            AnthropicEffort::Low,
            AnthropicEffort::Medium,
            AnthropicEffort::High,
            AnthropicEffort::Xhigh,
            AnthropicEffort::Max
        ])
    );
}

/// #2874 C1 §1.2: ONE round trip yields window AND ceiling, one shape.
#[tokio::test]
async fn anthropic_limits_reads_output_ceiling_from_v1_models() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "claude-sonnet-4-5", "max_input_tokens": 200_000, "max_tokens": 64_000},
                {"id": "claude-legacy"}
            ],
            "has_more": false
        })))
        .mount(&server)
        .await;

    let client = reqwest::Client::new();
    let limits = AnthropicApi
        .limits(&client, &server.uri(), "claude-sonnet-4-5", Some("k"))
        .await;
    assert_eq!(limits.input_window, Some(200_000));
    assert_eq!(limits.output_ceiling, Some(64_000));
    // A model the entry does not name declares nothing (fail-soft default).
    let unknown = AnthropicApi
        .limits(&client, &server.uri(), "claude-legacy", Some("k"))
        .await;
    assert_eq!(unknown, BackendLimits::default());
}

/// #2874 C1: a failing probe fails soft — unknown limits, never a guess.
#[tokio::test]
async fn anthropic_limits_fail_soft_when_the_probe_fails() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let limits = AnthropicApi
        .limits(
            &reqwest::Client::new(),
            &server.uri(),
            "claude-sonnet-4-5",
            None,
        )
        .await;
    assert_eq!(limits, BackendLimits::default());
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
