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
//!
//! # They are not load-flaky, and this is why — do not "fix" them with a lane
//!
//! Reported as failing under machine load and hunted for deliberately: 6 500+
//! runs of this file at 32× process oversubscription with every core pinned,
//! plus 84 whole-`newt-core` runs at up to 256 test threads. **Zero failures
//! here** — while the SAME runs reproduced load failures in `retry::tests`
//! (env-global), `tool_spinner_pty_test` (real PTY) and
//! `execute_tool_branch_tests`, so the harness demonstrably catches this class
//! on this box. Each suspect is closed by construction, not by luck:
//!
//! * **The `Buf` sink.** `flush` is a no-op and every write lands in the shared
//!   `Vec` synchronously, so there is nothing to lose to a missed flush; and
//!   `stream_onto` awaits the helper to COMPLETION before reading it, so the
//!   markdown writer's `finish()` has already run.
//! * **wiremock.** One server per test, one mock, one request, no `expect()`
//!   counts — there is no port or expectation shared with anything.
//! * **The tty/spinner global.** These call with `color = false`, so
//!   `legacy_caps` yields `LineCaps::None`, `Terminal::lease_with_caps` refuses
//!   on `!caps.can_own()` BEFORE touching the arbiter mutex, and the spinner is
//!   `None`. Zero bytes on stdout, zero contention on the lease.
//! * **The interrupt flag.** `cancelled()` reads the flag on its FIRST poll and
//!   `select!` is `biased`, so an already-set flag wins without a timer and a
//!   flag set from the sink is observed on the very next loop iteration. No
//!   window either way.
//! * **Chunk boundaries** (what load actually moves): `SseAccumulator` holds a
//!   rolling line buffer and `MarkdownStreamWriter` holds a line buffer, so
//!   arrival splits are normalised before anything is asserted.
//!
//! The hunt did turn up one REAL load-sensitivity here, and it was not any of
//! the above: the loop decoded each chunk with its own `from_utf8_lossy`, so a
//! multi-byte character split across two `chunk()` boundaries became U+FFFD —
//! silently, in the text that is printed, returned, persisted and re-sent.
//! Where the boundary falls is exactly what machine load moves. Every body in
//! this file was ASCII, which is why nothing here could ever see it. Fixed by
//! `decode_chunk` and covered below; the Ollama wire still has the twin.

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
        matches!(out, StreamOutcome::Cancelled(None)),
        "an interrupt ends the turn; it is not a wire failure to fall back from — \
         and with no call fired there is no usage to report, which is `None` and \
         never a zero: {out:?}"
    );
}

/// Serve one SSE response over a socket the test owns, and interrupt only once
/// the client is demonstrably reading it.
///
/// `wiremock` cannot express this case. The third outcome arm needs the
/// interrupt to land AFTER the send resolved and BEFORE any text is painted,
/// and nothing wiremock exposes says when the client began reading — a flag
/// tripped from a `Respond` races the send's own completion, which would make
/// the test either flaky or (worse) silently cover the pre-send arm instead.
///
/// TCP backpressure is the signal that removes the race. The padding is far
/// larger than the socket buffers, so `write_all` returns only after the client
/// has drained megabytes: by then the send has long resolved and the chunk loop
/// is running, so the pre-send check cannot be what fires. The padding is SSE
/// COMMENT lines (`:…`), which `apply_line` drops before `serde_json` is ever
/// reached, so this costs bytes and not parsing.
async fn interrupt_once_the_client_is_reading(frames: &str) -> (StreamOutcome, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancel.clone();
    let head = frames.to_string();

    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut scratch = [0u8; 8192];
        // The head's CONTENT does not matter; that it arrived does — the
        // response must not start before the client has actually asked.
        let asked = sock.read(&mut scratch).await.unwrap();
        assert!(asked > 0, "the client sent its request");
        // Neither `Content-Length` nor chunked framing, deliberately: an
        // HTTP/1.1 response body then runs to connection close, and this
        // connection never closes. The client therefore BLOCKS in `chunk()`
        // rather than seeing an EOF — an EOF here would be a cut stream, which
        // is a different arm entirely.
        sock.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Connection: close\r\n\r\n{head}"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let pad = format!(":{}\n", "x".repeat(64 * 1024 - 2));
        for _ in 0..256 {
            sock.write_all(pad.as_bytes()).await.unwrap();
        }
        // 16 MiB have gone out and been drained, so the answer stream is well
        // under way. NOW the operator presses Esc.
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        std::future::pending::<()>().await;
    });

    let buf = Buf::default();
    let req = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({"stream": true}));
    let out =
        openai_stream_final_answer(req, buf.clone(), false, false, false, Some(cancel.as_ref()))
            .await;
    server.abort();
    (out, buf.text())
}

/// Serve an SSE body whose bytes are cut at a chosen offset, with a real gap
/// between the two writes — the split `reqwest` would otherwise only produce
/// when the machine is busy, made to happen on demand.
async fn stream_split_at(body: &str, cut: usize) -> (StreamOutcome, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bytes = body.as_bytes().to_vec();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        sock.set_nodelay(true).unwrap();
        let mut scratch = [0u8; 8192];
        // The head's CONTENT does not matter; that it arrived does — the
        // response must not start before the client has actually asked.
        let asked = sock.read(&mut scratch).await.unwrap();
        assert!(asked > 0, "the client sent its request");
        sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Connection: close\r\n\r\n",
        )
        .await
        .unwrap();
        sock.write_all(&bytes[..cut]).await.unwrap();
        sock.flush().await.unwrap();
        // Let the reader drain the first half before the rest is written, so
        // the two halves land in two different `chunk()` calls.
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        sock.write_all(&bytes[cut..]).await.unwrap();
        sock.shutdown().await.unwrap();
    });

    let buf = Buf::default();
    let req = reqwest::Client::new()
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&serde_json::json!({"stream": true}));
    let out = openai_stream_final_answer(req, buf.clone(), false, false, false, None).await;
    server.abort();
    (out, buf.text())
}

