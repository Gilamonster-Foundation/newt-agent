use super::*;

/// A completed-spill viewport painted by a tool result is DISMISSED by the
/// loop's hooks — round-top and cap-exit — and cannot outlive the turn
/// (the fn-exit guard discards on every path). Pins the load-bearing
/// erase-before-canonical-write discipline the call sites implement.
#[derive(Default)]
struct DismissTrackingRenderer {
    rendered: AtomicUsize,
    erased: AtomicUsize,
    active: std::sync::atomic::AtomicBool,
}

impl CompletedSpillRenderer for DismissTrackingRenderer {
    fn render_completed(&self, _output: &str, _width: usize, _max_height: usize) -> usize {
        self.rendered.fetch_add(1, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);
        3
    }
    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
    fn erase(&self) {
        self.erased.fetch_add(1, Ordering::SeqCst);
        self.active.store(false, Ordering::SeqCst);
    }
    fn discard(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn ollama_loop_dismisses_the_completed_viewport_it_painted() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaResponder {
            tool_rounds_served: served.clone(),
            final_answer: "done".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "do the thing",
        BackendKind::Ollama,
    );
    let renderer = std::sync::Arc::new(DismissTrackingRenderer::default());
    ctx.completed_spill_renderer = Some(renderer.clone());
    ctx.safe_context = None;

    let _ = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("turn completes");

    assert!(
        renderer.rendered.load(Ordering::SeqCst) > 0,
        "the wired renderer painted for tool results"
    );
    assert!(
        renderer.erased.load(Ordering::SeqCst) > 0,
        "the loop's dismiss hooks erased the viewport"
    );
    assert!(
        !renderer.is_active(),
        "no viewport survives the turn (round-top / cap-exit / guard)"
    );
}

#[tokio::test]
async fn a_set_cancel_flag_abandons_the_turn_before_any_network_call() {
    // The interrupt checkpoint at the round-loop top runs before the first
    // request, so a pre-tripped flag returns instantly — the bogus URL
    // (a closed port) is never contacted. If the checkpoint regressed,
    // the dispatch would try to connect and this would not return empty.
    let messages = msgs();
    let caveats = Caveats::top();
    let flag = std::sync::atomic::AtomicBool::new(true);
    let (reply, streamed, usage, hallu) = chat_complete(
        ChatCtx {
            url: "http://127.0.0.1:1",
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
            task: "do the thing",
            workspace: ".",
            color: false,
            markdown: false,
            tool_offload: false,
            spill_store: None,
            disclosure: None,
            compaction_store: None,
            scratchpad: false,
            scratchpad_store: None,
            code_search: None,
            where_is: None,
            nav: None,
            exposure: Default::default(),
            experience_store: None,
            step_ledger: None,
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: 5,
            narration_nudge_cap: 1,
            action_nudges: true,
            prompt_disposition: PromptDisposition::Act,
            prompt_intake: None,
            workflow_grace_rounds: 0,
            tool_output_lines: 20,
            debug: false,
            trace: false,
            num_ctx: None,
            input_ceiling_pct: 80,
            low_budget_pct: 15,
            connect_timeout_secs: 5,
            inference_timeout_secs: 120,
            mid_loop_trim_threshold: 40,
            compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            phantom_reaches: None,
            end_reason: None,
            solve_obs: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: 8_192,
            rewrites_history: true,
            exec_floor: None,
            write_ledger: None,
            attribution: None,
            cancel: Some(&flag),
            live_tool_output: None,
            git_tool: None,
            crew_runner: None,
            operating_mode_control: None,
            plan_mode_control: None,
            steering: None,
            completed_spill_renderer: None,
        },
        &mut NoMcp,
    )
    .await
    .expect("an interrupted turn still returns Ok, just empty");
    assert!(reply.is_empty(), "interrupted before any model output");
    assert!(!streamed);
    assert!(usage.is_none());
    assert_eq!(hallu, 0);
}

/// Serves one tool call, and — simulating the operator typing WHILE that
/// request is on the wire — submits a steering message from inside the
/// responder. The next round's request body must carry it.
///
/// Submitting from the responder rather than pre-loading the queue is the
/// point: a pre-loaded queue would also pass if steering were only ever
/// drained once, before the turn started. This can only pass if the drain
/// really happens at each round boundary.
struct SteersMidTurn {
    inbox: std::sync::Arc<SessionSteeringInbox>,
    requests_seen: Arc<AtomicUsize>,
    steer: String,
    final_answer: String,
}

impl Respond for SteersMidTurn {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.requests_seen.fetch_add(1, Ordering::SeqCst) == 0 {
            self.inbox.submit(self.steer.clone());
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                    }]
                }
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": self.final_answer }
        }))
    }
}

