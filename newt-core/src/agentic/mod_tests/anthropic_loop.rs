//! Anthropic (`/v1/messages`) loop tests — dispatch, native SSE streaming,
//! tool round-trips, stop-reason semantics, and the recovery arms, all
//! against wiremock backends (mirrors the `http_loop.rs` harness idioms).
//!
//! Every test is serialized on one lane (`anthropic_loop_env`): the loop
//! reads process-global env (the `NEWT_ANTHROPIC_STREAM` valve and the
//! `NEWT_HTTP_BACKOFF_*` retry knobs), so concurrent tests could observe
//! each other's guards.

use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Set/unset an env var for the test's duration, restoring prior state on
/// drop (env vars are process-global — hence the serial lane above).
struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, prev }
    }
    fn unset(key: &'static str) -> Self {
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, prev }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.as_deref() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Zero-delay retry envelope + an EXPLICIT streaming-valve state, so tests
/// exercise retries without sleeping and never depend on ambient env.
fn test_env(stream: bool) -> Vec<EnvGuard> {
    let mut guards = vec![
        EnvGuard::set("NEWT_HTTP_BACKOFF_BASE_MS", "0"),
        EnvGuard::set("NEWT_HTTP_BACKOFF_MAX_MS", "0"),
        EnvGuard::set("NEWT_HTTP_JITTER", "0"),
    ];
    guards.push(if stream {
        EnvGuard::unset("NEWT_ANTHROPIC_STREAM")
    } else {
        EnvGuard::set("NEWT_ANTHROPIC_STREAM", "off")
    });
    guards
}

fn msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ]
}

/// The MCP tests here exercise a remote tool's MECHANICS (delta parsing,
/// parallel results, tool round-trip, spill) through `my_server__get_thing` —
/// they are NOT about authorization. Post the `mcp-under-leash` name-grant
/// closure, an MCP call needs a structural grant, so the shared ctx puts that
/// operation on a persona allow-list. `NoMcp` tests are unaffected: they never
/// dispatch an MCP call, and `persona_tools` gates only the MCP path.
fn persona_allow() -> &'static [String] {
    static ALLOW: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| vec!["my_server__get_thing".to_string()]);
    ALLOW.as_slice()
}

/// The loop tests' workspace: a path that deliberately does NOT exist.
///
/// These tests are about the LOOP — nudges, wire shapes, retries, round caps —
/// not about the self-verify gate, which #1943 arms by default. Under
/// `cargo test` the process's `.` is this crate's own directory, which ships a
/// `Cargo.toml`, so an armed gate correctly detects `cargo test` and adds a
/// round to every one of these tests. Pointing them at a workspace that
/// affords no verification keeps each measuring what it is named for, and
/// removes an ambient-filesystem dependency they never wanted (#514).
///
/// The gate's own wiring is NOT left unproved by this — that would recreate,
/// in the test suite, exactly the dark gate #1943 exists to end. It is proved
/// against a workspace that DOES afford a check, by
/// `an_armed_self_verify_gate_adds_a_round_when_the_workspace_ships_a_check`.
const NO_CHECKS_WORKSPACE: &str = "newt-core-test-workspace-that-does-not-exist";

fn ctx<'a>(server_uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    ChatCtx {
        rewrites_history: true,
        url: server_uri,
        model: "claude-test",
        kind: BackendKind::Anthropic,
        api_key: Some("sk-ant-test"),
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
        persona_tools: Some(persona_allow()),
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
        exec_floor: None,
        write_ledger: None,
        attribution: None,
        cancel: None,
        live_tool_output: None,
        git_tool: None,
        crew_runner: None,
        operating_mode_control: None,
        plan_mode_control: None,
        disposition_request_control: None,
        steering: None,
        completed_spill_renderer: None,
    }
}

fn body_json(req: &Request) -> serde_json::Value {
    serde_json::from_slice(&req.body).unwrap_or_default()
}

/// A non-streaming `/v1/messages` 200 body.
fn json_reply(stop: &str, content: serde_json::Value, input: u64, output: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "model": "claude-test",
        "stop_reason": stop,
        "content": content,
        "usage": {"input_tokens": input, "output_tokens": output},
    }))
}

