use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn ctx<'a>(server_uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    ChatCtx {
        overflow_retry: Default::default(),
        run_allowance: None,
        verify_outcomes: false,
        round_cap_hit: None,
        smart_harness: None,
        url: server_uri,
        model: "test-model",
        kind: BackendKind::Ollama,
        api_key: None,
        messages,
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
        caveats,
        persona_tools: None,
        cognition: None,
        chat_completions_capability: Default::default(),
        responses_capability: Default::default(),
        openai_api: Default::default(),
        output_allowance: None,
        attempt_ledger: None,
        reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
        emits_leading_reasoning: false,
        max_tool_rounds: 8,
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
        // #307: test ChatCtx carries no preset exec floor (headless default).
        exec_floor: None,
        write_ledger: None,
        attribution: None,
        cancel: None,
        live_tool_output: None,
        git_tool: None,
        crew_runner: None,
        operating_mode_control: None,
        plan_mode_control: None,
        plan_draft_sink: None,
        steering: None,
        completed_spill_renderer: None,
    }
}

/// Set a hard gate immediately above the live initial wire request. The
/// following tool result must then exercise the preflight refusal rather
/// than making this regression depend on a frozen catalog size.
fn initial_request_budget(messages: &[MemMessage], task: &str) -> usize {
    let tools = merged_tool_definitions(
        &NoMcp, false, false, false, None, false, false, false, false, false, false, false, false,
    );
    let mut wire_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|message| {
                serde_json::json!({"role": message.role.as_str(), "content": message.content})
            })
            .collect();
    let receipt = crate::TurnPromptContext::ephemeral_operator(
        "ephemeral-headless",
        task.as_bytes().to_vec(),
        task.as_bytes().to_vec(),
    );
    prompt_read::ensure_active_prompt_card(
        &mut wire_messages,
        prompt_read::PromptReadContext::new(Some(&receipt), task, None),
        None,
    );
    estimate_request_tokens(
        &wire_messages,
        Some(&tools),
        crate::tokens::TokenEstimation::default(),
    )
    .saturating_add(1)
}

fn body_json(req: &Request) -> serde_json::Value {
    serde_json::from_slice(&req.body).unwrap_or_default()
}

fn is_stream(req: &Request) -> bool {
    body_json(req)["stream"].as_bool().unwrap_or(false)
}

fn ndjson(lines: &[serde_json::Value]) -> ResponseTemplate {
    let body: String = lines
        .iter()
        .map(|l| format!("{l}\n"))
        .collect::<Vec<_>>()
        .join("");
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
}

/// Tool calls for the first two tools-offering requests (each reporting
/// the backend ACCEPTED an 8,734-token prompt), then a final answer.
struct AcceptsLargePrompts {
    tools_rounds: Arc<AtomicUsize>,
}
impl Respond for AcceptsLargePrompts {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            return ndjson(&[serde_json::json!({
                "message": {"content": "budget raised, here is the answer"},
                "done": true, "prompt_eval_count": 8_700, "eval_count": 12
            })]);
        }
        let n = self.tools_rounds.fetch_add(1, Ordering::SeqCst);
        if n < 2 {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]},
                "prompt_eval_count": 8_734, "eval_count": 10,
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "budget raised, here is the answer"},
                "prompt_eval_count": 8_700, "eval_count": 12,
            }))
        }
    }
}

/// THE trace-class regression (the motivating failure): a poisoned-low
/// `max_ok_input` (the largest prompt SEEN, not accepted) used to refuse
/// sends the backend was happily evaluating. Now: the over-budget
/// acceptance (a) reaches the caller as an `Accepted` observation with
/// the backend's real prompt size, and (b) raises the in-turn send
/// budget, so the turn completes instead of latching anti-thrash into
/// the Refused bail across the following rounds.
#[tokio::test]
async fn poisoned_low_budget_recovers_via_accepted_observation_and_raise() {
    let server = MockServer::start().await;
    let tools_rounds = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(AcceptsLargePrompts {
            tools_rounds: tools_rounds.clone(),
        })
        .mount(&server)
        .await;

    // A task big enough (~12k chars ≈ 3k est. tokens) to sit over the
    // poisoned 2,000-token budget but far under what the backend accepts.
    let big_task = "study the workspace and report. ".repeat(380);
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user(&big_task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_ok_input = Some(2_000); // the poisoned ratchet
    c.on_round_usage = Some(&mut hook);
    let (reply, _streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the turn must complete — no Refused bail after the raise");

    assert_eq!(reply, "budget raised, here is the answer");
    assert!(
        observations.iter().any(|o| matches!(
            o,
            RoundObservation::Accepted {
                prompt_tokens: 8_734,
                ..
            }
        )),
        "the accepted 8,734-token prompt must reach the hook: {observations:?}"
    );
    // Every accepted round carried a non-zero chars/4 estimate for
    // calibration pairing.
    for o in &observations {
        if let RoundObservation::Accepted {
            estimated_tokens, ..
        } = o
        {
            assert!(*estimated_tokens > 0, "estimate rides along: {o:?}");
        }
    }
}

/// Always tool calls (with usage) — drives the anti-thrash latch under an
/// unreachable hard token budget so the turn ends in the Refused Err.
struct ToolCallsWithUsage;
impl Respond for ToolCallsWithUsage {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]},
                "prompt_eval_count": 14_000, "eval_count": 5,
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": "cap exit"}}))
        }
    }
}

