//! Exact admission is checked against the actual provider request, without a model.
use super::*;
use crate::agentic::smart_harness::SmartHarness;

const WINDOW: u32 = 65_536;
const INPUT_BOUND: u32 = WINDOW * 80 / 100;
const TASK: &str = "keep the exact operator prompt";

#[derive(Default)]
struct AdmissionTrace {
    probes: Vec<(Vec<u8>, Option<u32>, Vec<u8>)>,
    generations: Vec<Vec<u8>>,
    consumed_probes: usize,
    violations: Vec<&'static str>,
}

type Trace = Arc<Mutex<AdmissionTrace>>;

fn measured_tokens(body: &serde_json::Value) -> u32 {
    // This fake tokenizer exposes the cold chars/4 under-count on tool-heavy
    // history; the client must use the endpoint's result, not this formula.
    (body["messages"].to_string().chars().count().div_ceil(2)
        + body["tools"].to_string().chars().count().div_ceil(4)) as u32
}

fn record_count(trace: &Trace, request: &Request, tokens: Option<u32>) -> ResponseTemplate {
    let bytes = tokens
        .map(|tokens| serde_json::to_vec(&serde_json::json!({"input_tokens":tokens})).unwrap())
        .unwrap_or_default();
    trace
        .lock()
        .unwrap()
        .probes
        .push((request.body.clone(), tokens, bytes.clone()));
    ResponseTemplate::new(if tokens.is_some() { 200 } else { 404 })
        .set_body_raw(bytes, "application/json")
}

async fn mount_counter(
    server: &MockServer,
    trace: &Trace,
    count: Option<fn(&serde_json::Value) -> u32>,
) {
    let trace = trace.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/input_tokens"))
        .respond_with(move |request: &Request| {
            record_count(
                &trace,
                request,
                count.map(|count| count(&body_json(request))),
            )
        })
        .mount(server)
        .await;
}

async fn mount_generation(server: &MockServer, trace: &Trace, exact: bool, tool_first: bool) {
    mount_generation_with_failures(server, trace, exact, tool_first, 0).await;
}

async fn mount_generation_with_failures(
    server: &MockServer,
    trace: &Trace,
    exact: bool,
    tool_first: bool,
    failures: usize,
) {
    let trace = trace.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let mut trace = trace.lock().unwrap();
            trace.generations.push(request.body.clone());
            if exact {
                let admitted = trace.probes.len() > trace.consumed_probes
                    && trace.probes.last().is_some_and(|(body, tokens, _)| {
                        body == &request.body && tokens.is_some_and(|n| n <= INPUT_BOUND)
                    });
                if !admitted {
                    trace
                        .violations
                        .push("generation lacked a fresh matching count within the declared bound");
                    // A deterministic, non-retryable fixture failure. Assert in
                    // the test task rather than panicking the mock server task.
                    return ResponseTemplate::new(400)
                        .set_body_string("generation lacked exact admission");
                }
                trace.consumed_probes = trace.probes.len();
            }
            if trace.generations.len() <= failures {
                return ResponseTemplate::new(503).set_body_string("server busy");
            }
            if tool_first && trace.generations.len() == failures + 1 {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices":[{"finish_reason":"tool_calls","message":{
                        "role":"assistant","content":null,"tool_calls":[{
                            "id":"measured_call","type":"function","function":{
                                "name":"get_context_remaining","arguments":"{}"
                            }
                        }]
                    }}]
                }));
            }
            if is_stream(request) {
                return sse_replay("measured answer");
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"finish_reason":"stop","message":{
                    "role":"assistant","content":"measured answer"
                }}]
            }))
        })
        .mount(server)
        .await;
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
    messages.push(MemMessage::user(TASK));
    messages
}

fn count_ctx<'a>(uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    let mut context = ctx(uri, messages, caveats);
    context.kind = BackendKind::Openai;
    context.num_ctx = Some(WINDOW);
    context.task = TASK;
    context.action_nudges = false;
    context
}

