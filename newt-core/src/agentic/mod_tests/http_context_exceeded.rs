use super::*;

struct OverflowRequests {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    failures: usize,
    replay: DisplayReplay,
}

impl Respond for OverflowRequests {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if self.replay.take(req) {
            return sse_replay("recovered");
        }
        let mut requests = self.requests.lock().unwrap();
        requests.push(body_json(req));
        if requests.len() <= self.failures {
            ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": {"message": "Context size has been exceeded."}
            }))
        } else {
            self.replay.arm(req);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"finish_reason": "stop", "message": {
                    "role": "assistant", "content": "recovered"
                }}]
            }))
        }
    }
}

fn history() -> Vec<MemMessage> {
    let mut messages = vec![MemMessage::system("base policy")];
    for i in 0..50 {
        messages.push(MemMessage::user(format!(
            "old step {i}: {}",
            "x".repeat(1_200)
        )));
        messages.push(MemMessage::assistant(format!(
            "old reply {i}: {}",
            "y".repeat(1_200)
        )));
    }
    messages.push(MemMessage::user("keep the exact operator prompt"));
    messages
}

type LoopResult = anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)>;

async fn run_overflow(
    failures: usize,
    messages: &[MemMessage],
    smart: bool,
) -> (LoopResult, Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OverflowRequests {
            requests: requests.clone(),
            failures,
            replay: Default::default(),
        })
        .mount(&server)
        .await;
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations = observability::SolveObservation::default();
    let harness = smart.then(|| {
        crate::agentic::smart_harness::SmartHarness::new(
            agent_harness::Session::new(Default::default()).unwrap(),
            Arc::new(|prompt| {
                let evidence: serde_json::Value =
                    serde_json::from_str(prompt.lines().last().unwrap()).unwrap();
                let reply = if let Some(candidates) = evidence["candidates"].as_array() {
                    serde_json::to_string(
                        &candidates
                            .iter()
                            .filter(|candidate| candidate["required"] == true)
                            .map(|candidate| candidate["cid"].clone())
                            .collect::<Vec<_>>(),
                    )
                    .unwrap()
                } else {
                    "\"answer\"".to_string()
                };
                Box::pin(async move { Ok((reply, None)) })
            }),
            Default::default(),
        )
        .unwrap()
    });
    let mut c = ctx(&uri, messages, &caveats);
    c.smart_harness = harness.as_ref();
    c.kind = BackendKind::Openai;
    c.task = "keep the exact operator prompt";
    c.action_nudges = false;
    c.num_ctx = Some(65_536);
    c.solve_obs = Some(&mut observations);
    let result = chat_complete(c, &mut NoMcp).await;
    if result.is_ok() {
        if let Some(harness) = &harness {
            let transmitted = server.received_requests().await.unwrap();
            assert_eq!(
                harness.replay_last_request().unwrap(),
                transmitted.last().unwrap().body
            );
        }
    }
    let requests = requests.lock().unwrap().clone();
    let events = observations
        .behavior_signals
        .iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .filter(|event| event["kind"] == "context_exceeded")
        .collect();
    (result, requests, events)
}

#[tokio::test]
async fn context_exceeded_shrinks_before_second_attempt_and_records_recovery() {
    let (result, requests, events) = run_overflow(1, &history(), false).await;
    assert_eq!(result.unwrap().0, "recovered");
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["messages"].to_string().len() < requests[0]["messages"].to_string().len(),
        "context-exceeded requests must shrink before redispatch"
    );
    assert_eq!(
        events.len(),
        1,
        "overflow and its re-projection must appear in events"
    );
    assert!(events[0]["projected_tokens"].as_u64().is_some());
    assert!(requests[1]["messages"]
        .to_string()
        .contains("keep the exact operator prompt"));
}