/// A turn that ends `Err` at the authoritative full-request preflight
/// STILL delivered the earlier round's `Accepted` observation first —
/// evidence at the moment of observation, not in an epilogue the error
/// skips (the spec's headline property).
#[tokio::test]
async fn err_turn_still_delivered_accepted_observations_first() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ToolCallsWithUsage)
        .mount(&server)
        .await;

    // Incompressible context + hard token budget: the initial request is
    // valid and accepted, then its fresh result makes the follow-up
    // impossible. The full gate refuses that follow-up before the wire.
    let messages = vec![
        MemMessage::system(format!("you are a test. {}", "rule. ".repeat(7_000))),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut compress_state = CompressState::new();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.mid_loop_trim_tokens = Some(initial_request_budget(&messages, "do the thing"));
    c.compress_state = Some(&mut compress_state);
    c.on_round_usage = Some(&mut hook);
    let err = chat_complete(c, &mut NoMcp)
        .await
        .expect_err("the known-over-budget follow-up must refuse the send");

    let msg = err.to_string();
    assert!(msg.contains("complete inference request needs"), "{msg}");
    assert!(msg.contains("tool results were not truncated"), "{msg}");
    assert!(
        observations.iter().any(|o| matches!(
            o,
            RoundObservation::Accepted {
                prompt_tokens: 14_000,
                ..
            }
        )),
        "accepted rounds before the bail must have been reported: {observations:?}"
    );
}

/// Probe 1: thinking-only (empty content, non-empty `thinking`, generated
/// tokens); the corrective retry then recovers. The hook must see exactly
/// one `ThinkingOnly` (once per turn) plus the recovery's `Accepted`.
struct ThinkingOnlyThenRecover {
    probes: Arc<AtomicUsize>,
}
impl Respond for ThinkingOnlyThenRecover {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            if self.probes.load(Ordering::SeqCst) <= 1 {
                ndjson(&[serde_json::json!({
                    "message": {"content": ""}, "done": true,
                    "prompt_eval_count": 9, "eval_count": 4
                })])
            } else {
                ndjson(&[serde_json::json!({
                    "message": {"content": "recovered after thinking-only"},
                    "done": true, "prompt_eval_count": 12, "eval_count": 3
                })])
            }
        } else {
            let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "thinking": "all reasoning, no final text"
                    },
                    "prompt_eval_count": 10, "eval_count": 2559,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered after thinking-only"},
                    "prompt_eval_count": 12, "eval_count": 3,
                }))
            }
        }
    }
}

#[tokio::test]
async fn thinking_only_response_emits_one_thinking_only_observation() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ThinkingOnlyThenRecover {
            probes: probes.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.on_round_usage = Some(&mut hook);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the corrective retry recovers the turn");

    assert_eq!(reply, "recovered after thinking-only");
    let thinking = observations
        .iter()
        .filter(|o| matches!(o, RoundObservation::ThinkingOnly))
        .count();
    assert_eq!(thinking, 1, "exactly once per turn: {observations:?}");
    assert!(
        observations
            .iter()
            .any(|o| matches!(o, RoundObservation::Accepted { .. })),
        "the recovered round is usable output: {observations:?}"
    );
}

/// Tool round + final round both reporting a prompt at ≥95% of the
/// request's `num_ctx` — Ollama may have silently dropped the head, so
/// the rounds are window evidence of NOTHING: no `Accepted` observation,
/// no budget raise.
struct TruncationSuspectResponder {
    tools_rounds: Arc<AtomicUsize>,
    /// Reported prompt size for every round — set ≥95% of the request's
    /// `num_ctx` so each round reads as truncation-suspect.
    suspect_prompt: u32,
}
impl Respond for TruncationSuspectResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let suspect_prompt = self.suspect_prompt;
        if is_stream(req) {
            return ndjson(&[serde_json::json!({
                "message": {"content": "suspect answer"}, "done": true,
                "prompt_eval_count": suspect_prompt, "eval_count": 5
            })]);
        }
        let n = self.tools_rounds.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]},
                // ≥95% of num_ctx — truncation suspect.
                "prompt_eval_count": suspect_prompt, "eval_count": 5,
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "suspect answer"},
                "prompt_eval_count": suspect_prompt, "eval_count": 5,
            }))
        }
    }
}

#[tokio::test]
async fn truncation_suspect_rounds_emit_nothing() {
    let server = MockServer::start().await;
    // Derive the window from the live catalog. The exact prompt + schemas
    // must fit the input ceiling (input_ceiling_pct% of num_ctx), so reserve
    // ~311 tokens of headroom above the catalog (a catalog-INDEPENDENT
    // figure for the tiny system/card/user messages) and back out num_ctx.
    // The reported prompt is then pinned at ≥95% of that num_ctx, so every
    // round stays truncation-suspect no matter how the catalog grows.
    // (Reproduces the historical 5,120 num_ctx / 4,096 ceiling / ~5,000
    // report at today's catalog size.)
    const INPUT_CEILING_PCT: usize = 80; // matches ctx() default below
    let input_ceiling = builtin_catalog_tokens(PromptDisposition::Act)
        + prompt_read::response_repository_policy_tokens()
        + 311;
    let num_ctx = (input_ceiling * 100).div_ceil(INPUT_CEILING_PCT) as u32;
    let suspect_prompt = num_ctx * 98 / 100; // ≥95% of num_ctx → suspect
    let tools_rounds = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(TruncationSuspectResponder {
            tools_rounds: tools_rounds.clone(),
            suspect_prompt,
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    assert_eq!(
        c.input_ceiling_pct as usize, INPUT_CEILING_PCT,
        "derived num_ctx assumes the ctx() input-ceiling percentage"
    );
    c.num_ctx = Some(num_ctx);
    c.on_round_usage = Some(&mut hook);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("suspect rounds still complete the turn");

    assert_eq!(reply, "suspect answer");
    assert!(
        observations.is_empty(),
        "a possibly head-truncated prompt is evidence of nothing: \
             {observations:?}"
    );
}

/// OpenAI-path mirror: tool round then final content, both with usage —
/// the hook receives `Accepted` for both (no `num_ctx` on this wire, so
/// no truncation gate), and an absent hook stays a no-op.
struct OpenAiAcceptsResponder;
impl Respond for OpenAiAcceptsResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some()
            && !body_json(req)["messages"]
                .as_array()
                .map(|m| m.iter().any(|x| x["role"] == "tool"))
                .unwrap_or(false)
        {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                    }]
                }}],
                "usage": {"prompt_tokens": 5_120, "completion_tokens": 9},
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "openai accepted"}}],
                "usage": {"prompt_tokens": 5_200, "completion_tokens": 11},
            }))
        }
    }
}