fn smart(session: agent_harness::Session) -> SmartHarness {
    SmartHarness::new(
        session,
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
}

#[tokio::test]
async fn exact_count_reprojects_and_recounts_before_any_generation() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    mount_counter(&server, &trace, Some(measured_tokens)).await;
    mount_generation(&server, &trace, true, false).await;
    let messages = history();
    let caveats = Caveats::top();
    let result = chat_complete(count_ctx(&server.uri(), &messages, &caveats), &mut NoMcp).await;
    let trace = trace.lock().unwrap();
    assert!(trace.violations.is_empty(), "{:?}", trace.violations);
    assert_eq!(result.unwrap().0, "measured answer");
    assert!(
        trace.probes[0].1.unwrap() > INPUT_BOUND,
        "fixture must expose the cold-prior under-count"
    );
    assert!(trace.probes.len() >= 2);
    assert!(trace.probes[1].0.len() < trace.probes[0].0.len());
    assert_ne!(
        trace.generations[0], trace.probes[0].0,
        "oversized candidate reached generation"
    );
    assert!(String::from_utf8_lossy(&trace.generations[0]).contains(TASK));
}

#[tokio::test]
async fn exact_count_rejection_preserves_a_known_window_for_the_protected_prompt() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    mount_counter(&server, &trace, Some(measured_tokens)).await;
    mount_generation(&server, &trace, true, false).await;
    // The prompt's protected and live copies cost more than 80% of the
    // authoritative input bound but fit inside that bound. Rejecting removable
    // history must not be misread as a smaller server context window.
    let task = "p".repeat(43_000);
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user(format!("old step: {}", "x".repeat(10_000))),
        MemMessage::assistant(format!("old reply: {}", "y".repeat(10_000))),
        MemMessage::user(&task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = count_ctx(&uri, &messages, &caveats);
    context.task = &task;
    let result = chat_complete(context, &mut NoMcp).await;
    let trace = trace.lock().unwrap();
    assert!(trace.violations.is_empty(), "{:?}", trace.violations);
    assert!(trace.probes[0].1.unwrap() > INPUT_BOUND);
    assert_eq!(result.unwrap().0, "measured answer");
    let (_, Some(admitted), _) = trace.probes.last().unwrap() else {
        panic!("the successful request must have a measured count");
    };
    assert!(*admitted > INPUT_BOUND * 80 / 100);
    assert!(*admitted <= INPUT_BOUND);
    assert!(trace.generations[0].len() < trace.probes[0].0.len());
    for request in &trace.generations {
        let body: serde_json::Value = serde_json::from_slice(request).unwrap();
        assert!(body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| { message["role"] == "user" && message["content"] == task }));
    }
}

#[tokio::test]
async fn exact_count_irreducible_operator_prompt_sends_no_generation() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    mount_counter(&server, &trace, Some(|_| INPUT_BOUND + 1)).await;
    mount_generation(&server, &trace, true, false).await;
    let messages = vec![MemMessage::system("policy"), MemMessage::user(TASK)];
    let caveats = Caveats::top();
    let result = chat_complete(count_ctx(&server.uri(), &messages, &caveats), &mut NoMcp).await;
    let trace = trace.lock().unwrap();
    assert!(result.is_err());
    assert!(
        !trace.probes.is_empty(),
        "the assembled request must be measured"
    );
    assert!(
        trace.generations.is_empty(),
        "irreducible exact overflow must not dispatch"
    );
    assert!(trace
        .probes
        .iter()
        .all(|(body, _, _)| String::from_utf8_lossy(body).contains(TASK)));
}

#[tokio::test]
async fn exact_count_preserves_the_latest_observed_tool_result_when_refusing() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    mount_counter(&server, &trace, Some(|_| INPUT_BOUND + 1)).await;
    mount_generation(&server, &trace, true, false).await;
    let result_text = "latest observed tool result must remain exact";
    let mut session = agent_harness::Session::new(Default::default()).unwrap();
    session.record_messages(&[
        serde_json::json!({"role":"user","content":TASK}),
        serde_json::json!({"role":"assistant","content":"","tool_calls":[{
            "id":"observed_call","type":"function","function":{"name":"read_file","arguments":"{}"}
        }]}),
        serde_json::json!({"role":"tool","tool_call_id":"observed_call","content":result_text}),
    ]).unwrap();
    let harness = smart(session);
    let messages = vec![MemMessage::system("policy"), MemMessage::user(TASK)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = count_ctx(&uri, &messages, &caveats);
    context.smart_harness = Some(&harness);
    let result = chat_complete(context, &mut NoMcp).await;
    let trace = trace.lock().unwrap();
    assert!(result.is_err());
    assert!(!trace.probes.is_empty());
    assert!(trace.generations.is_empty());
    for (body, _, _) in &trace.probes {
        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert!(body["messages"].to_string().contains(TASK));
        assert!(body["messages"].as_array().unwrap().iter().any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == "observed_call"
                && message["content"] == result_text
        }));
    }
}

