use super::*;

#[tokio::test]
async fn responses_proactively_compacts_before_the_first_dispatch() {
    // #1528 B3 (req 1/2): when the FIRST request is LOCALLY known to exceed the
    // input budget, the loop compacts BEFORE dispatching — no round-trip to learn
    // it from a cw-400. The single request the server sees is already compacted
    // (carries the reference-summary envelope) and STILL carries tools (a real
    // tool round — the round counter is not consumed). Regression: before B3 the
    // Responses loop only compacted REACTIVELY, after a provider 400 (so this
    // request would have been dispatched over budget, or 400'd first).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "proactively compacted"}]}]
        })))
        .mount(&server)
        .await;

    let task = "PROACTIVE: compact before the first dispatch";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    // A LOCAL budget (no cw-400 needed): the raw history dwarfs it, the compacted
    // form fits. recover_cw_400 stays None — proactive never learns from a 400.
    ctx.safe_context = Some(12_000);
    ctx.num_ctx = Some(12_000);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("proactive compaction fits the request and it dispatches once");
    assert_eq!(reply, "proactively compacted");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        1,
        "a single dispatch — proactively compacted, no cw-400 round-trip"
    );
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap_or_default();
    assert!(
        body["input"]
            .to_string()
            .contains("newt-compaction-summary"),
        "the first request was compacted BEFORE dispatch (reference-summary envelope present)"
    );
    assert!(
        body["tools"].is_array(),
        "the compacted request is still a REAL tool round — the round is not consumed"
    );
    let n_items = body["input"].as_array().map_or(0, Vec::len);
    assert!(n_items < 30, "compacted to {n_items} items (raw was ~121)");
}

#[tokio::test]
async fn responses_final_summary_is_proactively_compacted() {
    // #1528 B3 (req 6): the tools-DISABLED final summary is proactively compacted
    // when it is locally over budget, before its dispatch. `max_tool_rounds == 0`
    // skips the tool rounds so this exercises ONLY the final-summary guard.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "summarized"}]}]
        })))
        .mount(&server)
        .await;

    let task = "FINAL SUMMARY: compact the tools-disabled summary too";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = Some(12_000);
    ctx.num_ctx = Some(12_000);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 0;
    let mut end_reason = None;
    ctx.end_reason = Some(&mut end_reason);

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("the tools-disabled summary is proactively compacted and dispatches");
    assert_eq!(reply, "summarized");
    assert_eq!(end_reason, Some(crate::TurnEndReason::RoundCap));

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(reqs.len(), 1, "only the final summary dispatched");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap_or_default();
    assert!(
        body["tools"].is_null(),
        "the final summary is tools-disabled"
    );
    assert!(
        body["input"]
            .to_string()
            .contains("newt-compaction-summary"),
        "the tools-disabled summary was proactively compacted before dispatch"
    );
    assert!(
        body["input"].to_string().contains("progress update"),
        "Responses receives the same resumable cap handoff as chat backends"
    );
}

// --- #1528 B3 (item 6): pure lifecycle-invariant property tests ---

/// A pure, side-effect-free MODEL of one proactive pre-dispatch decision, mirroring
/// the guard + transactional `compact_responses_input` + the authoritative preflight
/// at the estimate level. `committed_post_estimate` is `Some(fenced estimate)` IFF
/// the transactional helper committed a candidate (which it does only when the
/// candidate passed `check_post_bridge_budget`, i.e. fits the budget). Corresponds to
/// `formal/CompactionLifecycle` (estimate → compact → validate → readyToDispatch |
/// abort).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProactiveDecision {
    DispatchAsIs,
    DispatchCompacted,
    FailClosed,
}

fn proactive_decision(
    actionable_budget: Option<usize>,
    pre_estimate: usize,
    committed_post_estimate: Option<usize>,
) -> ProactiveDecision {
    let Some(budget) = actionable_budget else {
        return ProactiveDecision::DispatchAsIs; // no budget → no gate
    };
    if pre_estimate <= budget {
        return ProactiveDecision::DispatchAsIs; // already fits → no compaction
    }
    match committed_post_estimate {
        Some(post) if post <= budget => ProactiveDecision::DispatchCompacted,
        _ => ProactiveDecision::FailClosed,
    }
}

/// B3-CG-001/005: over the whole finite decision space, DISPATCH implies the
/// governing estimate is within budget, and a compaction FAILURE implies zero
/// dispatch. Also: no budget → dispatch as-is; a fitting request is never compacted.
#[test]
fn proactive_decision_never_dispatches_over_budget() {
    for budget in [None, Some(0usize), Some(100), Some(1000)] {
        for pre in [0usize, 100, 1000, 5000] {
            for committed in [None, Some(0usize), Some(100), Some(1000), Some(5000)] {
                let d = proactive_decision(budget, pre, committed);
                match d {
                    ProactiveDecision::DispatchAsIs => {
                        // Only when there is no budget, or the request already fits.
                        assert!(
                            budget.is_none() || pre <= budget.unwrap(),
                            "DispatchAsIs must mean no-budget or already-fits: {budget:?} {pre}"
                        );
                    }
                    ProactiveDecision::DispatchCompacted => {
                        let b = budget.expect("compacted dispatch requires a budget");
                        assert!(pre > b, "compaction only runs when over budget");
                        assert!(
                            committed.expect("committed implies Some") <= b,
                            "DISPATCH ⟹ post-fence estimate ≤ actionable budget"
                        );
                    }
                    ProactiveDecision::FailClosed => {
                        // Failure ⟹ zero dispatch: either no committed candidate, or
                        // the committed candidate did not fit (never emitted here).
                        let b = budget.expect("fail-closed only under a budget");
                        assert!(pre > b);
                        assert!(committed.is_none_or(|p| p > b));
                    }
                }
            }
        }
    }
}