#[tokio::test]
async fn context_exceeded_two_failed_shrinks_end_terminal() {
    let (result, requests, events) = run_overflow(usize::MAX, &history(), false).await;
    assert_eq!(
        observability::error_class(&result.unwrap_err()),
        Some(observability::ErrorClass::ContextExceeded)
    );
    assert_eq!(
        requests.len(),
        3,
        "initial dispatch plus two strictly smaller attempts"
    );
    assert!(requests
        .windows(2)
        .all(|pair| pair[1]["messages"].to_string().len() < pair[0]["messages"].to_string().len()));
    assert_eq!(
        events.len(),
        requests.len(),
        "every rejected attempt must be recorded"
    );
    assert_eq!(
        events.last().unwrap()["projected_tokens"],
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn context_exceeded_exhausted_ends_with_typed_reason_not_none() {
    // Regression (eval F14): a llama.cpp numberless `500 "Context size has been
    // exceeded."` that survives every bounded recovery attempt used to `return
    // Err(e)` with `end_reason` never written — an untyped end indistinguishable
    // from a driver-level crash. It must end `Some(Failed)`.
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OverflowRequests {
            requests: requests.clone(),
            failures: usize::MAX,
            replay: Default::default(),
        })
        .mount(&server)
        .await;
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut reason = None;
    let messages = history();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = "keep the exact operator prompt";
    c.action_nudges = false;
    c.num_ctx = Some(65_536);
    c.end_reason = Some(&mut reason);
    let result = chat_complete(c, &mut NoMcp).await;
    assert!(result.is_err(), "recovery bound must still exhaust");
    assert_eq!(
        reason,
        Some(crate::TurnEndReason::Failed),
        "an exhausted context-overflow recovery must never leave end_reason: None"
    );
}

#[tokio::test]
async fn context_exceeded_irreducible_request_ends_with_typed_reason_not_none() {
    // Same regression as above, but for the single-attempt "request is
    // irreducible" exhaustion path (see `context_exceeded_never_resends_an_
    // unchanged_request` below) — a distinct early return site.
    let caveats = Caveats::top();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OverflowRequests {
            requests: Arc::new(Mutex::new(Vec::new())),
            failures: usize::MAX,
            replay: Default::default(),
        })
        .mount(&server)
        .await;
    let uri = server.uri();
    let mut reason = None;
    let messages = msgs();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.action_nudges = false;
    c.end_reason = Some(&mut reason);
    let result = chat_complete(c, &mut NoMcp).await;
    assert!(result.is_err());
    assert_eq!(reason, Some(crate::TurnEndReason::Failed));
}

#[tokio::test]
async fn context_exceeded_never_resends_an_unchanged_request() {
    let (result, requests, events) = run_overflow(usize::MAX, &msgs(), false).await;
    assert_eq!(
        observability::error_class(&result.unwrap_err()),
        Some(observability::ErrorClass::ContextExceeded)
    );
    assert_eq!(
        requests.len(),
        1,
        "irreducible request must not be retried unchanged"
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["projected_tokens"], serde_json::Value::Null);
}

#[tokio::test]
async fn smart_context_exceeded_reprojects_the_recorded_request_before_retry() {
    let (result, requests, events) = run_overflow(1, &history(), true).await;
    assert_eq!(result.unwrap().0, "recovered");
    assert_eq!(requests.len(), 2);
    assert!(requests[1]["messages"].to_string().len() < requests[0]["messages"].to_string().len());
    assert_eq!(events.len(), 1);
    assert!(events[0]["projected_tokens"].as_u64().is_some());
}

#[tokio::test]
async fn cap_summary_context_exceeded_keeps_round_cap_and_records_the_rejection() {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let mut requests = captured.lock().unwrap();
            requests.push(body_json(request));
            if requests.len() == 1 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices":[{"finish_reason":"tool_calls","message":{
                        "role":"assistant","content":null,"tool_calls":[{
                            "id":"completed_call","type":"function","function":{
                                "name":"get_context_remaining","arguments":"{}"
                            }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(500).set_body_json(serde_json::json!({
                    "error":{"message":"Context size has been exceeded."}
                }))
            }
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = observability::SolveObservation::default();
    let mut state = CompressState::new();
    let mut reason = None;
    let mut context = ctx(&uri, &messages, &caveats);
    context.kind = BackendKind::Openai;
    context.max_tool_rounds = 1;
    context.action_nudges = false;
    context.solve_obs = Some(&mut observation);
    context.compress_state = Some(&mut state);
    context.end_reason = Some(&mut reason);
    let (reply, _, _, _) = chat_complete(context, &mut NoMcp).await.unwrap();
    assert!(reply.contains("tool-round limit (1"), "{reply}");
    assert_eq!(reason, Some(crate::TurnEndReason::RoundCap));
    let requests = requests.lock().unwrap();
    let summaries = &requests[1..];
    assert!(!summaries.is_empty());
    assert!(summaries.len() <= 3, "at most two smaller summary attempts");
    for summary in summaries {
        assert!(
            summary["tools"].is_null(),
            "the tool cap cannot be bypassed"
        );
        let messages = summary["messages"].as_array().unwrap();
        assert!(messages.iter().any(|message| {
            message["role"] == "tool" && message["tool_call_id"] == "completed_call"
        }));
    }
    assert!(
        summaries.windows(2).all(|pair| {
            pair[1]["messages"].to_string().len() < pair[0]["messages"].to_string().len()
        }),
        "a rejected summary must never be re-sent unchanged"
    );
    let rejections = observation
        .behavior_signals
        .iter()
        .filter(|signal| {
            matches!(
                signal,
                observability::BehaviorSignal::ContextExceeded { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rejections.len(),
        summaries.len(),
        "a rejected cap summary must not disappear into the fallback"
    );
    assert!(matches!(
        rejections.last().unwrap(),
        observability::BehaviorSignal::ContextExceeded {
            projected_tokens: None,
            ..
        }
    ));
    assert!(state.calibration.ratio(None) >= 1.5);
}