#[tokio::test]
async fn exact_count_absent_endpoint_falls_back_to_anchored_admission() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    mount_counter(&server, &trace, None).await;
    mount_generation(&server, &trace, false, false).await;
    let messages = msgs();
    let caveats = Caveats::top();
    let result = chat_complete(count_ctx(&server.uri(), &messages, &caveats), &mut NoMcp).await;
    let trace = trace.lock().unwrap();
    assert_eq!(result.unwrap().0, "measured answer");
    assert!(
        !trace.probes.is_empty(),
        "endpoint absence must be observed"
    );
    assert!(trace.probes.iter().all(|(_, tokens, _)| tokens.is_none()));
    assert!(!trace.generations.is_empty());
}

#[tokio::test]
async fn exact_count_rechecks_identical_turns_and_final_display_reissues() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    mount_counter(&server, &trace, Some(|_| 43_210)).await;
    mount_generation(&server, &trace, true, false).await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut state = CompressState::new();
    for _ in 0..2 {
        let mut context = count_ctx(&uri, &messages, &caveats);
        context.compress_state = Some(&mut state);
        let result = chat_complete(context, &mut NoMcp).await;
        assert!(trace.lock().unwrap().violations.is_empty());
        assert_eq!(result.unwrap().0, "measured answer");
    }
    let trace = trace.lock().unwrap();
    assert_eq!(
        trace.generations.len(),
        4,
        "two primary rounds and their display reissues"
    );
    assert_eq!(
        trace.probes.len(),
        trace.generations.len(),
        "no count may be cached across requests or turns"
    );
}

#[tokio::test]
async fn exact_count_covers_the_tools_disabled_cap_exit_summary() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    mount_counter(&server, &trace, Some(|_| 43_210)).await;
    mount_generation(&server, &trace, true, true).await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = count_ctx(&uri, &messages, &caveats);
    context.max_tool_rounds = 1;
    let result = chat_complete(context, &mut NoMcp).await;
    let trace = trace.lock().unwrap();
    assert!(trace.violations.is_empty(), "{:?}", trace.violations);
    assert_eq!(result.unwrap().0, "measured answer");
    assert_eq!(
        trace.generations.len(),
        2,
        "one tool round and the cap summary"
    );
    let first: serde_json::Value = serde_json::from_slice(&trace.generations[0]).unwrap();
    let summary: serde_json::Value = serde_json::from_slice(&trace.generations[1]).unwrap();
    assert!(first["tools"].is_array());
    assert!(summary["tools"].is_null());
    assert_eq!(trace.probes.len(), 2);
}

fn reachable_events(
    directory: &std::path::Path,
    head: content_addressable::ContentId,
    replayed_requests: &mut Vec<Vec<u8>>,
) -> Vec<(serde_json::Value, serde_json::Value)> {
    use agent_harness::forensics::{inspect_from_store, InspectionLimits};
    let store = agent_harness::store::FrameStore::open(directory).unwrap();
    // The 102-message fixture has 205 immediate projection references before
    // harness additions. Bound this exhaustive inspection separately from the
    // default 64-reference budget; retain its normal byte limit.
    let inspection_limits = InspectionLimits {
        max_references: 256,
        ..Default::default()
    };
    let mut pending = vec![head];
    let mut seen = std::collections::BTreeSet::new();
    let mut events = Vec::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(record) = inspect_from_store(&store, id, inspection_limits).unwrap() else {
            continue;
        };
        if record.kind == "request" {
            replayed_requests
                .push(agent_harness::forensics::replay_from_store(&store, record.id).unwrap());
        }
        pending.extend(record.parents);
        for reference in record.references {
            if reference.profile == "dag-cbor" {
                pending.push(reference.cid.parse().unwrap());
            } else if record.kind == "event" && reference.relation == "payload" {
                let bytes = store.source(&reference.cid.parse().unwrap()).unwrap();
                if let Ok(payload) = serde_json::from_slice(&bytes) {
                    events.push((record.record.clone(), payload));
                }
            }
        }
    }
    events
}