/// An SSE body from `data:` frames (the `event:` lines are redundant — the
/// accumulator keys on the payload's `type`).
fn sse(frames: &[serde_json::Value]) -> ResponseTemplate {
    let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream")
}

/// An SSE stream that answers with plain text and full usage.
fn sse_text_reply(parts: &[&str], input: u64, output: u64) -> ResponseTemplate {
    let mut frames = vec![
        serde_json::json!({"type": "message_start",
            "message": {"model": "claude-test", "usage": {"input_tokens": input}}}),
        serde_json::json!({"type": "content_block_start",
            "index": 0, "content_block": {"type": "text"}}),
    ];
    for p in parts {
        frames.push(serde_json::json!({"type": "content_block_delta",
            "index": 0, "delta": {"type": "text_delta", "text": p}}));
    }
    frames.push(serde_json::json!({"type": "content_block_stop", "index": 0}));
    frames.push(serde_json::json!({"type": "message_delta",
        "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": output}}));
    frames.push(serde_json::json!({"type": "message_stop"}));
    sse(&frames)
}

/// MCP stub that records every argument object it is called with.
struct RecordingMcp {
    name: &'static str,
    result: &'static str,
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
}
#[async_trait::async_trait]
impl McpTools for RecordingMcp {
    fn handles(&self, name: &str) -> bool {
        name == self.name
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    async fn call(&mut self, leased: &LeasedMcpCall<'_>) -> String {
        self.seen.lock().unwrap().push(leased.args().clone());
        self.result.to_string()
    }
}

// -----------------------------------------------------------------------
// 1 + 18: non-streaming dispatch end-to-end, valve honored on the wire
// -----------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn stream_off_dispatches_kind_anthropic_end_to_end() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(json_reply(
            "end_turn",
            serde_json::json!([{"type": "text", "text": "anthropic says hi"}]),
            11,
            5,
        ))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    // Calling chat_complete (not anthropic_chat_complete) pins the dispatch.
    let (reply, streamed, usage, hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("anthropic dispatch should succeed");

    assert_eq!(reply, "anthropic says hi");
    assert!(!streamed, "stream-off mode never prints live");
    let u = usage.expect("usage decoded from the reply");
    assert_eq!((u.input_tokens, u.output_tokens), (11, 5));
    assert_eq!(hallu, 0);
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn stream_off_valve_sends_stream_false_in_the_body() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(json_reply(
            "end_turn",
            serde_json::json!([{"type": "text", "text": "valve honored"}]),
            3,
            2,
        ))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, _, _, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
        .await
        .expect("dispatch");
    assert_eq!(reply, "valve honored");

    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        body_json(&requests[0])["stream"],
        serde_json::json!(false),
        "NEWT_ANTHROPIC_STREAM=off must send stream:false"
    );
}

// -----------------------------------------------------------------------
// 2: SSE streamed text
// -----------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn sse_streamed_text_concatenates_and_reports_usage() {
    let _env = test_env(true);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-ant-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(sse_text_reply(&["Hello ", "world"], 7, 3))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, usage, hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("streamed dispatch should succeed");

    assert_eq!(reply, "Hello world", "deltas accumulated across frames");
    assert!(streamed, "the final answer was printed live via SSE");
    let u = usage.expect("message_start + message_delta usage merged");
    assert_eq!((u.input_tokens, u.output_tokens), (7, 3));
    assert_eq!(hallu, 0);

    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        body_json(&requests[0])["stream"],
        serde_json::json!(true),
        "the default valve state streams"
    );
}

// -----------------------------------------------------------------------
// 3: tool_use round trip — the history replay converts back to the wire
// -----------------------------------------------------------------------

