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
        verify_outcomes: false,
        round_cap_hit: None,
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
    frames.push(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.to_string());
    frames.push(format!(
        r#"{{"choices":[],"usage":{{"prompt_tokens":{input},"completion_tokens":{output}}}}}"#
    ));
    frames.push("[DONE]".to_string());
    sse(&frames.iter().map(String::as_str).collect::<Vec<_>>())
}

const STREAM_USAGE: &str = r#"{"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":5}}"#;

fn stream_usage() -> Option<crate::TokenUsage> {
    Some(crate::TokenUsage {
        input_tokens: 42,
        output_tokens: 5,
    })
}

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

/// #2372 (moved onto the primary path from the deleted display reissue): a
/// primary stream cut before `[DONE]` after its usage chunk was generated and
/// billed, so every attempt is failed WITH that usage.
#[tokio::test]
async fn a_primary_stream_cut_after_its_usage_chunk_is_failed_with_that_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(&[
            r#"{"choices":[{"delta":{"content":"half an answ"}}]}"#,
            STREAM_USAGE,
        ]))
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    let error = chat_complete(c, &mut NoMcp)
        .await
        .expect_err("a stream that never reaches [DONE] is not an answer");
    assert!(format!("{error:#}").contains("[DONE]"), "{error:#}");

    let received = server.received_requests().await.expect("journal").len();
    let ledger = ledger.lock().unwrap();
    let records: Vec<_> = ledger.records().collect();
    assert_eq!(records.len(), received, "attempts == wire requests");
    for record in records {
        assert_eq!(record.state, crate::attempts::AttemptState::Failed);
        assert_eq!(record.usage, stream_usage());
    }
}

/// #2372 (moved onto the primary path): an error event after a usage frame
/// fails the attempt with that usage even when `[DONE]` follows, and a context
/// rejection is classified `ContextExceeded` while any other error is not.
#[tokio::test]
async fn a_primary_stream_error_event_after_usage_is_failed_with_that_usage() {
    for (error_frame, context_exceeded) in [
        (
            r#"{"error":{"message":"Context size has been exceeded."}}"#,
            true,
        ),
        (r#"{"error":{"message":"busy","code":503}}"#, false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(&[STREAM_USAGE, error_frame, "[DONE]"]))
            .mount(&server)
            .await;
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
        let mut c = ctx(&uri, &messages, &caveats);
        c.attempt_ledger = Some(&ledger);
        let error = chat_complete(c, &mut NoMcp)
            .await
            .expect_err("an error event is not an answer");
        assert_eq!(
            crate::agentic::observability::error_class(&error)
                == Some(crate::agentic::observability::ErrorClass::ContextExceeded),
            context_exceeded,
            "{error_frame}: {error:#}"
        );

        let received = server.received_requests().await.expect("journal").len();
        let ledger = ledger.lock().unwrap();
        let records: Vec<_> = ledger.records().collect();
        assert_eq!(
            records.len(),
            received,
            "{error_frame}: attempts == wire requests"
        );
        for record in records {
            assert_eq!(
                record.state,
                crate::attempts::AttemptState::Failed,
                "{error_frame}"
            );
            assert_eq!(record.usage, stream_usage(), "{error_frame}");
        }
    }
}

/// Review round 2, item 1: a complete `[DONE]` primary stream that strict
/// decoding rejects (here a tool call without an id) was still generated and
/// billed. Every attempt is failed WITH the usage the stream reported.
#[tokio::test]
async fn a_primary_stream_rejected_by_strict_decoding_is_failed_with_its_reported_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"type":"function","function":{"name":"read_file","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            STREAM_USAGE,
            "[DONE]",
        ]))
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    let error = chat_complete(c, &mut NoMcp)
        .await
        .expect_err("a tool call without an id is rejected");
    assert!(format!("{error:#}").contains("has no ID"), "{error:#}");

    let received = server.received_requests().await.expect("journal").len();
    let ledger = ledger.lock().unwrap();
    let records: Vec<_> = ledger.records().collect();
    assert_eq!(records.len(), received, "attempts == wire requests");
    for record in records {
        assert_eq!(record.state, crate::attempts::AttemptState::Failed);
        assert_eq!(record.usage, stream_usage());
    }
}

