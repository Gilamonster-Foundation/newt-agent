//! #2824: endpoint continuation capability cannot override the hard output stop.
use super::*;

#[tokio::test]
async fn reasoning_length_stops_even_when_the_endpoint_can_continue() {
    for inline in [false, true] {
        for advertised in [false, true] {
            let server = MockServer::start().await;
            let message = if inline {
                serde_json::json!({"content":"<think>private unfinished plan"})
            } else {
                serde_json::json!({"content":null,"reasoning_content":"private unfinished plan"})
            };
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices":[{"finish_reason":"length","message":message}],
                    "usage":{"prompt_tokens":20,"completion_tokens":8}
                })))
                .mount(&server)
                .await;
            let messages = msgs();
            let caveats = Caveats::top();
            let uri = server.uri();
            let mut c = ctx(&uri, &messages, &caveats);
            c.kind = BackendKind::Openai;
            c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
            c.chat_completions_capability.bounded_reasoning_continuation = Some(advertised);
            let (reply, _, usage, _) = chat_complete(c, &mut NoMcp).await.unwrap();
            assert!(
                reply.contains("output limit") && reply.contains("operator"),
                "{reply}"
            );
            assert!(!reply.contains("private unfinished plan"));
            assert_eq!(usage.unwrap().output_tokens, 8);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn openai_reasoning_only_stop_is_not_misclassified_as_overflow() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "private reasoning with a normal stop"
                }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    c.chat_completions_capability.bounded_reasoning_continuation = Some(true);
    c.solve_obs = Some(&mut observation);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("ordinary stop remains a terminal empty response");

    assert!(reply.contains("empty response"));
    assert!(!reply.contains("private reasoning"));
    assert!(observation.behavior_signals.iter().all(|signal| !matches!(
        signal,
        crate::agentic::observability::BehaviorSignal::ReasoningOverflow { .. }
    )));
}