/// Round 1 answers with a tool_use; round 2 ASSERTS the request replays the
/// assistant tool_use block verbatim followed by a user message whose first
/// block is the paired tool_result, then answers end_turn.
struct RoundTripResponder {
    calls: Arc<AtomicUsize>,
}
impl Respond for RoundTripResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return json_reply(
                "tool_use",
                serde_json::json!([
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "toolu_1",
                     "name": "my_server__get_thing", "input": {"key": "value"}},
                ]),
                40,
                9,
            );
        }
        let body = body_json(req);
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let assistant_pos = messages.iter().position(|m| {
            m["role"] == "assistant"
                && m["content"].as_array().is_some_and(|blocks| {
                    blocks.iter().any(|b| {
                        b["type"] == "tool_use"
                            && b["id"] == "toolu_1"
                            && b["name"] == "my_server__get_thing"
                            && b["input"]["key"] == "value"
                    })
                })
        });
        let paired_result = assistant_pos.is_some_and(|i| {
            messages.get(i + 1).is_some_and(|m| {
                m["role"] == "user"
                    && m["content"][0]["type"] == "tool_result"
                    && m["content"][0]["tool_use_id"] == "toolu_1"
                    && m["content"][0]["content"] == "tool-result-text"
            })
        });
        if !paired_result {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error",
                          "message": "round-trip assertion failed: tool_use/tool_result pairing"}
            }));
        }
        json_reply(
            "end_turn",
            serde_json::json!([{"type": "text", "text": "done after tool"}]),
            60,
            4,
        )
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn tool_use_round_trip_replays_blocks_verbatim() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(RoundTripResponder {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let mut mcp = RecordingMcp {
        name: "my_server__get_thing",
        result: "tool-result-text",
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let (reply, _, _, hallu) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut mcp)
        .await
        .expect("tool round trip should succeed");

    assert_eq!(reply, "done after tool");
    assert_eq!(hallu, 0, "a routed MCP call is not a hallucination");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "tool round + final answer");
    assert_eq!(
        mcp.seen.lock().unwrap().as_slice(),
        &[serde_json::json!({"key": "value"})],
        "the executed call carried the decoded object arguments"
    );
}

// -----------------------------------------------------------------------
// 4: parallel tool_use → ONE user message carries both tool_results
// -----------------------------------------------------------------------

struct ParallelResultsResponder {
    calls: Arc<AtomicUsize>,
}
impl Respond for ParallelResultsResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return json_reply(
                "tool_use",
                serde_json::json!([
                    {"type": "tool_use", "id": "toolu_a",
                     "name": "my_server__get_thing", "input": {"n": 1}},
                    {"type": "tool_use", "id": "toolu_b",
                     "name": "my_server__get_thing", "input": {"n": 2}},
                ]),
                50,
                12,
            );
        }
        let body = body_json(req);
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        // Anthropic REQUIRES all parallel-call results in the single next
        // user message. Collect (message index, tool_use_id) for every
        // tool_result block on the wire.
        let mut carriers: Vec<(usize, Vec<String>)> = Vec::new();
        for (i, m) in messages.iter().enumerate() {
            if m["role"] != "user" {
                continue;
            }
            let ids: Vec<String> = m["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b["type"] == "tool_result")
                        .filter_map(|b| b["tool_use_id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if !ids.is_empty() {
                carriers.push((i, ids));
            }
        }
        let ok = carriers.len() == 1 && carriers[0].1 == ["toolu_a", "toolu_b"];
        if !ok {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error",
                          "message": format!("parallel results assertion failed: {carriers:?}")}
            }));
        }
        json_reply(
            "end_turn",
            serde_json::json!([{"type": "text", "text": "both results landed"}]),
            70,
            5,
        )
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn parallel_tool_results_land_in_one_user_message_in_call_order() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ParallelResultsResponder {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let mut mcp = RecordingMcp {
        name: "my_server__get_thing",
        result: "ok",
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let (reply, _, _, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut mcp)
        .await
        .expect("parallel round should succeed");

    assert_eq!(reply, "both results landed");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        mcp.seen.lock().unwrap().len(),
        2,
        "both parallel calls executed"
    );
}

// -----------------------------------------------------------------------
// 5 + 6: streaming tool_use — input_json_delta accumulation, zero-arg calls
// -----------------------------------------------------------------------