/// Every user-role content string in an Ollama request body.
fn user_contents(req: &wiremock::Request) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
    body["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter()
                .filter(|m| m["role"] == "user")
                .filter_map(|m| m["content"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// #952/#1669: the operator's mid-turn correction reaches the model at the
/// NEXT round boundary — not at the round cap, and not never.
#[tokio::test]
async fn steering_submitted_mid_turn_appears_in_the_next_rounds_request() {
    const STEER: &str = "don't change the public API";
    let server = MockServer::start().await;
    let inbox = std::sync::Arc::new(SessionSteeringInbox::new());
    let seen = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SteersMidTurn {
            inbox: inbox.clone(),
            requests_seen: seen.clone(),
            steer: STEER.into(),
            final_answer: "acknowledged".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "do the thing",
        BackendKind::Ollama,
    );
    // `hard_budget_ctx` exists to prove the 256-token refusal; this test
    // is about round boundaries, so give it an ordinary budget.
    ctx.model = "test-model";
    ctx.safe_context = None;
    ctx.max_tool_rounds = 3;
    ctx.steering = Some(inbox.as_ref() as &dyn SteeringInbox);
    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("turn completes");
    assert_eq!(reply, "acknowledged");

    let reqs = server.received_requests().await.expect("journal");
    assert!(
        reqs.len() >= 2,
        "need a second round to observe the delivery, got {}",
        reqs.len()
    );
    assert!(
        !user_contents(&reqs[0]).iter().any(|c| c.contains(STEER)),
        "the steer did not exist yet when round 0 was sent"
    );
    assert!(
        user_contents(&reqs[1]).iter().any(|c| c.contains(STEER)),
        "round 1 must carry the operator's mid-turn correction; got {:?}",
        user_contents(&reqs[1])
    );
    assert_eq!(inbox.pending(), 0, "delivery consumes the queue");
}

/// The regression floor: with no inbox lent, nothing about the request
/// bodies changes. Every headless / eval caller passes `None`.
#[tokio::test]
async fn no_inbox_means_no_extra_user_messages() {
    let server = MockServer::start().await;
    let seen = Arc::new(AtomicUsize::new(0));
    let idle = std::sync::Arc::new(SessionSteeringInbox::new());
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SteersMidTurn {
            // Submits into an inbox the loop was never given.
            inbox: idle.clone(),
            requests_seen: seen.clone(),
            steer: "never delivered".into(),
            final_answer: "done".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "do the thing",
        BackendKind::Ollama,
    );
    ctx.model = "test-model";
    ctx.safe_context = None;
    ctx.max_tool_rounds = 3;
    ctx.steering = None;
    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("journal");
    for r in &reqs {
        assert!(
            !user_contents(r)
                .iter()
                .any(|c| c.contains("never delivered")),
            "a steer reached the model through an inbox that was never lent"
        );
    }
    assert_eq!(idle.pending(), 1, "it stayed queued, undelivered");
}

// -----------------------------------------------------------------------
// Read-only nudge injection test
//
// Scenario: model keeps calling list_dir (read-only) for 3 rounds.
// On round 4 the harness injects the nudge.  The responder detects the
// nudge text in the message list and returns a final text answer instead
// of another tool call, proving the nudge reached the model.
// -----------------------------------------------------------------------

struct ReadOnlyNudgeResponder {
    /// Flipped to true the first time the responder sees the nudge text.
    nudge_seen: Arc<std::sync::atomic::AtomicBool>,
}

impl Respond for ReadOnlyNudgeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default();
        let has_nudge = body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter().any(|m| {
                    m["content"]
                        .as_str()
                        .map(|c| c.contains("read-only rounds so far"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        if has_nudge {
            self.nudge_seen
                .store(true, std::sync::atomic::Ordering::SeqCst);
            // Return a plain text answer — no more tool calls.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "nudge received, writing file now" }
            }))
        } else if request_has_tools(req) {
            // Keep returning list_dir calls until the nudge arrives.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{ "function": {
                        "name": "list_dir",
                        "arguments": { "path": "." }
                    }}]
                }
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "final summary" }
            }))
        }
    }
}

#[tokio::test]
async fn read_only_nudge_injected_after_three_rounds() {
    let server = MockServer::start().await;
    let nudge_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ReadOnlyNudgeResponder {
            nudge_seen: nudge_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, _streamed, _usage, _hallu) = chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
            task: "list all files",
            workspace: ".",
            color: false,
            markdown: false,
            tool_offload: false,
            spill_store: None,
            disclosure: None,
            compaction_store: None,
            scratchpad: false,
            scratchpad_store: None,
            code_search: None,
            where_is: None,
            nav: None,
            exposure: Default::default(),
            experience_store: None,
            step_ledger: None,
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: 10,
            narration_nudge_cap: 1,
            action_nudges: true,
            prompt_disposition: PromptDisposition::Act,
            prompt_intake: None,
            workflow_grace_rounds: 0,
            tool_output_lines: 5,
            debug: false,
            trace: false,
            num_ctx: None,
            input_ceiling_pct: 80,
            low_budget_pct: 15,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            phantom_reaches: None,
            end_reason: None,
            solve_obs: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: 8_192,
            rewrites_history: true,
            exec_floor: None,
            write_ledger: None,
            attribution: None,
            cancel: None,
            live_tool_output: None,
            git_tool: None,
            crew_runner: None,
            operating_mode_control: None,
            plan_mode_control: None,
            steering: None,
            completed_spill_renderer: None,
        },
        &mut NoMcp,
    )
    .await
    .expect("chat_complete should succeed");

    assert!(
        nudge_seen.load(std::sync::atomic::Ordering::SeqCst),
        "nudge was never injected after 3 consecutive read-only rounds"
    );
    assert_eq!(
        reply, "nudge received, writing file now",
        "model should have responded to the nudge with a final answer"
    );
}
