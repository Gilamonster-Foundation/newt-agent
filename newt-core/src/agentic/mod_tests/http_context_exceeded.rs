use super::*;

struct OverflowRequests {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    failures: usize,
}

impl Respond for OverflowRequests {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            return sse_replay("recovered");
        }
        let mut requests = self.requests.lock().unwrap();
        requests.push(body_json(req));
        if requests.len() <= self.failures {
            ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "error": {"message": "Context size has been exceeded."}
            }))
        } else {
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
                Box::pin(async move { Ok(reply) })
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