/// Round 1 streams a tool_use whose input arrives as `input_json_delta`
/// frames split MID-TOKEN; round 2 streams the final answer.
struct SseToolScript {
    calls: Arc<AtomicUsize>,
    first: Vec<serde_json::Value>,
}
impl Respond for SseToolScript {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            sse(&self.first)
        } else {
            sse_text_reply(&["assembled"], 20, 4)
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn input_json_delta_split_mid_token_executes_with_the_full_object() {
    let _env = test_env(true);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(SseToolScript {
            calls: calls.clone(),
            first: vec![
                serde_json::json!({"type": "message_start",
                    "message": {"model": "claude-test", "usage": {"input_tokens": 30}}}),
                serde_json::json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "tool_use", "id": "toolu_j",
                                      "name": "my_server__get_thing"}}),
                // The JSON splits mid-key and mid-value across frames.
                serde_json::json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"pa"}}),
                serde_json::json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "th\": \"a"}}),
                serde_json::json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": ".rs\"}"}}),
                serde_json::json!({"type": "content_block_stop", "index": 0}),
                serde_json::json!({"type": "message_delta",
                    "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 8}}),
                serde_json::json!({"type": "message_stop"}),
            ],
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let mut mcp = RecordingMcp {
        name: "my_server__get_thing",
        result: "ok",
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let (reply, _, _, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut mcp)
        .await
        .expect("streamed tool round should succeed");

    assert_eq!(reply, "assembled");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        mcp.seen.lock().unwrap().as_slice(),
        &[serde_json::json!({"path": "a.rs"})],
        "the accumulated partial_json parsed to the full object"
    );
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn zero_argument_tool_use_executes_with_an_empty_object() {
    let _env = test_env(true);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(SseToolScript {
            calls: calls.clone(),
            first: vec![
                serde_json::json!({"type": "message_start",
                    "message": {"model": "claude-test", "usage": {"input_tokens": 15}}}),
                // No input_json_delta at all — a zero-argument call.
                serde_json::json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "tool_use", "id": "toolu_z",
                                      "name": "my_server__get_thing"}}),
                serde_json::json!({"type": "content_block_stop", "index": 0}),
                serde_json::json!({"type": "message_delta",
                    "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 3}}),
                serde_json::json!({"type": "message_stop"}),
            ],
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let mut mcp = RecordingMcp {
        name: "my_server__get_thing",
        result: "ok",
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let (reply, _, _, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut mcp)
        .await
        .expect("zero-arg tool round should succeed");

    assert_eq!(reply, "assembled");
    assert_eq!(
        mcp.seen.lock().unwrap().as_slice(),
        &[serde_json::json!({})],
        "no deltas → empty-object arguments"
    );
}

// -----------------------------------------------------------------------
// 7: tool-round cap → the final summary request advertises NO tools
// -----------------------------------------------------------------------

/// Answers every tools-carrying request with a fresh tool_use; the
/// tools-disabled cap-exit summary (no `tools` key) gets the final text.
struct ToolsUntilCap {
    calls: Arc<AtomicUsize>,
}
impl Respond for ToolsUntilCap {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let body = body_json(req);
        if body.get("tools").is_none() {
            if body.get("tool_choice").is_some() {
                return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "type": "error",
                    "error": {"type": "invalid_request_error",
                              "message": "tool_choice without tools on the summary request"}
                }));
            }
            return json_reply(
                "end_turn",
                serde_json::json!([{"type": "text", "text": "capped summary"}]),
                90,
                6,
            );
        }
        json_reply(
            "tool_use",
            serde_json::json!([
                {"type": "tool_use", "id": format!("toolu_{n}"),
                 "name": "my_server__get_thing", "input": {"n": n}},
            ]),
            40 + n as u64,
            7,
        )
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn tool_round_cap_summary_request_has_no_tools_key() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ToolsUntilCap {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 2;
    let mut end_reason = None;
    c.end_reason = Some(&mut end_reason);
    let mut mcp = RecordingMcp {
        name: "my_server__get_thing",
        result: "ok",
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let (reply, streamed, _, _) = chat_complete(c, &mut mcp)
        .await
        .expect("cap exit should produce the summary");

    assert!(reply.starts_with("capped summary"), "{reply}");
    assert!(!streamed, "the cap-exit summary is stream:false");
    assert_eq!(end_reason, Some(crate::TurnEndReason::RoundCap));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "two tool rounds + one tools-disabled summary"
    );

    let requests = server.received_requests().await.expect("recorded");
    let last = body_json(requests.last().expect("summary request"));
    assert!(last.get("tools").is_none(), "no tools on the summary");
    assert!(last.get("tool_choice").is_none(), "no tool_choice either");
    assert_eq!(last["stream"], serde_json::json!(false));
}

// -----------------------------------------------------------------------
// 8: system coalescing — one top-level `system` string, no system roles
// -----------------------------------------------------------------------

struct SystemShapeResponder;
impl Respond for SystemShapeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let system = body["system"].as_str().unwrap_or_default();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let no_system_roles = !messages.iter().any(|m| m["role"] == "system");
        let first_is_user = messages.first().is_some_and(|m| m["role"] == "user");
        if !(system.contains("sys one")
            && system.contains("sys two")
            && no_system_roles
            && first_is_user)
        {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error",
                          "message": "system-coalescing assertion failed"}
            }));
        }
        json_reply(
            "end_turn",
            serde_json::json!([{"type": "text", "text": "coalesced ok"}]),
            10,
            2,
        )
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn multiple_system_messages_coalesce_into_top_level_system() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(SystemShapeResponder)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("sys one"),
        MemMessage::system("sys two"),
        MemMessage::user("hello"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.task = "hello";
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("coalesced request should be accepted");
    assert_eq!(reply, "coalesced ok");
}

