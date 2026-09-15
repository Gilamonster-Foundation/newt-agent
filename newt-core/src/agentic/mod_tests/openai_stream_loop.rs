//! The OpenAI-compatible final answer, tested at the WIRING (#123, #2372).
//!
//! `openai_sse` proves the parser with no HTTP in it. This file drives whole
//! turns through `chat_complete`. Since #2372 an accepted final answer is
//! generated ONCE: the loop returns the gated reply with `was_streamed = false`
//! and the host renders it, so these tests pin the request counts and the text
//! that comes back rather than a display stream.
//!
//! These tests touch no process-global state (no env, no filesystem), so
//! unlike `anthropic_loop.rs` they need neither a serial lane nor an env guard.

use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A workspace that deliberately does not exist, so the self-verify gate
/// finds no check to demand and cannot add a round to the request counts
/// these tests assert on (same reason as `anthropic_loop.rs`).
const NO_CHECKS_WORKSPACE: &str = "newt-core-test-workspace-that-does-not-exist";

fn ctx<'a>(server_uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    ChatCtx {
        smart_harness: None,
        url: server_uri,
        model: "test-model",
        kind: BackendKind::Openai,
        api_key: Some("sk-test"),
        messages,
        task: "do the thing",
        workspace: NO_CHECKS_WORKSPACE,
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

fn msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ]
}

fn body_json(req: &Request) -> serde_json::Value {
    serde_json::from_slice(&req.body).unwrap_or_default()
}

/// A complete JSON response from an endpoint ignoring the requested SSE transport.
fn probe_reply(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 7},
    }))
}

/// End to end: an interrupt that lands after the answer arrived ends the turn
/// with an empty reply — the loop's own round-boundary contract.
///
/// An empty REPLY is not an empty BILL. The round was paid for before the
/// operator pressed anything, so its usage has to survive the cancelled arm.
#[tokio::test]
async fn an_interrupt_after_the_probe_ends_the_turn_with_no_second_call() {
    let server = MockServer::start().await;
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CancelOnProbe {
            flag: cancel.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = ctx(&uri, &messages, &caveats);
    ctx.cancel = Some(cancel.as_ref());
    let (reply, streamed, usage, _hallu) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("an interrupt is not an error");

    assert_eq!(reply, "", "an interrupted turn ends with an empty reply");
    assert!(!streamed);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let u = usage.expect("the round was billed before the interrupt landed");
    assert_eq!(
        (u.input_tokens, u.output_tokens),
        (100, 7),
        "the cancelled arm returns the turn's accumulated usage, not a fresh zero: {u:?}"
    );
}

/// Answers the probe and trips the interrupt flag while doing it: by the time
/// the loop would accept the answer, the operator has hit Esc.
struct CancelOnProbe {
    flag: Arc<std::sync::atomic::AtomicBool>,
}
impl Respond for CancelOnProbe {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        probe_reply("the probe already answered")
    }
}

/// A markdown turn returns the RAW answer — it is persisted and re-sent, so no
/// styling may enter it — and leaves rendering to the host.
#[tokio::test]
async fn a_markdown_turn_returns_the_raw_answer_for_the_host_to_render() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(probe_reply("**bold**"))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = ctx(&uri, &messages, &caveats);
    ctx.markdown = true;
    let (reply, streamed, _usage, _hallu) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("a markdown turn completes");

    assert_eq!(reply, "**bold**", "the transcript keeps the raw source");
    assert!(!streamed, "the host renders it");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

/// A tool round and its answer are exactly two generations: nothing is
/// generated a second time for display.
#[tokio::test]
async fn a_tool_round_and_its_answer_are_two_generations() {
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ToolThenAnswer { seen: seen.clone() })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("tool round then answer");

    assert_eq!(reply, "answered after the tool");
    assert!(!streamed);
    assert_eq!(seen.lock().unwrap().len(), 2, "one tool batch, one answer");
}

struct ToolThenAnswer {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}
impl Respond for ToolThenAnswer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let had_tool_result = body["messages"]
            .as_array()
            .map(|m| m.iter().any(|x| x["role"] == "tool"))
            .unwrap_or(false);
        self.seen.lock().unwrap().push(body);
        if had_tool_result {
            return probe_reply("answered after the tool");
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                }]
            }}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 4},
        }))
    }
}

/// #2372: reasoning never reaches the accepted answer — the string that is
/// returned, persisted and re-sent. Both shapes: an inline `<think>` block, and
/// #528's lone leading closer. The deleted display stream used to filter these;
/// the primary path's `split_reasoning` must.
#[tokio::test]
async fn reasoning_does_not_leak_into_the_accepted_answer() {
    for content in ["<think>x</think>Done.", "x</think>Done."] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(probe_reply(content))
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("dispatch");

        assert_eq!(reply, "Done.", "{content:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[cfg(test)]
#[path = "openai_primary_stream.rs"]
mod primary_stream;