#[tokio::test]
async fn openai_loop_reports_accepted_rounds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiAcceptsResponder)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    c.on_round_usage = Some(&mut hook);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai loop should succeed");

    assert_eq!(reply, "openai accepted");
    let accepted: Vec<u32> = observations
        .iter()
        .filter_map(|o| match o {
            RoundObservation::Accepted { prompt_tokens, .. } => Some(*prompt_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(
        accepted,
        vec![5_120, 5_200],
        "both usable rounds reported, in order: {observations:?}"
    );
}

/// Persistent empties (probe AND stream return empty content, no tool
/// calls) at a prompt ≥85% of the configured `safe_context`, with no
/// generated tokens — so the suspicious-empty corrective retry is NOT
/// taken (that path needs `eval_count > 0`). The loop exhausts its two
/// `overflow_retries`, then on the next persistent empty falls through to
/// the silent-overflow exit and must emit exactly one
/// `SuspectedOverflow { prompt_tokens }` carrying the merged (largest
/// single) prompt size — the loop-emission seam that the dispatch-seam
/// `record_overflow` tests at probe.rs cannot reach.
struct PersistentEmptyOverflow;
impl Respond for PersistentEmptyOverflow {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if is_stream(_req) {
            // Stream re-issue: empty content, no tokens generated, but the
            // round still reports a large evaluated prompt.
            return ndjson(&[serde_json::json!({
                "message": {"content": ""}, "done": true,
                "prompt_eval_count": 8_734, "eval_count": 0
            })]);
        }
        // Probe (non-stream): empty content, no tool calls, no generated
        // tokens, large evaluated prompt.
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": ""},
            "prompt_eval_count": 8_734, "eval_count": 0,
        }))
    }
}

#[tokio::test]
async fn persistent_empty_over_safe_context_emits_suspected_overflow() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(PersistentEmptyOverflow)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    // Derive the window from the live catalog: catalog weight plus ~215
    // tokens (a catalog-INDEPENDENT offset covering the tiny system/card/
    // user messages plus headroom) so the exact request keeps fitting as
    // the catalog grows. The reported 8_734-token prompt stays far above
    // 85% of this window, so the silent-overflow gate still fires.
    // (Reproduces the historical 4_000 at today's catalog size.)
    c.safe_context = Some(
        (builtin_catalog_tokens(PromptDisposition::Act)
            + prompt_read::response_repository_policy_tokens()
            + 215) as u32,
    );
    c.on_round_usage = Some(&mut hook);
    let (_reply, streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("persistent empties return the empty-response message, not Err");

    // Diagnostic exit returns non-streamed placeholder text.
    assert!(
        !streamed,
        "the silent-overflow exit is not a streamed reply"
    );
    // Exactly one SuspectedOverflow, carrying the merged (largest single)
    // prompt size — emitted once at the exit, never per retry.
    let overflow: Vec<u32> = observations
        .iter()
        .filter_map(|o| match o {
            RoundObservation::SuspectedOverflow { prompt_tokens } => Some(*prompt_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(
        overflow,
        vec![8_734],
        "one SuspectedOverflow at the merged prompt size: {observations:?}"
    );
    // No Accepted: empty content is never usable output, so the window
    // evidence must not ratchet a success.
    assert!(
        !observations
            .iter()
            .any(|o| matches!(o, RoundObservation::Accepted { .. })),
        "empty rounds are not Accepted evidence: {observations:?}"
    );
}

// ---------------------------------------------------------------------
// #1528 B4 — accepted-round usage observations (Phase 3). The emit rules:
// an `Accepted` observation is reported ONLY for (a) completed usable text
// or (b) a FULLY-VALIDATED tool-call batch (after whole-batch validation,
// before the first tool side effect); NEVER for a content-invalid or
// correlation-impossible batch, an empty response, or a round the backend
// reported no usage for. A collecting hook records every observation.
// ---------------------------------------------------------------------

fn accepted_prompts(observations: &[RoundObservation]) -> Vec<u32> {
    observations
        .iter()
        .filter_map(|o| match o {
            RoundObservation::Accepted { prompt_tokens, .. } => Some(*prompt_tokens),
            _ => None,
        })
        .collect()
}

/// B4 rule (a): a single completed-usable-text round emits EXACTLY one
/// `Accepted`, carrying the backend's reported prompt size.
struct OllamaTextOnce {
    prompt: u32,
}
impl Respond for OllamaTextOnce {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let p = self.prompt;
        if is_stream(req) {
            return ndjson(&[serde_json::json!({
                "message": {"content": "final answer ready"}, "done": true,
                "prompt_eval_count": p, "eval_count": 4
            })]);
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": "final answer ready"},
            "prompt_eval_count": p, "eval_count": 4,
        }))
    }
}

#[tokio::test]
async fn accepted_text_emits_exactly_one_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaTextOnce { prompt: 5_000 })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.on_round_usage = Some(&mut hook);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("a usable-text turn completes");

    assert_eq!(reply, "final answer ready");
    assert_eq!(
        accepted_prompts(&observations),
        vec![5_000],
        "exactly one Accepted for one usable-text response: {observations:?}"
    );
}

