/// Suite #2449: captured semantic intent reaches all four actual wire loops,
/// with and without SmartHarness. The auxiliary observes the core's pinned
/// value; omitted provider controls must not erase it or alter admission.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn cognition_capture_and_projection_agree_on_all_wires_with_smart_on_or_off() {
    use crate::role_profile::Cognition;
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    let _wire_env = super::super::anthropic_loop_tests::test_env(false);
    let _output_env = super::super::TestEnvGuard::unset("NEWT_ANTHROPIC_MAX_TOKENS");
    for wire in ["ollama", "chat", "anthropic", "responses"] {
        for smart in [false, true] {
            let server = MockServer::start().await;
            let body = match wire {
                "ollama" => {
                    serde_json::json!({"message":{"role":"assistant","content":"done"},"done":true})
                }
                "anthropic" => {
                    serde_json::json!({"id":"msg_1","type":"message","role":"assistant","model":"served","stop_reason":"end_turn","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":10,"output_tokens":5}})
                }
                "responses" => {
                    serde_json::json!({"id":"resp_1","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"done"}]}]})
                }
                _ => {
                    serde_json::json!({"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]})
                }
            };
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
            let workspace = tempfile::tempdir().unwrap();
            let kind = match wire {
                "ollama" => BackendKind::Ollama,
                "anthropic" => BackendKind::Anthropic,
                _ => BackendKind::Openai,
            };
            let mut config = TurnDriverConfig::new(
                server.uri(),
                "served",
                kind,
                workspace.path().to_string_lossy(),
            );
            config.openai_api = if wire == "responses" {
                crate::OpenAiApi::Responses
            } else {
                crate::OpenAiApi::ChatCompletions
            };
            // The ambient default agrees here; the sibling-switch fixture
            // separately proves that a mismatch cannot retarget this turn.
            std::env::set_var(
                "NEWT_OPENAI_API",
                if wire == "responses" {
                    "responses"
                } else {
                    "chat_completions"
                },
            );
            let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
            if smart {
                let observed = observed.clone();
                config.smart_harness = Some(Arc::new(
                    super::super::smart_harness::SmartHarness::new(
                        agent_harness::Session::new(Default::default()).unwrap(),
                        Arc::new(move |_| {
                            observed
                                .lock()
                                .unwrap()
                                .push(crate::cognition::effective_cognition());
                            Box::pin(async { Ok(("\"answer\"".to_string(), None)) })
                        }),
                        Default::default(),
                    )
                    .unwrap(),
                ));
            }
            let mut driver = TurnDriver::new(config).with_cognition(Some(Cognition::Meticulous));
            crate::cognition::set_cli_cognition(crate::cognition::CognitionOverride::Off);
            driver.submit("finish without tools").unwrap();
            let TurnStatus::Completed(outcome) = pump_to_done(&mut driver).await else {
                panic!("{wire}, smart={smart}: no outcome")
            };
            assert_eq!(outcome.error, None, "{wire}, smart={smart}");
            assert_eq!(outcome.semantic_cognition, Some(Cognition::Meticulous));
            assert_eq!(outcome.reply, "done");
            if smart {
                assert_eq!(*observed.lock().unwrap(), vec![Some(Cognition::Meticulous)]);
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1, "{wire}, smart={smart}");
            let request: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert!(request.get("chat_template_kwargs").is_none());
            assert!(request.get("reasoning_effort").is_none());
            match wire {
                "responses" => {
                    assert_eq!(requests[0].url.path(), "/v1/responses");
                    assert_eq!(request["reasoning"]["effort"], "high");
                    assert_eq!(
                        outcome.reasoning_effort,
                        Some(crate::model_card::ReasoningEffort::High)
                    );
                    assert_eq!(outcome.output_allowance.unwrap().tokens, 16_000);
                }
                "anthropic" => {
                    assert_eq!(requests[0].url.path(), "/v1/messages");
                    assert_eq!(request["max_tokens"], 8_192);
                    assert!(request.get("reasoning").is_none());
                    assert_eq!(outcome.output_allowance.unwrap().tokens, 8_192);
                }
                _ => {
                    assert!(request.get("reasoning").is_none());
                    assert!(request.get("max_tokens").is_none());
                    assert!(outcome.output_allowance.is_none());
                }
            }
        }
    }
}
