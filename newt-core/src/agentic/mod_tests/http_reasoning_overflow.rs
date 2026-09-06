use super::*;

struct OpenAiReasoningOverflowResponder {
    round: AtomicUsize,
    second_request: Arc<Mutex<Option<serde_json::Value>>>,
    overflow_twice: bool,
    inline_reasoning: bool,
}

impl Respond for OpenAiReasoningOverflowResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            return sse_replay("completed after bounded continuation");
        }
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        if round > 0 {
            *self.second_request.lock().expect("capture lock") = Some(body_json(req));
        }
        if round == 0 || self.overflow_twice {
            let message = if self.inline_reasoning {
                serde_json::json!({
                    "role": "assistant",
                    "content": format!("<think>unfinished inline plan {round}")
                })
            } else {
                serde_json::json!({
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": format!("unfinished plan {round}")
                })
            };
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "finish_reason": "length",
                    "message": message
                }],
                "usage": {"prompt_tokens": 20, "completion_tokens": 8}
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "completed after bounded continuation"
                }
            }],
            "usage": {"prompt_tokens": 24, "completion_tokens": 5}
        }))
    }
}

#[tokio::test]
async fn openai_reasoning_overflow_continues_once_with_the_current_plan() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    let responder = OpenAiReasoningOverflowResponder {
        round: AtomicUsize::new(0),
        second_request: second_request.clone(),
        overflow_twice: false,
        inline_reasoning: false,
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(responder)
        // Two model rounds + the #123 streaming re-issue of the second one.
        .expect(3)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    c.chat_completions_capability.bounded_reasoning_continuation = Some(true);
    c.solve_obs = Some(&mut observation);

    let (reply, _, usage, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("bounded continuation succeeds");

    assert_eq!(reply, "completed after bounded continuation");
    assert_eq!(usage.expect("usage accumulated").output_tokens, 13);
    let request = second_request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("second request captured");
    let replayed = request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("partial assistant message replayed");
    assert_eq!(replayed["reasoning_content"], "unfinished plan 0");
    assert!(!reply.contains("unfinished plan"));
    assert!(observation.behavior_signals.iter().any(|signal| matches!(
        signal,
        crate::agentic::observability::BehaviorSignal::ReasoningOverflow {
            continuation_attempted: true,
            continuation_succeeded: true,
            ..
        }
    )));
    assert_eq!(
        observation
            .behavior_signals
            .iter()
            .filter_map(|signal| match signal {
                crate::agentic::observability::BehaviorSignal::ChatCompletionFinish {
                    finish_reason,
                    ..
                } => finish_reason.as_deref(),
                _ => None,
            })
            .collect::<Vec<_>>(),
        ["length", "stop"]
    );
}

#[tokio::test]
async fn openai_reasoning_overflow_stops_after_one_failed_continuation() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningOverflowResponder {
            round: AtomicUsize::new(0),
            second_request,
            overflow_twice: true,
            inline_reasoning: false,
        })
        .expect(2)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    c.chat_completions_capability.bounded_reasoning_continuation = Some(true);
    c.solve_obs = Some(&mut observation);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("second overflow is classified, not retried forever");

    assert!(
        reply.contains("empty response"),
        "honest terminal result: {reply}"
    );
    assert!(observation.behavior_signals.iter().any(|signal| matches!(
        signal,
        crate::agentic::observability::BehaviorSignal::ReasoningOverflow {
            continuation_attempted: true,
            continuation_succeeded: false,
            ..
        }
    )));
}

#[tokio::test]
async fn openai_inline_reasoning_overflow_uses_the_same_bounded_continuation() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningOverflowResponder {
            round: AtomicUsize::new(0),
            second_request: second_request.clone(),
            overflow_twice: false,
            inline_reasoning: true,
        })
        // Two model rounds + the #123 streaming re-issue of the second one.
        .expect(3)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    c.chat_completions_capability.bounded_reasoning_continuation = Some(true);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("inline bounded continuation succeeds");

    assert_eq!(reply, "completed after bounded continuation");
    assert!(!reply.contains("inline plan"));
    let request = second_request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("second request captured");
    let replayed = request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("inline assistant partial replayed");
    assert_eq!(replayed["content"], "<think>unfinished inline plan 0");
    assert!(replayed.get("reasoning_content").is_none());
}

#[tokio::test]
async fn openai_reasoning_overflow_does_not_retry_an_unknown_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "unfinished private plan"
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
    c.solve_obs = Some(&mut observation);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("unknown endpoint stops without an unsafe retry");

    assert!(reply.contains("empty response"));
    assert!(!reply.contains("private plan"));
    assert!(observation.behavior_signals.iter().any(|signal| matches!(
        signal,
        crate::agentic::observability::BehaviorSignal::ReasoningOverflow {
            continuation_attempted: false,
            continuation_succeeded: false,
            ..
        }
    )));
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