/// B4 rules (b) + "a later tool-execution FAILURE does not erase the
/// provider-accept evidence": a WELL-FORMED tool batch (valid name + object
/// args) is validated, so exactly one `Accepted` is emitted for that round —
/// and it STILL stands after the tool call then fails at execution (the tool
/// does not exist). The following round's final text emits its own single
/// `Accepted`, proving at-most-one per response across the two rounds.
struct OllamaValidToolThenText {
    probes: Arc<AtomicUsize>,
}
impl Respond for OllamaValidToolThenText {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            return ndjson(&[serde_json::json!({
                "message": {"content": "all done"}, "done": true,
                "prompt_eval_count": 5_200, "eval_count": 3
            })]);
        }
        let n = self.probes.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // A structurally VALID call to a tool that does not exist: the
            // batch validates (name present, object args), so it is accept
            // evidence; execution then fails with an unknown-tool result.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]},
                "prompt_eval_count": 6_000, "eval_count": 5,
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "all done"},
                "prompt_eval_count": 5_200, "eval_count": 3,
            }))
        }
    }
}

#[tokio::test]
async fn validated_tool_calls_emit_one_accepted_and_survive_execution_failure() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaValidToolThenText {
            probes: probes.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.on_round_usage = Some(&mut hook);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the tool round then the final answer complete the turn");

    assert_eq!(reply, "all done");
    // One Accepted for the validated tool round (6_000) and one for the
    // final text (5_200) — at most one per response, and the tool round's
    // Accepted survives the unknown-tool execution failure.
    assert_eq!(
        accepted_prompts(&observations),
        vec![6_000, 5_200],
        "validated tool round + final text each emit exactly one Accepted: {observations:?}"
    );
}

/// #2313 (b1a): every primary Ollama request is exactly one ledger attempt,
/// keyed by its exact wire bytes. The only direct proof against double
/// counting and silent drops: attempts == the requests the server received on
/// this wire's generation path (`/api/chat`), and each attempt's request id is
/// the `RawContentId` of one received body. Nothing else may be hit, so the
/// filter cannot hide an extra inference call.
///
/// Scope of the count: the Ollama primary loop's probe and cap-exit summary. Summarizer, auxiliary and other wires are not yet wired.
#[tokio::test]
async fn every_ollama_request_is_one_ledger_attempt_keyed_by_its_wire_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaValidToolThenText {
            probes: Arc::new(AtomicUsize::new(0)),
        })
        .mount(&server)
        .await;
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the tool round then the final answer complete the turn");
    assert_eq!(reply, "all done");

    let received = server.received_requests().await.expect("journal");
    let generation: Vec<_> = received
        .iter()
        .filter(|request| request.url.path() == "/api/chat")
        .collect();
    assert_eq!(
        generation.len(),
        received.len(),
        "only the generation path may be hit: {:?}",
        received.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
    assert_eq!(generation.len(), 2, "tool probe, answer probe (#2372)");

    let ledger = ledger.lock().unwrap();
    let records: Vec<_> = ledger.records().collect();
    assert_eq!(records.len(), generation.len(), "attempts == wire requests");
    let mut wire: Vec<_> = generation
        .iter()
        .map(|request| content_addressable::RawContentId::from_content(&request.body))
        .collect();
    let mut recorded: Vec<_> = records.iter().map(|record| record.key.request).collect();
    wire.sort();
    recorded.sort();
    assert_eq!(recorded, wire, "each attempt is keyed by one received body");
    let turn = &records[0].key.turn;
    assert!(turn.starts_with("prompt:"), "{turn}");
    for record in &records {
        assert_eq!(&record.key.turn, turn);
        assert_eq!(record.key.role, "primary");
        assert_eq!(record.state, crate::attempts::AttemptState::Ok);
    }
    let totals = ledger.totals();
    assert_eq!(
        (totals.in_tokens, totals.out_tokens, totals.usage_complete),
        (6_000 + 5_200, 5 + 3, true)
    );
}

/// Tool call on every tools-bearing request; a plain answer (with usage) for
/// the tools-disabled cap-exit summary.
struct OllamaToolThenCapSummary;
impl Respond for OllamaToolThenCapSummary {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        if body.get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]},
                "prompt_eval_count": 900, "eval_count": 4,
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "capped summary"},
                "prompt_eval_count": 950, "eval_count": 6,
            }))
        }
    }
}

/// #2313 (b1a): the Ollama cap-exit summary — sent through the shared
/// `dispatch_with_decoder` — is one attempt too, so attempts still equal the
/// requests on `/api/chat` when the round cap ends the turn.
#[tokio::test]
async fn an_ollama_cap_exit_summary_is_one_ledger_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaToolThenCapSummary)
        .mount(&server)
        .await;
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 1;
    c.attempt_ledger = Some(&ledger);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the round cap ends the turn with a summary");
    assert!(reply.contains("capped summary"), "{reply}");

    let received = server.received_requests().await.expect("journal");
    assert!(
        received.iter().all(|r| r.url.path() == "/api/chat"),
        "only the generation path may be hit"
    );
    let summaries = received
        .iter()
        .filter(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body)
                .is_ok_and(|body| body.get("tools").is_none())
        })
        .count();
    assert_eq!(summaries, 1, "exactly one tools-disabled cap-exit summary");
    let ledger = ledger.lock().unwrap();
    let mut wire: Vec<_> = received
        .iter()
        .map(|r| content_addressable::RawContentId::from_content(&r.body))
        .collect();
    let mut recorded: Vec<_> = ledger.records().map(|r| r.key.request).collect();
    wire.sort();
    recorded.sort();
    assert_eq!(
        recorded, wire,
        "attempts == wire requests, keyed by their bodies"
    );
    assert!(ledger
        .records()
        .all(|r| r.state == crate::attempts::AttemptState::Ok && r.usage.is_some()));
}