/// A character split across two chunks is still that character.
///
/// `reqwest` splits where the socket did, not where the protocol did — the
/// same fact `SseAccumulator`'s line buffer exists for — and a chunk boundary
/// lands wherever machine load puts it. Decoding each chunk on its own turns
/// the half that arrived into U+FFFD, and that corruption is silent and
/// permanent: the mangled text is what gets printed, returned, persisted, and
/// re-sent to the model. It is also invisible to every other test in this
/// file, because every other body here is ASCII.
#[tokio::test]
async fn a_character_split_across_two_chunks_is_not_corrupted() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"café\"}}]}\n\ndata: [DONE]\n\n";
    // Between the two bytes of `é` (0xC3 0xA9) — the boundary a busy box picks
    // by accident.
    let cut = body.find('é').unwrap() + 1;

    let (out, painted) = stream_split_at(body, cut).await;

    assert_eq!(
        printed(out),
        "café",
        "the returned answer is what gets re-sent"
    );
    assert_eq!(
        painted, "▸  café\n",
        "and the operator sees the same: {painted:?}"
    );
}

/// The adversarial form of the test above, and the one that cannot go vacuous.
///
/// The socket test can only split where the kernel agrees to split, so on some
/// run it may deliver both halves together and pass without proving anything.
/// This splits at EVERY byte offset — including inside all of a 2-, 3- and
/// 4-byte character — and is pure, so it is the guard that actually holds.
#[test]
fn decode_chunk_reassembles_a_split_at_every_byte_offset() {
    let s = "aé→𝄞z";
    let bytes = s.as_bytes();
    for cut in 0..=bytes.len() {
        let mut carry = Vec::new();
        let mut got = decode_chunk(&mut carry, &bytes[..cut]);
        got.push_str(&decode_chunk(&mut carry, &bytes[cut..]));
        assert_eq!(got, s, "split at byte {cut} corrupted the answer");
        assert!(carry.is_empty(), "nothing is left held at byte {cut}");
    }
}

/// Byte-at-a-time is the same property taken to its limit — the shape
/// `openai_sse`'s own `one_byte_at_a_time_produces_the_same_answer` already
/// uses one layer up.
#[test]
fn decode_chunk_survives_one_byte_at_a_time() {
    let s = "aé→𝄞z";
    let mut carry = Vec::new();
    let got: String = s
        .as_bytes()
        .iter()
        .map(|b| decode_chunk(&mut carry, &[*b]))
        .collect();
    assert_eq!(got, s);
    assert!(carry.is_empty());
}

/// Bytes that are not a cut character but genuinely invalid must be SPENT, not
/// held: holding them would grow the carry without bound and stall the stream
/// forever, waiting for a continuation that is never coming.
#[test]
fn decode_chunk_spends_invalid_bytes_instead_of_stalling_on_them() {
    let mut carry = Vec::new();
    let got = decode_chunk(&mut carry, b"ok\xffthen");
    assert_eq!(got, "ok\u{FFFD}then");
    assert!(
        carry.is_empty(),
        "an invalid byte is consumed, never carried"
    );
}

/// The third outcome arm, which no test reached: the operator interrupts after
/// the stream is under way but before one character of the answer has been
/// painted. That is neither a cut wire nor a fallback — the turn ends empty,
/// because printing the complete probe answer over an interrupt is the exact
/// opposite of what Esc asked for.
///
/// The usage rides along for the same reason `UseProbe`'s does, and the reason
/// was never "the answer arrived": the second call reported tokens and they
/// were spent. Dropping them here while merging them one arm up would make the
/// turn's billed cost depend on WHICH way the second call failed.
#[tokio::test]
async fn an_interrupt_before_any_text_ends_the_turn_and_still_bills_the_call() {
    // Usage FIRST, so it is parsed well before the interrupt can land; then a
    // role-only delta, which paints nothing. No text delta anywhere, and no
    // `[DONE]` — this stream is stopped, not finished.
    let (out, painted) = interrupt_once_the_client_is_reading(
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":5}}\n\n\
         data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    )
    .await;

    assert_eq!(
        painted, "",
        "nothing reached the terminal — that is what makes this arm this arm"
    );
    match out {
        StreamOutcome::Cancelled(usage) => {
            // `expect` and not a silent skip: reaching the PRE-SEND `Cancelled`
            // instead would look identical without it, and a test that cannot
            // tell which arm it covered is not covering either.
            let u = usage.expect("the second call reported usage before the interrupt");
            assert_eq!(
                (u.input_tokens, u.output_tokens),
                (42, 5),
                "an interrupted call is still a call the operator paid for: {u:?}"
            );
        }
        other => panic!("an interrupt with nothing on screen ends the turn, got {other:?}"),
    }
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
///
/// An empty REPLY is not an empty BILL. The probe round was paid for before
/// the operator pressed anything, so its usage has to survive the cancelled
/// arm; a turn that reports no tokens because it ended early would understate
/// what the session actually cost.
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
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "only the probe — the operator cancelled before the re-issue"
    );
    let u = usage.expect("the probe round was billed before the interrupt landed");
    assert_eq!(
        (u.input_tokens, u.output_tokens),
        (100, 7),
        "the cancelled arm returns the turn's accumulated usage, not a fresh zero: {u:?}"
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