// -----------------------------------------------------------------------
// 9: narration nudge → the re-dispatch keeps strict user/assistant
//    alternation on the wire
// -----------------------------------------------------------------------

struct AlternationResponder {
    calls: Arc<AtomicUsize>,
}
impl Respond for AlternationResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Phrasing the classifier reads as pending-action (mirrors the
            // http_loop narration tests).
            return json_reply(
                "end_turn",
                serde_json::json!([{"type": "text", "text": "Let me edit the file now."}]),
                20,
                6,
            );
        }
        let body = body_json(req);
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        let alternates = roles.windows(2).all(|w| w[0] != w[1]);
        let has_assistant = roles.contains(&"assistant");
        if !alternates || !has_assistant || roles.first() != Some(&"user") {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error",
                          "message": format!("alternation assertion failed: {roles:?}")}
            }));
        }
        json_reply(
            "end_turn",
            serde_json::json!([{"type": "text",
                               "text": "All done — the edit is complete."}]),
            25,
            5,
        )
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn narration_nudge_redispatch_keeps_strict_alternation() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(AlternationResponder {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, _, _, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
        .await
        .expect("nudged turn should complete");

    assert_eq!(calls.load(Ordering::SeqCst), 2, "one nudge re-dispatch");
    assert!(
        reply.contains("complete"),
        "returns the post-nudge answer: {reply}"
    );
}

// -----------------------------------------------------------------------
// 10: 529 overloaded is retried, then succeeds
// -----------------------------------------------------------------------

struct OverloadedOnce {
    calls: Arc<AtomicUsize>,
}
impl Respond for OverloadedOnce {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(529).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}
            }))
        } else {
            json_reply(
                "end_turn",
                serde_json::json!([{"type": "text", "text": "recovered after overload"}]),
                14,
                4,
            )
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn overloaded_529_is_retried_then_succeeds() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(OverloadedOnce {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, _, _, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
        .await
        .expect("529 must be retried, not surfaced");

    assert_eq!(reply, "recovered after overload");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one retry");
}

// -----------------------------------------------------------------------
// 11: 400 invalid_request is fatal — no retry, server message surfaces
// -----------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn invalid_request_400_is_fatal_and_surfaces_the_message() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error",
                      "message": "max_tokens: field required"}
        })))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let err = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
        .await
        .expect_err("a 400 must be fatal");
    assert!(
        err.to_string().contains("max_tokens: field required"),
        "the server's message surfaces: {err}"
    );

    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 1, "no retry on a fatal 400");
}

// -----------------------------------------------------------------------
// 12: refusal — honest placeholder, no retry, no tool dispatch
// -----------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn refusal_with_empty_content_returns_the_honest_placeholder() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(json_reply("refusal", serde_json::json!([]), 9, 1))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _, hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("a refusal is NOT an error");

    assert_eq!(reply, "the model declined this request (refusal)");
    assert!(!streamed);
    assert_eq!(hallu, 0);
    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 1, "no retry and no tool round on refusal");
}

// -----------------------------------------------------------------------
// 13: max_tokens stop returns the truncated text
// -----------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn max_tokens_stop_returns_the_truncated_text() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(json_reply(
            "max_tokens",
            serde_json::json!([{"type": "text", "text": "The list has 3 entries."}]),
            12,
            8,
        ))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, _, _, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
        .await
        .expect("a length stop with text is accepted");
    assert_eq!(reply, "The list has 3 entries.");
}