/// Claims run_command cannot run until the loop's grounding nudge arrives,
/// then answers. Each round's probe reports its own usage.
struct OllamaBlockerThenAnswer;
impl Respond for OllamaBlockerThenAnswer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let nudged = body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with(crate::agentic::compress::LOOP_GUIDANCE_PREFIX))
            })
        });
        let (content, input) = if nudged {
            ("all done", 350)
        } else {
            ("run_command cannot run in this sandbox", 300)
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": content},
            "prompt_eval_count": input, "eval_count": 5,
        }))
    }
}

/// #2313 (b2): when the grounding nudge rejects a blocker claim, the answer it
/// rejected was still generated, so its usage joins the turn.
#[tokio::test]
async fn a_grounding_nudge_keeps_the_rejected_answers_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaBlockerThenAnswer)
        .mount(&server)
        .await;
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("run the test suite"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    let (reply, _, usage, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the nudged turn completes");
    assert_eq!(reply, "all done");
    let received = server.received_requests().await.expect("journal");
    assert_eq!(received.len(), 2, "one generation per round (#2372)");
    let totals = ledger.lock().unwrap().totals();
    assert_eq!((totals.attempts, totals.out_tokens), (2, 5 + 5));
    assert_eq!(
        usage.map(|u| u.output_tokens),
        Some(5 + 5),
        "the rejected answer's 5 generated tokens join the turn"
    );
}

/// B4 rule: a CONTENT-INVALID tool batch (RR1) — here a call with no name —
/// is NOT usable output, so NO `Accepted` is emitted for that round; the
/// loop echoes the rejection and re-dispatches, and only the following valid
/// text round is accepted. FAILS on the pre-fix code, which emitted
/// `Accepted` BEFORE validating the batch.
struct OllamaMalformedThenText {
    probes: Arc<AtomicUsize>,
}
impl Respond for OllamaMalformedThenText {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            return ndjson(&[serde_json::json!({
                "message": {"content": "recovered answer"}, "done": true,
                "prompt_eval_count": 5_200, "eval_count": 3
            })]);
        }
        let n = self.probes.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Malformed: a tool call with NO name → BatchRejection::ContentInvalid.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"arguments": {}}
                }]},
                "prompt_eval_count": 6_000, "eval_count": 5,
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "recovered answer"},
                "prompt_eval_count": 5_200, "eval_count": 3,
            }))
        }
    }
}

#[tokio::test]
async fn content_invalid_tool_batch_emits_no_accepted() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaMalformedThenText {
            probes: probes.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.on_round_usage = Some(&mut hook);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the rejected batch re-dispatches to a valid answer");

    assert_eq!(reply, "recovered answer");
    let accepted = accepted_prompts(&observations);
    assert!(
        !accepted.contains(&6_000),
        "a content-invalid batch is NOT accept evidence (would fire pre-fix): {observations:?}"
    );
    assert_eq!(
        accepted,
        vec![5_200],
        "only the re-dispatched valid text round is accepted: {observations:?}"
    );
}

/// OpenAI mirror: a CONTENT-INVALID batch (valid unique id, missing name →
/// RR1) emits NO `Accepted`; the loop echoes a keyed rejection and
/// re-dispatches. FAILS on the pre-fix code.
struct OpenAiMalformedThenText;
impl Respond for OpenAiMalformedThenText {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let has_tool_result = body_json(req)["messages"]
            .as_array()
            .map(|m| m.iter().any(|x| x["role"] == "tool"))
            .unwrap_or(false);
        if has_tool_result {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "recovered answer"}}],
                "usage": {"prompt_tokens": 5_200, "completion_tokens": 4},
            }))
        } else {
            // Valid unique id, but the call has no name → ContentInvalid.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {"arguments": "{}"}
                    }]
                }}],
                "usage": {"prompt_tokens": 6_000, "completion_tokens": 5},
            }))
        }
    }
}

#[tokio::test]
async fn openai_content_invalid_tool_batch_emits_no_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiMalformedThenText)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    c.on_round_usage = Some(&mut hook);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the rejected batch re-dispatches to a valid answer");

    assert_eq!(reply, "recovered answer");
    let accepted = accepted_prompts(&observations);
    assert!(
        !accepted.contains(&6_000),
        "a content-invalid batch is NOT accept evidence (would fire pre-fix): {observations:?}"
    );
    assert_eq!(
        accepted,
        vec![5_200],
        "only the valid round is accepted: {observations:?}"
    );
}

/// #2558/F39: a tool call whose `arguments` string is TRUNCATED/INVALID JSON
/// (e.g. a large embedded script cut off mid-generation) must not poison the
/// replayed history. FAILS on the pre-fix code, which pushed the raw invalid
/// string into `messages` before validating the batch — so every later
/// request (including a token-count preflight) carried unparseable JSON.
struct OpenAiTruncatedArgsThenText;
impl Respond for OpenAiTruncatedArgsThenText {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let has_tool_result = body_json(req)["messages"]
            .as_array()
            .map(|m| m.iter().any(|x| x["role"] == "tool"))
            .unwrap_or(false);
        if has_tool_result {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "recovered answer"}}],
                "usage": {"prompt_tokens": 5_200, "completion_tokens": 4},
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1", "type": "function",
                        "function": {
                            "name": "run_command",
                            "arguments": "{\"cmd\": \"python -c 'a huge script"
                        }
                    }]
                }}],
                "usage": {"prompt_tokens": 6_000, "completion_tokens": 5},
            }))
        }
    }
}

#[tokio::test]
async fn openai_truncated_tool_args_never_reach_a_later_request_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiTruncatedArgsThenText)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the rejected batch re-dispatches to a valid answer");

    assert_eq!(reply, "recovered answer");
    let received = server.received_requests().await.expect("journal");
    assert_eq!(received.len(), 2, "one generation per round");
    let second_body = body_json(&received[1]);
    let raw = second_body.to_string();
    assert!(
        !raw.contains("a huge script"),
        "the truncated raw arguments must not survive into a later request body: {raw}"
    );
    assert!(
        raw.contains("not valid JSON"),
        "the model must be told its arguments were rejected: {raw}"
    );
}