/// Grounds the mocked admission in a reopened store: observed count bytes and
/// the actual dispatched request remain reachable through its committed head.
#[tokio::test]
async fn exact_count_smart_admission_keeps_raw_response_evidence_and_request_replay() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    mount_counter(&server, &trace, Some(measured_tokens)).await;
    mount_generation(&server, &trace, true, false).await;
    let directory = tempfile::tempdir().unwrap();
    let harness =
        smart(agent_harness::Session::open(directory.path(), Default::default()).unwrap());
    let messages = history();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = count_ctx(&uri, &messages, &caveats);
    context.smart_harness = Some(&harness);
    let result = chat_complete(context, &mut NoMcp).await;
    let trace = trace.lock().unwrap();
    assert!(trace.violations.is_empty(), "{:?}", trace.violations);
    assert_eq!(result.unwrap().0, "measured answer");
    assert_eq!(
        trace.generations.len(),
        1,
        "smart mode does not regenerate its final answer"
    );
    assert_eq!(harness.replay_last_request().unwrap(), trace.generations[0]);
    let head = harness.head().unwrap();
    drop(harness);
    let restored =
        agent_harness::Session::restore(directory.path(), head, "local-session").unwrap();
    let mut replayed_requests = Vec::new();
    let events = reachable_events(directory.path(), restored.head(), &mut replayed_requests);
    assert!(
        replayed_requests.contains(&trace.generations[0]),
        "the dispatched bytes must replay from the fresh committed graph"
    );
    for (_, _, response_bytes) in &trace.probes {
        let matching = events
            .iter()
            .filter(|(_, payload)| {
                payload["response_body"]
                    .as_str()
                    .is_some_and(|text| text.as_bytes() == response_bytes)
            })
            .collect::<Vec<_>>();
        assert!(
            !matching.is_empty(),
            "observed count response is not reachable after restore"
        );
        assert!(
            matching
                .iter()
                .all(|(event, _)| event["origin"] == "harness" && event["kind"] == "intervention"),
            "count admission must not fabricate a primary model reply"
        );
    }
}

/// Grounds commit-before-dispatch in a real failed head publication immediately
/// after the counter replies; no generation may follow an uncommitted count.
#[tokio::test]
async fn exact_count_smart_persistence_failure_prevents_generation() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    let directory = tempfile::tempdir().unwrap();
    let harness =
        smart(agent_harness::Session::open(directory.path(), Default::default()).unwrap());
    let counter_trace = trace.clone();
    let directory_path = directory.path().to_path_buf();
    let broken = AtomicBool::new(false);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/input_tokens"))
        .respond_with(move |request: &Request| {
            let response = record_count(&counter_trace, request, Some(43_210));
            if !broken.swap(true, Ordering::SeqCst) {
                std::fs::rename(
                    directory_path.join("heads"),
                    directory_path.join("retained-heads"),
                )
                .unwrap();
                std::fs::write(
                    directory_path.join("heads"),
                    b"blocked checkpoint directory",
                )
                .unwrap();
            }
            response
        })
        .mount(&server)
        .await;
    mount_generation(&server, &trace, true, false).await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = count_ctx(&uri, &messages, &caveats);
    context.smart_harness = Some(&harness);
    let result = chat_complete(context, &mut NoMcp).await;
    let trace = trace.lock().unwrap();
    assert!(result.is_err());
    assert_eq!(trace.probes.len(), 1);
    assert!(
        trace.generations.is_empty(),
        "generation followed a failed admission commit"
    );
}

fn request_prior(body: &serde_json::Value) -> usize {
    estimate_request_tokens(
        body["messages"].as_array().unwrap(),
        body.get("tools"),
        crate::tokens::TokenEstimation::default(),
    )
}