/// Send one request to `url` through `dispatch_with_decoder` (no retries) with
/// the strict OpenAI decoder, expecting it to fail; returns the error and the
/// one attempt it recorded.
async fn failed_dispatch_attempt(
    url: &str,
    decode: fn(&[u8]) -> anyhow::Result<serde_json::Value>,
) -> (anyhow::Error, crate::attempts::AttemptRecord) {
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let scope = attempt_capture::AttemptScope {
        ledger: &ledger,
        turn: "prompt:turn",
        model: "test-model",
        backend: "test-backend",
    };
    let policy = RetryPolicy {
        max_retries: 0,
        base: std::time::Duration::ZERO,
        max: std::time::Duration::ZERO,
        jitter: false,
    };
    let error = dispatch_with_decoder(
        &policy,
        Some(scope),
        || async {
            Ok(reqwest::Client::new()
                .post(url)
                .json(&serde_json::json!({})))
        },
        "inference endpoint",
        |_, _, _| {},
        None,
        decode,
    )
    .await
    .expect_err("the response is rejected");
    let ledger = ledger.lock().unwrap();
    let records: Vec<_> = ledger.records().cloned().collect();
    assert_eq!(records.len(), 1);
    (error, records[0].clone())
}

/// Item 1's other primary send: `dispatch_with_decoder` (the cap-exit summary)
/// keeps a strictly rejected response's usage on its failed attempt too.
#[tokio::test]
async fn dispatch_with_decoder_keeps_a_strictly_rejected_responses_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(&[
            r#"{"choices":[{"delta":{"content":"a summary"}}]}"#,
            STREAM_USAGE,
            "[DONE]",
        ]))
        .mount(&server)
        .await;
    let url = format!("{}/v1/chat/completions", server.uri());
    let (error, record) =
        failed_dispatch_attempt(&url, smart_harness::decode_openai_response).await;
    assert!(
        format!("{error:#}").contains("no finish reason"),
        "{error:#}"
    );
    assert_eq!(record.state, crate::attempts::AttemptState::Failed);
    assert_eq!(record.usage, stream_usage());
}

/// Serve `body` under a `Content-Length` it never meets, so the client's body
/// read fails (not a clean EOF), and dispatch it with `decode`.
async fn dropped_body_attempt(
    body: String,
    decode: fn(&[u8]) -> anyhow::Result<serde_json::Value>,
) -> (anyhow::Error, crate::attempts::AttemptRecord) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut scratch = [0u8; 8192];
        assert!(
            sock.read(&mut scratch).await.unwrap() > 0,
            "the client asked"
        );
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n";
        sock.write_all(format!("{head}{body}").as_bytes())
            .await
            .unwrap();
        // Dropping the socket here cuts the body short of its declared length.
    });
    let (error, record) = failed_dispatch_attempt(&url, decode).await;
    server.abort();
    assert!(
        format!("{error:#}").contains("request failed reading response"),
        "{error:#}"
    );
    (error, record)
}