/// OpenAI RR2: a CORRELATION-IMPOSSIBLE batch (duplicate `tool_call_id`)
/// aborts the turn with an error and emits NO `Accepted` — a mis-routable
/// batch is never provider-accept evidence. FAILS on the pre-fix code, which
/// emitted `Accepted` before the correlation check.
struct OpenAiDuplicateId;
impl Respond for OpenAiDuplicateId {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [
                    {"id": "dup", "type": "function",
                     "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}},
                    {"id": "dup", "type": "function",
                     "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}}
                ]
            }}],
            "usage": {"prompt_tokens": 6_000, "completion_tokens": 5},
        }))
    }
}

#[tokio::test]
async fn openai_correlation_impossible_duplicate_id_emits_no_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiDuplicateId)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    c.on_round_usage = Some(&mut hook);
    let err = chat_complete(c, &mut NoMcp)
        .await
        .expect_err("a duplicate call id aborts the turn");
    assert!(
        err.to_string().contains("malformed provider output"),
        "{err}"
    );
    assert!(
            accepted_prompts(&observations).is_empty(),
            "a correlation-impossible batch is never accept evidence (would fire pre-fix): {observations:?}"
        );
}

/// OpenAI RR2: a CORRELATION-IMPOSSIBLE batch (missing `tool_call_id`) aborts
/// the turn and emits NO `Accepted`. FAILS on the pre-fix code.
struct OpenAiMissingId;
impl Respond for OpenAiMissingId {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [{
                    "type": "function",
                    "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                }]
            }}],
            "usage": {"prompt_tokens": 6_000, "completion_tokens": 5},
        }))
    }
}

#[tokio::test]
async fn openai_correlation_impossible_missing_id_emits_no_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiMissingId)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    c.on_round_usage = Some(&mut hook);
    let err = chat_complete(c, &mut NoMcp)
        .await
        .expect_err("a missing call id aborts the turn");
    assert!(
        err.to_string().contains("malformed provider output"),
        "{err}"
    );
    assert!(
            accepted_prompts(&observations).is_empty(),
            "a correlation-impossible batch is never accept evidence (would fire pre-fix): {observations:?}"
        );
}

/// B4 rule: unknown/absent usage must not invent an exact measurement — a
/// round the backend reported NO usage for emits NO `Accepted`, even though
/// the text itself is usable.
struct OllamaTextNoUsage;
impl Respond for OllamaTextNoUsage {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            return ndjson(&[serde_json::json!({
                "message": {"content": "answer"}, "done": true
            })]);
        }
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"message": {"content": "answer"}}))
    }
}

#[tokio::test]
async fn none_usage_round_emits_no_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaTextNoUsage)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observations: Vec<RoundObservation> = Vec::new();
    let mut hook = |obs: RoundObservation| observations.push(obs);
    let mut c = ctx(&uri, &messages, &caveats);
    c.on_round_usage = Some(&mut hook);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("a usable-text turn with no usage still completes");

    assert_eq!(reply, "answer");
    assert!(
        accepted_prompts(&observations).is_empty(),
        "no usage → no invented measurement, hence no Accepted: {observations:?}"
    );
}

/// #2313 (c): Esc while the Ollama probe waits for its response drops the
/// dispatch future, so nothing can settle the attempt; the handle's drop records
/// it cancelled, with no usage.
#[tokio::test]
async fn an_interrupted_ollama_probe_is_a_cancelled_attempt() {
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (url, server) =
        super::http_loop_tests::serve_until_interrupted("/api/chat", flag.clone()).await;
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("answer me"),
    ];
    let caveats = Caveats::top();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&url, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    c.cancel = Some(flag.as_ref());
    let _ = tokio::time::timeout(
        super::http_loop_tests::RAW_STREAM_TEST_BOUND,
        chat_complete(c, &mut NoMcp),
    )
    .await
    .expect("the interrupt ends the turn");
    server.abort();
    let records: Vec<_> = ledger.lock().unwrap().records().cloned().collect();
    assert_eq!(records.len(), 1);
    assert_eq!(
        (records[0].state, records[0].usage),
        (crate::attempts::AttemptState::Cancelled, None)
    );
}

// ---------------------------------------------------------------------------
// A tool-call batch with no call id is re-asked, not fatal (P0 U3).
// ---------------------------------------------------------------------------

/// Serves `script` in order (last entry repeats) and records every request body.
struct ScriptedChat {
    script: Vec<serde_json::Value>,
    bodies: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}
impl Respond for ScriptedChat {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let mut bodies = self.bodies.lock().unwrap();
        let i = bodies.len().min(self.script.len() - 1);
        bodies.push(serde_json::from_slice(&req.body).expect("JSON request"));
        ResponseTemplate::new(200).set_body_json(self.script[i].clone())
    }
}

fn idless_call() -> serde_json::Value {
    serde_json::json!({
        "choices": [{"message": {"content": null, "tool_calls": [{
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"no/such/file\"}"}
        }]}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 5},
    })
}

fn call_with_id() -> serde_json::Value {
    serde_json::json!({
        "choices": [{"message": {"content": null, "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"no/such/file\"}"}
        }]}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 5},
    })
}

