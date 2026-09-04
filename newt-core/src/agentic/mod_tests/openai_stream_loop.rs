//! #123 — the OpenAI-compatible streaming re-issue, tested at the WIRING.
//!
//! `openai_sse` already proves the parser with no HTTP in it. What is
//! unproved by those tests is everything this file covers: that the loop
//! issues a second `stream: true` request AFTER the probe round is accepted
//! (never on a tool round, never before the nudge cascade), that the streamed
//! answer is what comes back with `was_streamed = true`, and that every way
//! the second call can fail lands on the probe answer instead of on silence.
//!
//! Two tiers, because the end-to-end tier is blind to the terminal. A turn
//! can only be observed through the returned string and the `was_streamed`
//! flag — and that flag is what tells the CALLER not to print, so a suite
//! asserting only the flag stays green while every print site is deleted and
//! the operator gets a blank turn. The first section below therefore drives
//! `openai_stream_final_answer` onto a buffer and asserts the BYTES; the
//! sections after it drive whole turns through `chat_complete`.
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
// The BYTES: `openai_stream_final_answer` drives its output sink directly,
// so these assert what actually reached the terminal.
//
// The end-to-end tests below can only see the returned string and the
// `was_streamed` flag — and that flag is a literal on the success arm,
// decoupled from any write. Deleting the prefix, the per-delta write, the
// markdown push or the trailing newline leaves every one of them green while
// the operator gets a blank turn (the flag tells the CALLER not to print).
// The sink is the seam that makes those deletions fail.
// -----------------------------------------------------------------------

/// A sink the test can read back — the shape `display.rs`'s renderer tests
/// already use (`Arc<Mutex<Vec<u8>>>` behind a `Clone` handle), so the helper
/// keeps ownership of one writer while the test still holds a view of it.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Drive the streaming helper alone against a mock SSE body, onto a buffer.
/// Returns what it gave the caller AND what it painted.
async fn stream_onto(body: ResponseTemplate, markdown: bool) -> (StreamOutcome, String) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(body)
        .mount(&server)
        .await;
    let buf = Buf::default();
    let req = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.uri()))
        .json(&serde_json::json!({"stream": true}));
    let out = openai_stream_final_answer(req, buf.clone(), false, markdown, false, None).await;
    (out, buf.text())
}

/// The answer a `Printed` outcome carries; any other outcome is the test's
/// own setup being wrong about what the stream did.
fn printed(out: StreamOutcome) -> String {
    match out {
        StreamOutcome::Printed(text, _) => text,
        other => panic!("expected a streamed answer, got {other:?}"),
    }
}

/// The `▸  ` prefix, then every delta in arrival order, then the closing
/// newline — on the sink, not merely "a flag says so".
#[tokio::test]
async fn the_prefix_and_every_delta_reach_the_sink_in_order() {
    let (out, painted) = stream_onto(sse_text(&["Hel", "lo ", "world"], 100, 9), false).await;

    assert_eq!(printed(out), "Hello world");
    assert_eq!(
        painted, "▸  Hello world\n",
        "prefix + deltas in order + the closing newline all reach the terminal"
    );
}

/// The `<think>` half of the leak test its doc comment always claimed: the
/// filtered text is what the operator SEES, not just what is returned.
#[tokio::test]
async fn the_think_block_reaches_neither_the_answer_nor_the_terminal() {
    let (out, painted) = stream_onto(
        sse_text(&["<thi", "nk>secr", "et</think>", "the ", "answer"], 100, 9),
        false,
    )
    .await;

    assert_eq!(printed(out), "the answer");
    assert_eq!(
        painted, "▸  the answer\n",
        "the raw <think> block must never be painted: {painted:?}"
    );
}