/// B3-CG-003: the tools-disabled compaction target NEVER subtracts schema overhead,
/// the tools-enabled one always does, and dropping schemas can only RAISE the target
/// — across a sweep of ceilings / schema sizes / calibrations.
#[test]
fn compaction_target_schema_subtraction_follows_tools() {
    use super::send_budget::ResponsesBudgetState;
    for window in [4_096u32, 32_768, 131_072] {
        for schema in [0usize, 500, 4_000] {
            for cal in [0.5f32, 1.0, 2.0] {
                let mut s = ResponsesBudgetState::new(Some(window), 80, None, None, None, None);
                let recovered = s.recovered_budget_for_window(window);
                s.recover_from_cw400(recovered);
                s.set_tool_schema_tokens(schema);
                let with = s.compaction_budget(cal, true);
                let without = s.compaction_budget(cal, false);
                assert!(
                    without >= with,
                    "dropping schemas never lowers the target: {without} >= {with} \
                         (window={window} schema={schema} cal={cal})"
                );
                if schema == 0 {
                    assert_eq!(with, without, "no schemas → the flag is a no-op");
                }
            }
        }
    }
}

/// #1528 B3 (item 3, B3-CG-005): the IN-LOOP no-progress path — a tool round
/// yields an OVERSIZED fresh result, the next request exceeds the local budget,
/// and the proactive guard invokes the compactor EXACTLY ONCE (a counting
/// summarizer proves it). The protected newest tool output cannot be reduced, so
/// the authoritative preflight refuses: ONE dispatch, ZERO second dispatch, the
/// logical round intact. This FAILS if the proactive helper call is deleted — the
/// summarizer is never invoked on round 1 (`calls == 0`), which the pre-existing
/// preflight-refusal path does not do.
#[tokio::test]
async fn responses_proactive_no_progress_invokes_the_compactor_exactly_once() {
    use crate::agentic::compress::Summarizer;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "function_call", "name": "read_file",
                "arguments": "{\"path\":\"huge.txt\"}", "call_id": "c1"}]
        })))
        .mount(&server)
        .await;
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("huge.txt"), "x".repeat(64_000)).unwrap();

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let summ: Summarizer = Box::new(move |_r: String| {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok("brief".to_string()) })
    });

    let task = "read the huge fixture then summarize";
    // A SMALL reducible history — round 0 fits, and it survives as a summarizable
    // MIDDLE on round 1 (after the giant tool output becomes the protected tail).
    let mut messages = vec![MemMessage::system("base policy")];
    for i in 0..4 {
        messages.push(MemMessage::user(format!("earlier ask {i}")));
        messages.push(MemMessage::assistant(format!("earlier reply {i}")));
    }
    messages.push(MemMessage::user(task));
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.workspace = workspace.path().to_str().unwrap();
    ctx.summarizer = Some(&*summ);
    ctx.safe_context = Some(8_000);
    ctx.num_ctx = Some(8_000);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 2;

    let error = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the fresh giant tool output cannot be compacted → fail closed");
    let chain = format!("{error:#}");
    assert!(
        chain.contains("function outputs were not truncated"),
        "in-loop preflight refusal expected: {chain}"
    );
    assert!(
        chain.contains("proactive compaction"),
        "the B3 compaction reason is attached to the chain: {chain}"
    );
    let requests = server.received_requests().await.expect("request journal");
    assert_eq!(
        requests.len(),
        1,
        "round 0 dispatched; the oversized round 1 never did (zero second dispatch)"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the proactive guard invoked the compactor EXACTLY once on round 1"
    );
}

/// #1528 B3 (B3-CG-003, item 1): after the provider rejects tools, the SAME round
/// retries TOOLS-DISABLED; when that retry is locally over budget the PROACTIVE
/// guard compacts it with a target that does NOT subtract tool-schema overhead
/// (the request sends none). The HTTP journal shows req1 WITH tools, req2 WITHOUT
/// tools and proactively compacted, and the turn completes in the same round.
/// (The exact "no schema subtraction" arithmetic is pinned deterministically by
/// `compaction_target_schema_subtraction_follows_tools` and the budget unit test.)
#[tokio::test]
async fn responses_unsupported_tools_retry_is_proactively_compacted_without_schema_overhead() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct UnsupportedThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UnsupportedThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.served.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "this model does not support tools"}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "done tools-disabled"}]}]
                }))
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(UnsupportedThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "summarize the history";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = Some(12_000);
    ctx.num_ctx = Some(12_000);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("tools-disabled retry is proactively compacted and completes in the same round");
    assert_eq!(reply, "done tools-disabled");
    let reqs = server.received_requests().await.expect("request journal");
    assert_eq!(
        reqs.len(),
        2,
        "req1 (tools) rejected, req2 (tools-disabled) retried IN THE SAME round"
    );
    let b1: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap_or_default();
    assert!(b1["tools"].is_array(), "req1 advertised tools");
    let b2: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap_or_default();
    assert!(
        b2["tools"].is_null(),
        "req2 is tools-disabled (the schema overhead it must not reserve)"
    );
    assert!(
        b2["input"].to_string().contains("newt-compaction-summary"),
        "req2 was proactively compacted before dispatch"
    );
}