fn final_answer() -> serde_json::Value {
    serde_json::json!({
        "choices": [{"message": {"content": "all done"}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 5},
    })
}

/// Runs the chat-completions loop against `script`; returns the result, the
/// recorded request bodies, and how many `read_file` tool events ran.
async fn run_scripted(
    script: Vec<serde_json::Value>,
) -> (anyhow::Result<String>, Vec<serde_json::Value>, usize, usize) {
    let server = MockServer::start().await;
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedChat {
            script,
            bodies: bodies.clone(),
        })
        .mount(&server)
        .await;
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut events: Vec<crate::ToolEvent> = Vec::new();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    c.tool_events = Some(&mut events);
    let result = chat_complete(c, &mut NoMcp).await.map(|(reply, ..)| reply);
    let ran = events.iter().filter(|e| e.tool == "read_file").count();
    let rejected = events
        .iter()
        .filter(|e| e.tool == "(rejected tool-call batch)" && !e.ok)
        .count();
    let bodies = bodies.lock().unwrap().clone();
    (result, bodies, ran, rejected)
}

/// The id-less batch dispatches nothing and is re-asked; the next, well-formed
/// batch runs once and the turn completes.
#[tokio::test]
async fn idless_tool_call_is_re_asked_and_the_turn_completes() {
    let (result, bodies, ran, rejected) =
        run_scripted(vec![idless_call(), call_with_id(), final_answer()]).await;
    assert_eq!(result.expect("the turn completes"), "all done");
    assert_eq!(bodies.len(), 3, "re-ask, tool result, final");
    assert_eq!(ran, 1, "only the well-formed batch ran a tool");
    assert_eq!(
        rejected, 1,
        "the trace records why the re-ask round produced nothing"
    );
}

/// Three id-less batches in a row abort exactly as before.
#[tokio::test]
async fn three_idless_batches_in_a_row_abort_with_todays_error() {
    let (result, bodies, ran, _) = run_scripted(vec![idless_call()]).await;
    let err = result.expect_err("the budget is spent");
    assert!(
        err.to_string().contains("malformed provider output")
            && err.to_string().contains("missing a call id"),
        "{err}"
    );
    assert_eq!(bodies.len(), 3, "the third id-less batch aborts");
    assert_eq!(ran, 0, "nothing was dispatched");
}

/// A well-formed batch resets the budget: two id-less, one good, two more
/// id-less, then an answer completes.
#[tokio::test]
async fn a_well_formed_batch_resets_the_idless_budget() {
    let (result, _, ran, _) = run_scripted(vec![
        idless_call(),
        idless_call(),
        call_with_id(),
        idless_call(),
        idless_call(),
        final_answer(),
    ])
    .await;
    assert_eq!(result.expect("completes"), "all done");
    assert_eq!(ran, 1);
}

/// The re-ask text reaches the next request as a plain user message, and the
/// id-less calls are not replayed on the assistant turn.
#[tokio::test]
async fn the_re_ask_reaches_the_next_request_body() {
    let (_, bodies, _, _) = run_scripted(vec![idless_call(), call_with_id(), final_answer()]).await;
    let second = bodies[1]["messages"].as_array().expect("messages");
    let last = second.last().unwrap();
    assert_eq!(last["role"], "user", "a user message, not a tool result");
    assert!(
        last["content"]
            .as_str()
            .unwrap()
            .contains("could not be correlated"),
        "{last}"
    );
    assert!(
        second
            .iter()
            .all(|m| m["role"] != "assistant" || m.get("tool_calls").is_none()),
        "id-less calls must not be replayed: {second:?}"
    );
    assert!(second.iter().all(|m| m["role"] != "tool"));
    // The default replay sends a tool-only assistant turn as `content: ""`;
    // strict gateways reject that, so the withdrawn turn must carry text.
    let withdrawn = second
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .expect("the withdrawn assistant turn is still replayed");
    assert!(
        withdrawn["content"]
            .as_str()
            .is_some_and(|c| !c.trim().is_empty()),
        "empty assistant content on the wire: {withdrawn}"
    );
}

/// The measured run-2483-b failure: the model wrote its tool call as bare JSON
/// in the reply, the harness RECOVERED it, and a recovered call had no id, so it
/// reached the batch validator and was re-asked (trace: `recovered_tool_call`,
/// dialect `bare_json`). The harness now derives the id, so the recovered batch
/// never enters the re-ask path (see the derived-id tests below).
#[tokio::test]
async fn a_recovered_content_call_is_not_re_asked() {
    let (result, bodies, _, rejected) =
        run_scripted(vec![bare_reply(BARE), call_with_id(), final_answer()]).await;
    assert_eq!(result.expect("completes"), "all done");
    assert_eq!(rejected, 0, "no re-ask round: the id was derived");
    assert_eq!(bodies.len(), 3);
    assert!(
        bodies[1]["messages"].to_string().contains("nwt-rc-"),
        "the recovered call rode the transcript with a derived id"
    );
}

// ---------------------------------------------------------------------------
// P0: a call the harness RECOVERED from reply text gets a harness-derived id.
// ---------------------------------------------------------------------------

const BARE: &str = r#"{"name": "read_file", "arguments": {"path": "no/such/file"}}"#;

fn bare_reply(content: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{"message": {"content": content}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 5},
    })
}

/// Like `run_scripted`, but with an attempt ledger (the causal parent of a
/// derived id) and the solve observation (the trace's parse signals).
async fn run_recovery(
    script: Vec<serde_json::Value>,
) -> (
    anyhow::Result<String>,
    Vec<serde_json::Value>,
    Vec<crate::ToolEvent>,
    Vec<crate::ParseSignal>,
) {
    let server = MockServer::start().await;
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedChat {
            script,
            bodies: bodies.clone(),
        })
        .mount(&server)
        .await;
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut events: Vec<crate::ToolEvent> = Vec::new();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut obs = crate::agentic::observability::SolveObservation::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    c.tool_events = Some(&mut events);
    c.attempt_ledger = Some(&ledger);
    c.solve_obs = Some(&mut obs);
    let result = chat_complete(c, &mut NoMcp).await.map(|(reply, ..)| reply);
    let bodies = bodies.lock().unwrap().clone();
    (result, bodies, events, obs.parse_signals)
}

