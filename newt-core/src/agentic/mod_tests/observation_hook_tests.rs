use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn ctx<'a>(server_uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    ChatCtx {
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
        steering: None,
        completed_spill_renderer: None,
    }
}

/// Set a hard gate immediately above the live initial wire request. The
/// following tool result must then exercise the preflight refusal rather
/// than making this regression depend on a frozen catalog size.
fn initial_request_budget(messages: &[MemMessage], task: &str) -> usize {
    let tools = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
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