/// The cut-stream notice is a print site like any other — assert the operator
/// can actually see that the fragment above it was not the answer.
#[tokio::test]
async fn a_cut_stream_says_so_on_the_terminal() {
    let (out, painted) = stream_onto(
        sse(&[r#"{"choices":[{"delta":{"content":"the beginning of an"}}]}"#]),
        false,
    )
    .await;

    assert!(
        matches!(out, StreamOutcome::UseProbe(_)),
        "a cut stream hands the caller the complete probe answer, got {out:?}"
    );
    assert!(
        painted.starts_with("▸  the beginning of an"),
        "the fragment stays on screen: {painted:?}"
    );
    assert!(
        painted.contains("⚠  newt: stream cut mid-answer"),
        "an unannounced fragment reads as the answer: {painted:?}"
    );
}

/// `markdown` is bound from the `ChatCtx` rather than discarded, and the two
/// settings do different things: on, the block writer renders; off, the deltas
/// are verbatim. The RETURNED text is raw either way — it is persisted and
/// re-sent, so no styling may enter it.
#[tokio::test]
async fn markdown_on_renders_the_answer_and_markdown_off_does_not() {
    let (off, painted_off) = stream_onto(sse_text(&["**bo", "ld**"], 100, 9), false).await;
    let (on, painted_on) = stream_onto(sse_text(&["**bo", "ld**"], 100, 9), true).await;

    assert_eq!(printed(off), "**bold**");
    assert_eq!(printed(on), "**bold**");
    assert!(
        painted_off.contains("**bold**"),
        "markdown off is a verbatim passthrough: {painted_off:?}"
    );
    #[cfg(feature = "markdown")]
    assert!(
        painted_on.contains("bold") && !painted_on.contains("**"),
        "markdown on renders the emphasis instead of printing its markers: {painted_on:?}"
    );
    // The headless strip compiles a passthrough shim in place of the renderer,
    // so there the two settings paint the same bytes and only the wiring is
    // assertable.
    #[cfg(not(feature = "markdown"))]
    assert!(painted_on.contains("**bold**"), "{painted_on:?}");
}

// -----------------------------------------------------------------------
// Interrupts: Esc is the operator's, not the wire's.
// -----------------------------------------------------------------------

/// The re-issue is a WHOLE SECOND inference call. Racing the send against the
/// interrupt flag is what keeps an already-cancelled turn from paying for it —
/// and from leaving Esc unresponsive until the server finally answers.
#[tokio::test]
async fn an_interrupt_before_the_send_never_fires_the_second_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse_text(&["never asked for"], 100, 9))
        .mount(&server)
        .await;
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let buf = Buf::default();
    let req = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.uri()))
        .json(&serde_json::json!({"stream": true}));

    let out =
        openai_stream_final_answer(req, buf.clone(), false, false, false, Some(&cancel)).await;

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "a cancelled turn must not pay for a second full inference call"
    );
    assert_eq!(buf.text(), "", "nothing streamed, so nothing is painted");
    assert!(
        matches!(out, StreamOutcome::Cancelled),
        "an interrupt ends the turn; it is not a wire failure to fall back from"
    );
}

/// A sink that trips the interrupt flag the moment the answer starts
/// painting — the deterministic stand-in for Esc mid-stream (no sleeps, no
/// racing the mock server).
struct CancelOnWrite<'a> {
    buf: Buf,
    flag: &'a std::sync::atomic::AtomicBool,
}