// -----------------------------------------------------------------------
// 14: usage across rounds — max input, summed output
// -----------------------------------------------------------------------

struct UsageAcrossRounds {
    calls: Arc<AtomicUsize>,
}
impl Respond for UsageAcrossRounds {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            json_reply(
                "tool_use",
                serde_json::json!([
                    {"type": "tool_use", "id": "toolu_u1",
                     "name": "my_server__get_thing", "input": {"n": 1}},
                ]),
                100,
                10,
            )
        } else {
            json_reply(
                "end_turn",
                serde_json::json!([{"type": "text", "text": "usage merged"}]),
                120,
                5,
            )
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn usage_across_rounds_takes_max_input_and_sums_output() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(UsageAcrossRounds {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let mut mcp = RecordingMcp {
        name: "my_server__get_thing",
        result: "ok",
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let (reply, _, usage, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut mcp)
        .await
        .expect("dispatch");

    assert_eq!(reply, "usage merged");
    let u = usage.expect("accumulated usage");
    // Step 18.1 semantics via `merge_round_usage`: input = the LARGEST single
    // prompt (each round re-includes all prior history — summing would
    // double-count), output = the SUM (each completion is new generation).
    assert_eq!(u.input_tokens, 120, "max(100, 120), not the sum");
    assert_eq!(u.output_tokens, 15, "10 + 5");
}

// -----------------------------------------------------------------------
// 15: mid-stream error event after partial text → partial survives (#640)
// -----------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn mid_stream_error_after_partial_text_keeps_the_partial_answer() {
    let _env = test_env(true);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse(&[
            serde_json::json!({"type": "message_start",
                "message": {"model": "claude-test", "usage": {"input_tokens": 6}}}),
            serde_json::json!({"type": "content_block_start",
                "index": 0, "content_block": {"type": "text"}}),
            serde_json::json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta",
                          "text": "partial answer before the break"}}),
            serde_json::json!({"type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ]))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("the partial answer is accepted, not errored");

    assert_eq!(reply, "partial answer before the break");
    assert!(streamed, "the partial text was already printed live");
    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(
        requests.len(),
        1,
        "visible output means NO re-issue (a retry would re-print)"
    );
}

// -----------------------------------------------------------------------
// 16: cw-400 ("prompt is too long") compacts and retries once
// -----------------------------------------------------------------------

struct OverflowThenOk {
    calls: Arc<AtomicUsize>,
}
impl Respond for OverflowThenOk {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error",
                          "message": "prompt is too long: 200000 tokens > 100000 maximum"}
            }))
        } else {
            json_reply(
                "end_turn",
                serde_json::json!([{"type": "text", "text": "recovered after compaction"}]),
                80,
                6,
            )
        }
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn context_window_400_compacts_and_retries() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(OverflowThenOk {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    // The shared parse-only hook (also used by the OpenAI loops) reads
    // Anthropic's "prompt is too long: N tokens > M maximum" body.
    c.recover_cw_400 = Some(recover_context_window_400);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("the cw-400 must recover, not surface");

    assert_eq!(reply, "recovered after compaction");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "overflow → compact → exactly one retried dispatch"
    );
}

// -----------------------------------------------------------------------
// 17: wire tool shape — input_schema, object tool_choice, no `function`
// -----------------------------------------------------------------------

struct ToolShapeResponder;
impl Respond for ToolShapeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let tools = body["tools"].as_array().cloned().unwrap_or_default();
        let shape_ok = !tools.is_empty()
            && tools.iter().all(|t| {
                t["name"].as_str().is_some_and(|n| !n.is_empty())
                    && t["input_schema"].is_object()
                    && t.get("function").is_none()
                    && t.get("type").is_none()
            });
        let choice_ok = body["tool_choice"] == serde_json::json!({"type": "auto"});
        if !shape_ok || !choice_ok {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error",
                          "message": "tool-shape assertion failed"}
            }));
        }
        json_reply(
            "end_turn",
            serde_json::json!([{"type": "text", "text": "tools shape ok"}]),
            10,
            2,
        )
    }
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn advertised_tools_carry_input_schema_and_object_tool_choice() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ToolShapeResponder)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, _, _, _) = chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
        .await
        .expect("well-shaped tools should be accepted");
    assert_eq!(reply, "tools shape ok");
}