/// The recovered-call ids the replayed assistant turns carry, in request order.
fn replayed_ids(request: &serde_json::Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["tool_calls"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_recovered_call_completes_with_a_derived_id_and_a_reshaped_turn() {
    let (result, bodies, events, _) = run_recovery(vec![bare_reply(BARE), final_answer()]).await;
    assert_eq!(result.expect("the turn completes"), "all done");
    assert_eq!(bodies.len(), 2, "recovered call, then the final answer");
    assert_eq!(events.iter().filter(|e| e.tool == "read_file").count(), 1);
    let messages = bodies[1]["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .expect("the replayed assistant turn");
    let call = &assistant["tool_calls"][0];
    let id = call["id"].as_str().expect("a derived id");
    assert!(id.starts_with("nwt-rc-") && id.len() == 39, "{id}");
    assert_eq!(call["type"], "function");
    assert!(
        call["function"]["arguments"].is_string(),
        "stringified: {call}"
    );
    assert_eq!(
        assistant["content"], BARE,
        "the original text stays as evidence"
    );
    // A real ENOENT read now renders `"error: reading ..."` (the one
    // `"error:"` convention, #2553 finding-1 follow-up), so it also matches
    // the workflow-progress error fingerprint and a repair nudge follows the
    // tool turn — find the tool message by role rather than assuming it is
    // last; that nudge is not what this test is about.
    let tool = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .expect("the recovered call's tool result");
    assert_eq!(
        tool["tool_call_id"], id,
        "the result answers the derived id"
    );
}

#[tokio::test]
async fn identical_reply_text_in_two_rounds_derives_different_ids() {
    let (result, bodies, _, _) =
        run_recovery(vec![bare_reply(BARE), bare_reply(BARE), final_answer()]).await;
    result.expect("completes");
    let ids = replayed_ids(&bodies[2]);
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_ne!(ids[0], ids[1], "the causal parent differs per round");
}

#[tokio::test]
async fn two_identical_calls_in_one_reply_get_distinct_ids() {
    let one = "<function=read_file><parameter=path>no/such/file</parameter></function>";
    let (result, bodies, events, _) =
        run_recovery(vec![bare_reply(&format!("{one}{one}")), final_answer()]).await;
    result.expect("completes");
    let ids = replayed_ids(&bodies[1]);
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_ne!(ids[0], ids[1], "the ordinal separates identical calls");
    assert_eq!(events.iter().filter(|e| e.tool == "read_file").count(), 2);
}

/// A NATIVE id-less call keeps #2500's bounded re-ask: no id is ever derived
/// for a provider call.
#[tokio::test]
async fn a_native_idless_call_never_gets_a_derived_id() {
    let (result, bodies, _, _) =
        run_recovery(vec![idless_call(), call_with_id(), final_answer()]).await;
    result.expect("re-asked, then completes");
    assert!(
        bodies.iter().all(|b| !b.to_string().contains("nwt-rc-")),
        "no derived id may appear for a provider call"
    );
}

#[tokio::test]
async fn the_trace_signal_carries_the_full_cid_and_the_locator() {
    let (_, bodies, _, signals) = run_recovery(vec![bare_reply(BARE), final_answer()]).await;
    let id = replayed_ids(&bodies[1]).remove(0);
    let recovered = signals
        .iter()
        .find_map(|s| match s {
            crate::ParseSignal::RecoveredToolCall { calls, .. } => Some(calls.clone()),
            _ => None,
        })
        .expect("a recovered_tool_call signal");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].locator, id);
    assert!(recovered[0].cid.starts_with("bafy"), "{}", recovered[0].cid);
}

// ---------------------------------------------------------------------------
// #2482 item 6 / owed from the #2521 review: the missing-gate MCP refusal
// must ledger `ok = false` on the chat-completions wire too, not only the
// Anthropic loop (`mod_tests/anthropic_loop.rs`). Adapted from
// `run_scripted` above.
// ---------------------------------------------------------------------------

/// MCP stub that records every call it receives; local because
/// `anthropic_loop_tests::RecordingMcp` is module-private.
struct RecordingMcpStub {
    name: &'static str,
    seen: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}
#[async_trait::async_trait]
impl McpTools for RecordingMcpStub {
    fn handles(&self, name: &str) -> bool {
        name == self.name
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    async fn call(&mut self, leased: &LeasedMcpCall<'_>) -> String {
        self.seen.lock().unwrap().push(leased.args().clone());
        "tool-result-text".to_string()
    }
}

fn mcp_tool_call() -> serde_json::Value {
    serde_json::json!({
        "choices": [{"message": {"content": null, "tool_calls": [{
            "id": "call_1", "type": "function",
            "function": {"name": "my_server__get_thing", "arguments": "{}"}
        }]}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 5},
    })
}

#[tokio::test]
async fn missing_permission_gate_mcp_refusal_records_not_ok_on_the_chat_wire() {
    let server = MockServer::start().await;
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedChat {
            script: vec![mcp_tool_call(), final_answer()],
            bodies: bodies.clone(),
        })
        .mount(&server)
        .await;
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut events: Vec<crate::ToolEvent> = Vec::new();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    c.tool_events = Some(&mut events);
    // No permission gate installed at all — the missing-gate refusal path.
    c.permission_gate = None;
    let mut mcp = RecordingMcpStub {
        name: "my_server__get_thing",
        seen: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let (reply, ..) = chat_complete(c, &mut mcp)
        .await
        .expect("the loop must still complete after the refusal");

    assert_eq!(reply, "all done");
    assert_eq!(
        mcp.seen.lock().unwrap().len(),
        0,
        "the connector must receive zero calls when no gate is available"
    );
    assert_eq!(
        events.len(),
        1,
        "the refused call is still one ledgered event"
    );
    assert!(
        !events[0].ok,
        "a refusal the host never dispatched must not ledger ok=true: {:?}",
        events[0]
    );
}