impl std::io::Write for CancelOnWrite<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        std::io::Write::write(&mut self.buf, bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An interrupt mid-answer and a broken socket both stop the loop with no
/// `[DONE]` — but they are not the same event, and reporting the operator's
/// own keypress as "the stream ended before [DONE]" blames the wire for it.
/// The partial stays (it is on screen and the operator asked to stop, so the
/// complete answer is NOT reprinted over it).
#[tokio::test]
async fn an_interrupt_mid_answer_is_not_reported_as_a_broken_stream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(&[
            r#"{"choices":[{"delta":{"content":"the beginning of an"}}]}"#,
        ]))
        .mount(&server)
        .await;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let buf = Buf::default();
    let sink = CancelOnWrite {
        buf: buf.clone(),
        flag: &cancel,
    };
    let req = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.uri()))
        .json(&serde_json::json!({"stream": true}));

    let out = openai_stream_final_answer(req, sink, false, false, false, Some(&cancel)).await;

    let painted = buf.text();
    assert!(
        painted.contains("⚠  newt: interrupted"),
        "the operator's own keypress must not be reported as a wire failure: {painted:?}"
    );
    assert!(
        !painted.contains("[DONE]"),
        "nothing here is the stream's fault: {painted:?}"
    );
    match out {
        StreamOutcome::Printed(text, _) => assert!(
            painted.starts_with(&format!("▸  {text}")),
            "the partial already on screen is what comes back: {text:?} vs {painted:?}"
        ),
        other => panic!("an interrupt after visible text keeps the partial, got {other:?}"),
    }
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
    // Exact, because the interesting failure is REPLACING rather than merging
    // and a `>=` threshold passes either way: the probe reported 7 output
    // tokens and the stream 9, so a replacement scores 9 and still clears
    // `>= 9`. Input is the max of (100, 101) and output is the sum 7 + 9 —
    // `merge_round_usage`'s contract, not a coincidence of these numbers.
    let u = usage.expect("usage survives the streaming round");
    assert_eq!(
        u.input_tokens, 101,
        "input is the largest single round: {u:?}"
    );
    assert_eq!(
        u.output_tokens, 16,
        "both rounds were billed, so both are counted: 7 (probe) + 9 (stream): {u:?}"
    );
    assert_eq!(hallu, 0);
}

/// End to end: an interrupt that lands after the probe answered ends the turn
/// with an empty reply — the loop's own round-boundary contract — and the
/// second inference call never leaves the harness.
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
    let (reply, streamed, _usage, _hallu) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("an interrupt is not an error");

    assert_eq!(reply, "", "an interrupted turn ends with an empty reply");
    assert!(!streamed);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "only the probe — the operator cancelled before the re-issue"
    );
}

/// Answers the probe and trips the interrupt flag while doing it: by the time
/// the loop reaches the streaming re-issue, the operator has hit Esc.
struct CancelOnProbe {
    flag: Arc<std::sync::atomic::AtomicBool>,
}
impl Respond for CancelOnProbe {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        probe_reply("the probe already answered")
    }
}

/// The `markdown` field is bound from the `ChatCtx` (it used to be discarded),
/// so a markdown turn streams like any other — and the RETURNED answer stays
/// raw, because it is persisted and re-sent and no styling may enter it. The
/// rendering half is asserted on the sink above, where bytes are observable.
#[tokio::test]
async fn a_markdown_turn_streams_and_returns_the_raw_answer() {
    let server = MockServer::start().await;
    let seen = mount(&server, |_| sse_text(&["**bo", "ld**"], 101, 9)).await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = ctx(&uri, &messages, &caveats);
    ctx.markdown = true;
    let (reply, streamed, _usage, _hallu) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("a markdown turn streams too");

    assert_eq!(reply, "**bold**", "the transcript keeps the raw source");
    assert!(streamed);
    assert_eq!(seen.lock().unwrap().len(), 2);
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

/// A stream CUT mid-answer is not an answer. The fragment already on screen
/// stops at whatever byte the socket died on; the probe answer the loop is
/// still holding is complete, vetted, and the thing that gets persisted and
/// re-sent. Returning the fragment silently truncates the turn's record — so
/// the complete answer comes back instead, with `was_streamed = false` so the
/// caller prints it under the notice.
#[tokio::test]
async fn a_cut_stream_returns_the_complete_probe_answer_not_the_fragment() {
    let server = MockServer::start().await;
    let seen = mount(&server, |_| {
        // Text, then the socket ends: no usage chunk, no `[DONE]`.
        sse(&[r#"{"choices":[{"delta":{"content":"the beginning of an"}}]}"#])
    })
    .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("a cut stream is recoverable, not fatal");

    assert_eq!(
        reply, "the probe already answered",
        "a truncated fragment must never become the turn's answer"
    );
    assert!(
        !streamed,
        "the fragment on screen is not the answer, so the caller must print the complete one"
    );
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
