use super::*;

#[tokio::test]
async fn dispatch_responses_json_retries_transient_transport_failures() {
    // R5: the ONE shared Responses dispatch (used by BOTH the per-round loop
    // and the final tools-disabled summary) retries a transient status. A
    // persistent 503 exhausts the retries — the mock's `.expect(3)` (initial
    // + 2 retries) proves the summary path is no longer a bare, un-retried
    // `send()` that a transient blip could discard after all rounds were spent.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(503)) // retryable transport failure
        .expect(3)
        .mount(&server)
        .await;

    let retry = crate::retry::RetryPolicy {
        max_retries: 2,
        base: std::time::Duration::from_millis(0),
        max: std::time::Duration::from_millis(0),
        jitter: false,
    };
    let client = reqwest::Client::new();
    let url = format!("{}/v1/responses", server.uri());
    // B5: the dispatcher accepts only a ValidatedResponsesRequest. This test
    // exercises transport backoff, not the wire invariants, so it uses the
    // test-only constructor to stand up a validated body directly.
    let validated = responses_wire_validation::ValidatedResponsesRequest::from_body_for_test(
        serde_json::json!({"model": "m", "input": []}),
    );
    let err = super::dispatch_responses_json(&client, &url, None, &validated, &retry, false)
        .await
        .expect_err("a persistent 503 exhausts retries");
    assert_eq!(
        err.to_string(),
        "inference endpoint 503 Service Unavailable: "
    );
    assert_eq!(
        err.downcast_ref::<observability::DispatchError>()
            .unwrap()
            .class,
        observability::ErrorClass::Model
    );
    // `.expect(3)` is verified on server drop — it retried, not sent once.
}

#[tokio::test]
async fn responses_recovers_from_a_context_window_400_by_compacting_and_redispatching() {
    // #1528: a hard context-window 400 on the Responses wire must be
    // RECOVERED — learn the true window, tighten the input ceiling, compact
    // the running history to fit, and re-dispatch — never surfaced as a raw
    // 400. Regression: before this slice the Responses loop returned the 400
    // directly (no compaction, no redispatch).
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct OverflowThenOk {
        served: Arc<AtomicUsize>,
    }
    impl Respond for OverflowThenOk {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.served.fetch_add(1, Ordering::SeqCst) == 0 {
                // A numbered LiteLLM/vLLM overflow cw_overflow recognizes.
                ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "recovered and done"}]}]
                }))
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(OverflowThenOk {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "RECOVER: keep going after the window shrinks";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    // Cloud default: no local window → the first (over-window) request
    // dispatches and the provider's 400 drives recovery.
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    ctx.max_tool_rounds = 4;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("cw-400 recovery compacts and redispatches to success");
    assert_eq!(reply, "recovered and done");
    assert_eq!(
        served.load(Ordering::SeqCst),
        2,
        "exactly one 400 then one recovered 200 — the raw 400 never surfaced"
    );
}

#[tokio::test]
async fn responses_context_window_400_recovery_is_bounded() {
    // #1528: recovery is capped at 2 retries. A server that 400s every time
    // must ultimately surface the error after at most 1 + 2 dispatches, never
    // looping forever.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
        })))
        .expect(3) // initial + exactly 2 bounded recoveries
        .mount(&server)
        .await;

    let task = "BOUNDED: never loop forever on a persistent 400";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    // max_tool_rounds == 1 so the bound proven is the INNER `cw_retries` cap
    // (recovery retries in place), not the outer round cap: a single logical
    // round still dispatches at most initial + 2 recoveries before surfacing.
    ctx.max_tool_rounds = 1;

    openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a persistent cw-400 surfaces after the bounded retries");
    // `.expect(3)` verified on drop: initial + exactly 2 recoveries.
}

#[test]
fn responses_cw_recovery_ceiling_is_monotone_non_increasing() {
    // #1528: each recovery composes the freshly-learned window with the
    // previously-tightened ceiling via `min` (the same `recovered_input_budget`
    // declared-ceiling composition the recovery branch uses), so the effective
    // input ceiling can only shrink — never rebound — across successive 400s.
    let pct = 80;
    // First 400 learns a 6000-token window → 4800 input ceiling.
    let first = recovered_input_budget(6000, pct, None, None);
    assert_eq!(first, 4800);
    // A later 400 reporting a LARGER window must NOT raise the ceiling: the
    // retained tighter ceiling wins.
    let second = recovered_input_budget(100_000, pct, None, Some(first));
    assert!(second <= first, "ceiling rose: {second} > {first}");
    assert_eq!(second, first);
    // A later 400 reporting a SMALLER window tightens further.
    let third = recovered_input_budget(2_000, pct, None, Some(second));
    assert!(
        third < second,
        "ceiling failed to tighten: {third} !< {second}"
    );
}

