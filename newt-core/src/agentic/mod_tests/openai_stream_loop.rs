//! #123 — the OpenAI-compatible streaming re-issue, tested at the WIRING.
//!
//! `openai_sse` already proves the parser with no HTTP in it. What is
//! unproved by those tests is everything this file covers: that the loop
//! issues a second `stream: true` request AFTER the probe round is accepted
//! (never on a tool round, never before the nudge cascade), that the streamed
//! answer is what comes back with `was_streamed = true`, and that every way
//! the second call can fail lands on the probe answer instead of on silence.
//!
//! These tests touch no process-global state (no env, no filesystem), so
//! unlike `anthropic_loop.rs` they need neither a serial lane nor an env
//! guard: the streaming re-issue has no valve to set.

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

fn is_stream(req: &Request) -> bool {
    body_json(req)["stream"].as_bool().unwrap_or(false)
}

/// A non-streaming `/v1/chat/completions` 200 body.
fn probe_reply(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 7},
    }))
}

/// An SSE body from `data:` frames — the framing the wire actually uses.
fn sse(frames: &[&str]) -> ResponseTemplate {
    let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream")
}

/// Text deltas, a usage chunk, and the `[DONE]` sentinel.
fn sse_text(parts: &[&str], input: u64, output: u64) -> ResponseTemplate {
    let mut frames: Vec<String> = parts
        .iter()
        .map(|p| format!(r#"{{"choices":[{{"delta":{{"content":"{p}"}}}}]}}"#))
        .collect();
    frames.push(format!(
        r#"{{"choices":[],"usage":{{"prompt_tokens":{input},"completion_tokens":{output}}}}}"#
    ));
    frames.push("[DONE]".to_string());
    sse(&frames.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Records every request body so a test can assert on the SEQUENCE of
/// requests, then answers streaming and non-streaming requests differently.
struct Recorder<F: Fn(&serde_json::Value) -> ResponseTemplate> {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
    reply: F,
}
impl<F: Fn(&serde_json::Value) -> ResponseTemplate + Send + Sync + 'static> Respond
    for Recorder<F>
{
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let streaming = is_stream(req);
        self.seen.lock().unwrap().push(body.clone());
        if streaming {
            (self.reply)(&body)
        } else if body["messages"]
            .as_array()
            .map(|m| m.iter().any(|x| x["role"] == "tool"))
            .unwrap_or(false)
        {
            probe_reply("the tool round finished")
        } else {
            probe_reply("the probe already answered")
        }
    }
}

async fn mount<F>(server: &MockServer, reply: F) -> Arc<Mutex<Vec<serde_json::Value>>>
where
    F: Fn(&serde_json::Value) -> ResponseTemplate + Send + Sync + 'static,
{
    let seen = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(Recorder {
            seen: seen.clone(),
            reply,
        })
        .mount(server)
        .await;
    seen
}

// -----------------------------------------------------------------------
// The wiring: the accepted round is re-issued with stream:true
// -----------------------------------------------------------------------

/// The point of #123: the final answer arrives token by token, and the
/// caller is told it was already printed so it does not print it twice.
#[tokio::test]
async fn the_final_round_is_re_issued_as_a_stream() {
    let server = MockServer::start().await;
    let seen = mount(&server, |_| sse_text(&["Hello ", "world"], 101, 9)).await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, usage, hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("openai streaming re-issue should succeed");

    assert_eq!(reply, "Hello world", "the STREAMED answer is what returns");
    assert!(
        streamed,
        "was_streamed must be true or the caller prints the answer a second time"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one probe, one streaming re-issue: {seen:?}");
    assert_eq!(seen[0]["stream"], serde_json::json!(false));
    assert_eq!(seen[1]["stream"], serde_json::json!(true));
    assert_eq!(
        seen[1]["stream_options"]["include_usage"],
        serde_json::json!(true),
        "without include_usage the streamed round reports no tokens at all"
    );
    let u = usage.expect("usage survives the streaming round");
    assert!(
        u.output_tokens >= 9,
        "the streamed round's usage is merged in, not dropped: {u:?}"
    );
    assert_eq!(hallu, 0);
}

/// A tool round must NOT be re-issued as a stream — only the round that ends
/// the turn is. Streaming a tool round would double-bill every round of the
/// turn and print a half-answer the loop then discards.
#[tokio::test]
async fn a_tool_round_is_not_streamed() {
    let server = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = seen.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ToolThenAnswer { seen: recorded })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("tool round then streamed answer");

    assert_eq!(reply, "answered after the tool");
    assert!(streamed);
    let seen = seen.lock().unwrap();
    let streams: Vec<bool> = seen
        .iter()
        .map(|b| b["stream"].as_bool().unwrap_or(false))
        .collect();
    assert_eq!(
        streams,
        vec![false, false, true],
        "tool round, final probe, then exactly ONE stream at the end: {seen:?}"
    );
}

struct ToolThenAnswer {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}
impl Respond for ToolThenAnswer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let streaming = is_stream(req);
        let had_tool_result = body["messages"]
            .as_array()
            .map(|m| m.iter().any(|x| x["role"] == "tool"))
            .unwrap_or(false);
        self.seen.lock().unwrap().push(body);
        if streaming {
            return sse_text(&["answered ", "after the tool"], 100, 5);
        }
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

// -----------------------------------------------------------------------
// Fallback: the probe already holds a good answer, so a failed re-issue
// must never return silence.
// -----------------------------------------------------------------------

/// A non-2xx on the streaming request falls back to the probe answer, and
/// says so by returning `was_streamed = false` — nothing was printed, so the
/// caller still has to print it.
#[tokio::test]
async fn a_non_2xx_stream_falls_back_to_the_probe_content() {
    let server = MockServer::start().await;
    let seen = mount(&server, |_| ResponseTemplate::new(503)).await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("a failed stream is recoverable, not fatal");

    assert_eq!(reply, "the probe already answered");
    assert!(
        !streamed,
        "nothing was printed, so the caller must be told to print it"
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// A 200 stream that produces no text (cut before `[DONE]`, or a model that
/// simply said nothing on the second call) is the same failure: keep the
/// probe answer rather than returning a blank turn.
#[tokio::test]
async fn an_empty_stream_falls_back_to_the_probe_content() {
    let server = MockServer::start().await;
    let seen = mount(&server, |_| {
        // A role-only opening delta and then the socket ends: 200, valid
        // frames, no text, no `[DONE]`.
        sse(&[r#"{"choices":[{"delta":{"role":"assistant"}}]}"#])
    })
    .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("an empty stream is recoverable");

    assert_eq!(reply, "the probe already answered");
    assert!(!streamed);
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// The non-streaming arm of this loop runs every reply through
/// `split_reasoning`, so an inline `<think>` block never reaches the answer.
/// Turning streaming on must not start leaking it — into the terminal, into
/// the returned string, or into the transcript that gets re-sent. The tags are
/// split across deltas because that is how a real stream delivers them, and a
/// per-delta check could not see them.
#[tokio::test]
async fn inline_think_blocks_do_not_leak_into_the_streamed_answer() {
    let server = MockServer::start().await;
    let seen = mount(&server, |_| {
        sse_text(&["<thi", "nk>secr", "et</think>", "the ", "answer"], 100, 9)
    })
    .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("streamed dispatch");

    assert!(streamed, "text was printed live");
    assert_eq!(reply, "the answer");
    assert!(!reply.contains("secret"), "reasoning leaked: {reply:?}");
    assert_eq!(seen.lock().unwrap().len(), 2);
}
