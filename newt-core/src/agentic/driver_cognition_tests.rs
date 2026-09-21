/// Suite #2449: a nested core reader must see the driver's captured semantic
/// intent even on Ollama, which has no cognition wire projection. The real
/// HTTP/tool dispatch grounds the pure capture tests; no technique is faked.
#[tokio::test]
async fn cognition_capture_reaches_nested_core_reader_without_wire_controls() {
    use crate::cognition::{set_cli_cognition, CognitionOverride};
    use crate::role_profile::Cognition;

    struct ReadCognition(Arc<std::sync::Mutex<Vec<Option<Cognition>>>>);
    #[async_trait::async_trait]
    impl CrewRunner for ReadCognition {
        async fn dispatch(
            &self,
            _op: &str,
            _args: &serde_json::Value,
            _caveats: &Caveats,
        ) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .push(crate::cognition::effective_cognition());
            Ok("captured cognition observed".into())
        }
    }

    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    let server = MockServer::start().await;
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ToolCallingOllama {
            name: "crew",
            arguments: serde_json::json!({"task": "inspect captured posture"}),
            bodies: bodies.clone(),
        })
        .mount(&server)
        .await;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    set_cli_cognition(CognitionOverride::Set(Cognition::Meticulous));
    let mut driver =
        TurnDriver::new(cfg(&server.uri())).with_crew_runner(Arc::new(ReadCognition(seen.clone())));
    set_cli_cognition(CognitionOverride::Off);
    driver.submit("inspect captured posture").unwrap();
    let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
        panic!("turn did not finish");
    };
    assert_eq!(outcome.error, None);
    assert_eq!(*seen.lock().unwrap(), vec![Some(Cognition::Meticulous)]);
    assert!(
        outcome.output_allowance.is_none(),
        "semantic intent must not add Ollama reservation"
    );
    for body in bodies.lock().unwrap().iter() {
        for key in [
            "reasoning",
            "reasoning_effort",
            "think",
            "chat_template_kwargs",
            "max_tokens",
        ] {
            assert!(
                body.get(key).is_none(),
                "unsupported cognition field {key}: {body}"
            );
        }
        assert!(body["options"].get("num_predict").is_none());
    }
    assert_eq!(
        crate::cognition::effective_cognition(),
        None,
        "worker capture must not leak"
    );
}

/// Suite #2449: a sibling wire switch must not pair captured Responses policy
/// with Chat dispatch. The mock also changes the ambient API between rounds.
#[tokio::test]
async fn captured_responses_wire_survives_sibling_api_switch() {
    use crate::role_profile::Cognition;
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    let server = MockServer::start().await;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("note.txt"), "source evidence").unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(|request: &Request| {
            // The test holds GlobalSettingsGuard across the worker; use the
            // raw setter here rather than taking its lock on another thread.
            std::env::set_var("NEWT_OPENAI_API", "chat_completions");
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let read = body["input"].as_array().unwrap().iter()
                .any(|item| item["type"] == "function_call_output");
            let output = if read {
                serde_json::json!([{"type":"message", "content":[{"type":"output_text", "text":"read complete"}]}])
            } else {
                serde_json::json!([{"type":"function_call", "id":"fc_read", "call_id":"read", "name":"read_file", "arguments":"{\"path\":\"note.txt\"}"}])
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"output":output}))
        })
        .mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices":[{"message":{"role":"assistant","content":"wrong wire"},"finish_reason":"stop"}]
        })))
        .mount(&server).await;
    let mut config = TurnDriverConfig::new(
        server.uri(),
        "served",
        BackendKind::Openai,
        workspace.path().to_string_lossy(),
    );
    config.openai_api = crate::OpenAiApi::Responses;
    config.responses_capability =
        serde_json::from_value(serde_json::json!({"reasoning_effort":["low","medium","high"]}))
            .unwrap();
    config.num_ctx = Some(32_768);
    config.output_allowance = Some(3_000);
    let mut driver = TurnDriver::new(config).with_cognition(Some(Cognition::Meticulous));
    std::env::set_var("NEWT_OPENAI_API", "chat_completions");
    driver.submit("read note.txt and finish").unwrap();
    let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
        panic!("turn did not finish");
    };
    assert_eq!(outcome.error, None);
    assert_eq!(
        outcome.reply, "read complete",
        "the captured API owns dispatch"
    );
    assert_eq!(outcome.semantic_cognition, Some(Cognition::Meticulous));
    assert_eq!(
        outcome.reasoning_effort,
        Some(crate::model_card::ReasoningEffort::High)
    );
    assert_eq!(outcome.output_allowance.unwrap().tokens, 3_000);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(request.url.path(), "/v1/responses");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["store"], false);
        assert!(body.get("max_output_tokens").is_none());
    }
}

/// Suite #2449: advertised maxima constrain actual dispatch, while absence
/// preserves each of the four existing fixed effort values and reservations.
#[tokio::test]
async fn responses_advertised_effort_accepts_or_refuses_before_dispatch() {
    use crate::role_profile::Cognition;
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    // Keep the constructor fallback deterministic; typed config owns dispatch.
    std::env::set_var("NEWT_OPENAI_API", "responses");
    for (cognition, allowance) in [
        (Cognition::Zen, 2_048),
        (Cognition::Rational, 4_096),
        (Cognition::Thoughtful, 10_000),
        (Cognition::Meticulous, 16_000),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}]
            })))
            .mount(&server)
            .await;
        let workspace = tempfile::tempdir().unwrap();
        let mut config = TurnDriverConfig::new(
            server.uri(),
            "served",
            BackendKind::Openai,
            workspace.path().to_string_lossy(),
        );
        config.openai_api = crate::OpenAiApi::Responses;
        let mut driver = TurnDriver::new(config.clone()).with_cognition(Some(cognition));
        driver.submit("finish").unwrap();
        let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
            panic!("turn did not finish")
        };
        assert_eq!(outcome.error, None);
        assert_eq!(
            outcome.reasoning_effort.unwrap().as_str(),
            cognition.reasoning_effort()
        );
        assert_eq!(outcome.output_allowance.unwrap().tokens, allowance);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        config.responses_capability =
            serde_json::from_value(serde_json::json!({"reasoning_effort":["minimal","low"]}))
                .unwrap();
        let mut driver = TurnDriver::new(config).with_cognition(Some(cognition));
        driver.submit("finish").unwrap();
        let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
            panic!("turn did not return an outcome")
        };
        if matches!(cognition, Cognition::Zen | Cognition::Rational) {
            assert_eq!(outcome.error, None);
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
            assert_eq!(
                outcome.reasoning_effort.unwrap().as_str(),
                cognition.reasoning_effort()
            );
        } else {
            assert!(outcome
                .error
                .unwrap()
                .contains("not advertised by this Responses endpoint"));
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                1,
                "unsupported request must not be sent"
            );
            assert!(outcome.reasoning_effort.is_none());
            assert!(
                outcome.responses_capability.is_none(),
                "no admitted policy on failure"
            );
        }
    }
}