#[tokio::test]
async fn responses_cw_400_recovery_retries_the_same_logical_round_with_tools() {
    // #1528 (review P1): a cw-400 must retry the SAME logical tool round in
    // place, not advance the round counter. With max_tool_rounds == 1 the buggy
    // loop consumed the only round on recovery and demoted the recovered request
    // to the tools-disabled summary — 2 requests: [400, summary]. The fix
    // dispatches a real recovered TOOL round (still carrying tools); only a
    // COMPLETED round then advances to the summary — 3 requests:
    // [400, recovered tool round, summary].
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct OverflowThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for OverflowThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
                })),
                // The recovered request: a real tool round (get_context_remaining
                // is executed synthetically in-loop — no external side effect).
                1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "function_call", "name": "get_context_remaining",
                        "arguments": "{}", "call_id": "c1"}]
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "done"}]}]
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(OverflowThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "SAME ROUND: recovery must not burn the only tool round";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("recovery retries the round in place and the turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [400, recovered tool round, summary]; a 2-request run means \
             recovery burned the only round and demoted to the tools-disabled summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(1)["tools"].is_array(),
        "the RECOVERED request must still carry tools — a real tool round, not the summary"
    );
    assert!(
        body(2)["tools"].is_null(),
        "only the final summary (after the completed round) is tools-disabled"
    );
}

#[tokio::test]
async fn responses_cw_400_on_the_final_round_recovers_in_place() {
    // #1528 (review P1): a cw-400 on the LAST logical round retries THAT round
    // rather than jumping to the summary. max_tool_rounds == 2: round 0 completes
    // a tool call; round 1 400s, recovers IN PLACE and does another tool call,
    // then the completed round advances to the summary — 4 requests. The buggy
    // loop would `continue` past round 1 straight into the summary — 3 requests.
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct ToolThen400ThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for ToolThen400ThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let tool = serde_json::json!({
                "model": "mock",
                "output": [{"type": "function_call", "name": "get_context_remaining",
                    "arguments": "{}", "call_id": "c1"}]
            });
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(200).set_body_json(tool),
                1 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
                })),
                2 => ResponseTemplate::new(200).set_body_json(tool),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "finished"}]}]
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ToolThen400ThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "FINAL ROUND: a 400 on the last round retries it, not the summary";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    ctx.max_tool_rounds = 2;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("the final round recovers in place and completes");
    assert_eq!(reply, "finished");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        4,
        "expected round0 tool, round1 400, round1 recovered tool, summary; a \
             3-request run means the 400 burned round 1 and jumped to the summary"
    );
}

/// #1528 B3 (B3-CG-003, item 1, reactive): tools rejected → tools-disabled retry
/// → a context-window 400 → REACTIVE recovery compacts the tools-disabled request
/// (no schema overhead reserved) and redispatches IN THE SAME round. req3 carries
/// no tools, is compacted, and completes.
#[tokio::test]
async fn responses_unsupported_tools_then_cw400_reactive_recovery_no_schema_overhead() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct UnsupportedThen400ThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UnsupportedThen400ThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "this model does not support tools"}
                })),
                1 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "recovered tools-disabled"}]}]
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(UnsupportedThen400ThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "summarize the history";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("tools-disabled cw-400 recovers in the same round and completes");
    assert_eq!(reply, "recovered tools-disabled");
    let reqs = server.received_requests().await.expect("request journal");
    assert_eq!(
        reqs.len(),
        3,
        "req1 tools rejected, req2 tools-disabled 400, req3 tools-disabled recovered"
    );
    let b3: serde_json::Value = serde_json::from_slice(&reqs[2].body).unwrap_or_default();
    assert!(
        b3["tools"].is_null(),
        "the recovered request is tools-disabled"
    );
    assert!(
        b3["input"].to_string().contains("newt-compaction-summary"),
        "the recovered tools-disabled request was compacted"
    );
}