/// Review rounds 3 and 4, item e: a 2xx body whose read fails keeps the usage
/// its bytes already reported — whether the decode of those bytes was rejected
/// (a stream cut mid-answer), or succeeded (a whole `[DONE]` stream, or whole
/// JSON, before the break).
#[tokio::test]
async fn a_body_read_failure_keeps_the_usage_its_bytes_reported() {
    let answer = r#"{"choices":[{"delta":{"content":"an answer"},"finish_reason":"stop"}]}"#;
    let json: fn(&[u8]) -> anyhow::Result<serde_json::Value> =
        |bytes| Ok(serde_json::from_slice(bytes)?);
    let cases = [
        (
            "stream cut mid-answer",
            format!("data: {answer}\n\ndata: {STREAM_USAGE}\n\n"),
            smart_harness::decode_openai_response as fn(&[u8]) -> _,
        ),
        (
            "whole [DONE] stream",
            format!("data: {answer}\n\ndata: {STREAM_USAGE}\n\ndata: [DONE]\n\n"),
            smart_harness::decode_openai_response,
        ),
        (
            "whole Ollama JSON",
            r#"{"message":{"content":"an answer"},"prompt_eval_count":42,"eval_count":5}"#
                .to_string(),
            json,
        ),
    ];
    for (name, body, decode) in cases {
        let (_, record) = dropped_body_attempt(body, decode).await;
        assert_eq!(
            record.state,
            crate::attempts::AttemptState::Failed,
            "{name}"
        );
        assert_eq!(record.usage, stream_usage(), "{name}");
    }
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

/// Assert the #2313 invariant for one OpenAI Chat turn: every received request
/// is on the generation path (so no token-count probe, `/v1/models`, or other
/// path was hit and the filter hides nothing), and the ledger's attempts are
/// exactly those requests, keyed by their bodies.
async fn assert_chat_attempts_equal_wire_requests(
    server: &MockServer,
    ledger: &std::sync::Mutex<crate::attempts::AttemptLedger>,
) -> usize {
    let received = server.received_requests().await.expect("journal");
    assert!(
        received
            .iter()
            .all(|request| request.url.path() == "/v1/chat/completions"),
        "only /v1/chat/completions may be hit (no token-count probe paths): {:?}",
        received.iter().map(|r| r.url.path()).collect::<Vec<_>>()
    );
    let ledger = ledger.lock().unwrap();
    let mut wire: Vec<_> = received
        .iter()
        .map(|request| content_addressable::RawContentId::from_content(&request.body))
        .collect();
    let mut recorded: Vec<_> = ledger.records().map(|record| record.key.request).collect();
    wire.sort();
    recorded.sort();
    assert_eq!(
        recorded, wire,
        "attempts == wire requests, keyed by their bodies"
    );
    for record in ledger.records() {
        assert!(
            record.key.turn.starts_with("prompt:"),
            "{}",
            record.key.turn
        );
        assert_eq!(record.key.role, "primary");
        assert_eq!(record.state, crate::attempts::AttemptState::Ok);
    }
    wire.len()
}

/// #2313 (b1b): every primary OpenAI Chat request — the tool round and the
/// accepted answer — is exactly one ledger attempt.
///
/// Scope of the count: the Chat primary loop's rounds and cap-exit summary.
#[tokio::test]
async fn every_openai_chat_request_is_one_ledger_attempt_keyed_by_its_wire_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ToolThenAnswer {
            seen: Arc::new(Mutex::new(Vec::new())),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    let (reply, streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("tool round then answer");
    assert_eq!(reply, "answered after the tool");
    assert!(!streamed, "the host renders the accepted answer (#2372)");

    assert_eq!(
        assert_chat_attempts_equal_wire_requests(&server, &ledger).await,
        2,
        "tool round, accepted answer; no display reissue (#2372)"
    );
    let totals = ledger.lock().unwrap().totals();
    assert_eq!(
        (totals.in_tokens, totals.out_tokens, totals.usage_complete),
        (100 + 100, 4 + 7, true)
    );
}

/// Tool calls while tools are offered; an SSE summary once they are not.
struct ToolThenCapSummary;
impl Respond for ToolThenCapSummary {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                    }]
                }}],
                "usage": {"prompt_tokens": 90, "completion_tokens": 3},
            }));
        }
        sse_text(&["capped summary"], 95, 6)
    }
}

/// #2313 (b1b): the OpenAI Chat cap-exit summary is one attempt too.
#[tokio::test]
async fn an_openai_chat_cap_exit_summary_is_one_ledger_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ToolThenCapSummary)
        .mount(&server)
        .await;
    let messages = msgs();
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

    let requests = assert_chat_attempts_equal_wire_requests(&server, &ledger).await;
    let summaries = server
        .received_requests()
        .await
        .expect("journal")
        .iter()
        .filter(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body)
                .is_ok_and(|body| body.get("tools").is_none())
        })
        .count();
    assert_eq!((requests, summaries), (2, 1), "one tool round, one summary");
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

/// #2372: the accepted Chat answer — the string that is returned, persisted
/// and re-sent — follows the reasoning policy the display stream's filter had.
/// An inline `<think>` block never reaches it; a lone `</think>` is answer text
/// unless the backend declares the #528 leading shape.
#[tokio::test]
async fn the_chat_answer_follows_the_declared_reasoning_policy() {
    let undeclared = "End the block with `</think>` and then answer.";
    for (content, leading, expected) in [
        ("<think>x</think>Done.", false, "Done."),
        ("<think>x</think>Done.", true, "Done."),
        (undeclared, false, undeclared),
        ("x</think>Done.", true, "Done."),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(probe_reply(content))
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.emits_leading_reasoning = leading;
        let (reply, _streamed, _usage, _hallu) =
            chat_complete(c, &mut NoMcp).await.expect("dispatch");

        assert_eq!(reply, expected, "{content:?} declared={leading}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[cfg(test)]
#[path = "openai_primary_stream.rs"]
mod primary_stream;