async fn check_optional_measurement(cap_summary: bool, rejected: bool) {
    let server = MockServer::start().await;
    let trace = Trace::default();
    let counted = trace.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/input_tokens"))
        .respond_with(move |request: &Request| {
            let prior = request_prior(&body_json(request));
            let primary = counted.lock().unwrap().probes.is_empty();
            let tokens = if primary {
                prior as u32
            } else if rejected {
                INPUT_BOUND + 1_000
            } else {
                (prior * 2) as u32
            };
            record_count(&counted, request, Some(tokens))
        })
        .mount(&server)
        .await;
    mount_generation(&server, &trace, true, cap_summary).await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut state = CompressState::new();
    let mut observation = observability::SolveObservation::default();
    let mut reason = None;
    let mut context = count_ctx(&uri, &messages, &caveats);
    context.max_tool_rounds = if cap_summary { 1 } else { 8 };
    context.compress_state = Some(&mut state);
    context.solve_obs = Some(&mut observation);
    context.end_reason = Some(&mut reason);
    let (reply, _, _, _) = chat_complete(context, &mut NoMcp).await.unwrap();
    let trace = trace.lock().unwrap();
    assert!(trace.violations.is_empty(), "{:?}", trace.violations);
    assert_eq!(trace.probes.len(), 2, "one primary and one optional count");
    assert_eq!(trace.generations.len(), if rejected { 1 } else { 2 });
    let (body, Some(tokens), _) = &trace.probes[1] else {
        panic!("the optional request must have a measured count");
    };
    let body: serde_json::Value = serde_json::from_slice(body).unwrap();
    let expected_ratio = *tokens as f32 / request_prior(&body) as f32;
    assert!(
        expected_ratio > 1.5,
        "fixture must distinguish measured evidence from guessing"
    );
    assert_eq!(
        state.calibration.ratio(None),
        expected_ratio,
        "optional count must calibrate its own request, not reuse the primary count or guess 1.5x"
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
        .count();
    assert_eq!(rejections, usize::from(rejected));
    assert_eq!(
        reason,
        Some(if cap_summary {
            crate::TurnEndReason::RoundCap
        } else {
            crate::TurnEndReason::Completed
        })
    );
    if cap_summary {
        assert!(body["tools"].is_null());
        assert!(body["messages"].as_array().unwrap().iter().any(|message| {
            message["role"] == "tool" && message["tool_call_id"] == "measured_call"
        }));
        if rejected {
            assert!(reply.contains("tool-round limit (1"), "{reply}");
        }
    } else {
        assert_eq!(reply, "measured answer");
    }
}

#[tokio::test]
async fn exact_count_optional_display_refusal_learns_its_own_measurement() {
    check_optional_measurement(false, true).await;
}

#[tokio::test]
async fn exact_count_optional_cap_refusal_learns_its_own_measurement() {
    check_optional_measurement(true, true).await;
}

#[tokio::test]
async fn exact_count_optional_display_success_learns_its_own_measurement() {
    check_optional_measurement(false, false).await;
}

#[tokio::test]
async fn exact_count_optional_cap_success_learns_its_own_measurement() {
    check_optional_measurement(true, false).await;
}

#[tokio::test]
async fn exact_count_transport_retry_keeps_the_strongest_observed_count() {
    let server = MockServer::start().await;
    let trace = Trace::default();
    let counted = trace.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions/input_tokens"))
        .respond_with(move |request: &Request| {
            let prior = request_prior(&body_json(request));
            let first = counted.lock().unwrap().probes.is_empty();
            record_count(
                &counted,
                request,
                Some((prior * if first { 2 } else { 1 }) as u32),
            )
        })
        .mount(&server)
        .await;
    mount_generation_with_failures(&server, &trace, true, false, 1).await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut state = CompressState::new();
    let mut context = count_ctx(&uri, &messages, &caveats);
    context.compress_state = Some(&mut state);
    let (reply, _, _, _) = chat_complete(context, &mut NoMcp).await.unwrap();
    let trace = trace.lock().unwrap();
    assert!(trace.violations.is_empty(), "{:?}", trace.violations);
    assert_eq!(reply, "measured answer");
    assert_eq!(
        trace.probes.len(),
        3,
        "a fresh count precedes each generation"
    );
    assert_eq!(
        trace.generations.len(),
        3,
        "one transient retry and the display reissue"
    );
    assert_eq!(
        trace.generations[0], trace.generations[1],
        "the retry fixture must use the same request"
    );
    assert!(trace.probes[0].1.unwrap() > trace.probes[1].1.unwrap());
    assert_eq!(
        state.calibration.ratio(None),
        2.0,
        "a lower count on a genuine transport retry must not erase stronger observed evidence"
    );
}
