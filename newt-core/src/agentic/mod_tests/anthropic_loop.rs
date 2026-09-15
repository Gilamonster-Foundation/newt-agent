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

#[cfg(test)]
#[path = "anthropic_context_errors.rs"]
mod context_errors;

/// Set/unset an env var for the test's duration, restoring prior state on
/// drop (env vars are process-global — hence the serial lane above).
pub(super) struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
}
impl EnvGuard {
    pub(super) fn set(key: &'static str, value: &str) -> Self {
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
pub(super) fn test_env(stream: bool) -> Vec<EnvGuard> {
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
        verify_outcomes: false,
        round_cap_hit: None,
        smart_harness: None,
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
    // Calling chat_complete pins the shared dispatch.
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

/// #2312: Anthropic REQUIRES `max_tokens`, so it always carries the resolved
/// allowance. Tiers: explicit allowance > cognition table > wire default
/// (`NEWT_ANTHROPIC_MAX_TOKENS`, else 8192). The env var is the lowest tier.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn explicit_output_allowance_outranks_the_anthropic_env_default() {
    let mut env = test_env(false);
    env.push(EnvGuard::set("NEWT_ANTHROPIC_MAX_TOKENS", "5000"));
    for (output_allowance, sent) in [(None, 5_000), (Some(3_000), 3_000)] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(json_reply(
                "end_turn",
                serde_json::json!([{"type": "text", "text": "capped"}]),
                3,
                2,
            ))
            .mount(&server)
            .await;
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut obs = crate::agentic::SolveObservation::default();
        let mut c = ctx(&uri, &messages, &caveats);
        c.output_allowance = output_allowance;
        c.solve_obs = Some(&mut obs);
        chat_complete(c, &mut NoMcp).await.expect("dispatch");
        let requests = server.received_requests().await.expect("recorded");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            body_json(&requests[0])["max_tokens"],
            sent,
            "{output_allowance:?}"
        );
        // #2312: this wire always sends the cap, so it is server-enforced —
        // the env default included.
        assert_eq!(
            obs.output_allowance,
            Some(crate::agentic::OutputAllowance {
                tokens: sent,
                enforced: crate::agentic::Enforcement::Server,
            })
        );
    }
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
    let (reply, streamed, usage, _) = chat_complete(c, &mut mcp)
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
    assert_eq!(
        usage,
        Some(crate::TokenUsage {
            input_tokens: 90,
            output_tokens: 20,
        })
    );
}

/// Pin the summary-only dispatch contract before sharing its implementation:
/// provider error classification, reasoning parsing, and fallback usage differ.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn final_summary_provider_contracts() {
    let _env = test_env(false);
    let _retry_limit = EnvGuard::set("NEWT_HTTP_MAX_RETRIES", "1");
    for provider in ["ollama", "openai", "anthropic"] {
        for case in ["retry", "fatal", "malformed", "empty", "success"] {
            let server = MockServer::start().await;
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = calls.clone();
            let content = if case == "empty" {
                ""
            } else {
                "<think>inline reasoning</think>Finished."
            };
            // One fixture carries each provider's real content and usage shape.
            let response = serde_json::json!({
                "message": {"content": content},
                "choices": [{"message": {"content": content}}],
                "content": [
                    {"type": "thinking", "thinking": "native reasoning"},
                    {"type": "text", "text": content}
                ],
                "prompt_eval_count": 120, "eval_count": 7,
                "usage": {
                    "prompt_tokens": 120, "completion_tokens": 7,
                    "input_tokens": 120, "output_tokens": 7
                }
            });
            Mock::given(method("POST"))
                .respond_with(move |_: &Request| {
                    let attempt = counted.fetch_add(1, Ordering::SeqCst);
                    match case {
                        "retry" if attempt == 0 => {
                            ResponseTemplate::new(503).set_body_string("busy")
                        }
                        "fatal" => ResponseTemplate::new(400).set_body_string("invalid"),
                        "malformed" => ResponseTemplate::new(200).set_body_string("not JSON"),
                        _ => ResponseTemplate::new(200).set_body_json(&response),
                    }
                })
                .mount(&server)
                .await;
            let accumulated = Some(crate::TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
            });
            let cap = CapExit {
                max_tool_rounds: 2,
                accumulated,
                wasted_calls: 0,
                progress: None,
                observed: Vec::new(),
                request_budget: None,
                calibration: 1.0,
                estimation: crate::tokens::TokenEstimation::default(),
                ollama_options: None,
                prompt_measurement: Default::default(),
                fell_back: Default::default(),
            };
            let client = reqwest::Client::new();
            let url = server.uri();
            let policy = generation_policy::GenerationPolicy::default();
            let result = match provider {
                "ollama" => {
                    final_summary_ollama(&client, &url, "test", Vec::new(), &cap, None).await
                }
                "openai" => {
                    final_summary_openai(
                        (&client, &client),
                        &url,
                        "test",
                        None,
                        Vec::new(),
                        policy,
                        &cap,
                        None,
                    )
                    .await
                }
                _ => {
                    final_summary_anthropic(
                        &client,
                        &url,
                        "test",
                        None,
                        Vec::new(),
                        policy,
                        &cap,
                        None,
                    )
                    .await
                }
            };
            let (reply, streamed, usage) = result.expect("summary failures become fallbacks");
            // Every provider retries a transient 503 once. Until #2313's
            // classifier fix this pinned Ollama's non-retry as a contract: its
            // `Ollama 503 ...` text carried no status the classifier recognised.
            let retried = case == "retry";
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1 + usize::from(retried),
                "{provider}/{case}"
            );
            assert!(!streamed, "{provider}/{case}");
            if case == "success" || retried {
                assert_eq!(
                    reply,
                    if provider == "anthropic" {
                        content
                    } else {
                        "Finished."
                    },
                    "{provider}/{case}"
                );
                assert_eq!(
                    usage,
                    Some(crate::TokenUsage {
                        input_tokens: 120,
                        output_tokens: 27,
                    }),
                    "{provider}/{case}"
                );
            } else {
                assert!(
                    reply.contains("tool-round limit (2"),
                    "{provider}/{case}: {reply}"
                );
                assert_eq!(usage, accumulated, "{provider}/{case}");
            }
        }
    }
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
// #2313 (b1c): every Anthropic primary request is one ledger attempt
// -----------------------------------------------------------------------

/// Assert the #2313 invariant for one Anthropic turn: every received request is
/// on the generation path (`/v1/messages`, so nothing else was hit and the
/// filter hides nothing), and the ledger's attempts are exactly those requests,
/// keyed by their bodies. Returns the records, ordered by ordinal.
///
/// Scope of the count: the Anthropic primary loop's round dispatch (stream and
/// non-stream, including a no-output stream re-issue) and cap-exit summary.
async fn assert_anthropic_attempts_equal_wire_requests(
    server: &MockServer,
    ledger: &std::sync::Mutex<crate::attempts::AttemptLedger>,
) -> Vec<crate::attempts::AttemptRecord> {
    let received = server.received_requests().await.expect("journal");
    assert!(
        received
            .iter()
            .all(|request| request.url.path() == "/v1/messages"),
        "only /v1/messages may be hit: {:?}",
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
    let mut records: Vec<_> = ledger.records().cloned().collect();
    records.sort_by_key(|record| record.key.ordinal);
    for record in &records {
        assert!(
            record.key.turn.starts_with("prompt:"),
            "{}",
            record.key.turn
        );
        assert_eq!(record.key.role, "primary");
    }
    records
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn every_non_streaming_anthropic_round_is_one_ledger_attempt() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(UsageAcrossRounds {
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    let mut mcp = RecordingMcp {
        name: "my_server__get_thing",
        result: "ok",
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    chat_complete(c, &mut mcp).await.expect("dispatch");

    let records = assert_anthropic_attempts_equal_wire_requests(&server, &ledger).await;
    assert_eq!(records.len(), 2, "tool round, final answer");
    assert!(records
        .iter()
        .all(|r| r.state == crate::attempts::AttemptState::Ok));
    let totals = ledger.lock().unwrap().totals();
    assert_eq!((totals.in_tokens, totals.out_tokens), (100 + 120, 10 + 5));
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn a_streamed_anthropic_round_is_one_ledger_attempt() {
    let _env = test_env(true);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse_text_reply(&["Hello ", "world"], 7, 3))
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    chat_complete(c, &mut NoMcp)
        .await
        .expect("streamed dispatch");

    let records = assert_anthropic_attempts_equal_wire_requests(&server, &ledger).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, crate::attempts::AttemptState::Ok);
    assert_eq!(
        records[0].usage,
        Some(crate::TokenUsage {
            input_tokens: 7,
            output_tokens: 3
        })
    );
}

/// Precision (3) on a real loop: a 529 then a success is two requests and two
/// attempts with identical bytes — ordinal 0 `failed` with no usage, ordinal 1
/// `ok`.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn a_retried_anthropic_round_is_one_ledger_attempt_per_try() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(OverloadedOnce {
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    chat_complete(c, &mut NoMcp).await.expect("529 is retried");

    let records = assert_anthropic_attempts_equal_wire_requests(&server, &ledger).await;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].key.request, records[1].key.request);
    assert_eq!(
        (records[0].state, records[0].usage),
        (crate::attempts::AttemptState::Failed, None)
    );
    assert_eq!(records[1].state, crate::attempts::AttemptState::Ok);
}

#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn an_anthropic_cap_exit_summary_is_one_ledger_attempt() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ToolsUntilCap {
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 2;
    c.attempt_ledger = Some(&ledger);
    let mut mcp = RecordingMcp {
        name: "my_server__get_thing",
        result: "ok",
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    let (reply, _, _, _) = chat_complete(c, &mut mcp).await.expect("cap exit");
    assert!(reply.starts_with("capped summary"), "{reply}");

    let records = assert_anthropic_attempts_equal_wire_requests(&server, &ledger).await;
    assert_eq!(
        records.len(),
        3,
        "two tool rounds + one tools-disabled summary"
    );
    assert!(records
        .iter()
        .all(|r| r.state == crate::attempts::AttemptState::Ok));
}

/// A `pause_turn` reply (with usage), then the final answer.
struct PauseThenAnswer {
    calls: Arc<AtomicUsize>,
}
impl Respond for PauseThenAnswer {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            json_reply(
                "pause_turn",
                serde_json::json!([{"type": "text", "text": "thinking so far"}]),
                100,
                8,
            )
        } else {
            json_reply(
                "end_turn",
                serde_json::json!([{"type": "text", "text": "resumed answer"}]),
                110,
                6,
            )
        }
    }
}

/// #2313 (b2): a `pause_turn` reply generated tokens the operator pays for,
/// so its usage joins the turn instead of being dropped by the re-dispatch.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn a_paused_turn_keeps_the_paused_replys_usage() {
    let _env = test_env(false);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(PauseThenAnswer {
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    let (reply, _, usage, _) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(reply, "resumed answer");
    assert_eq!(
        usage,
        Some(crate::TokenUsage {
            input_tokens: 110,
            output_tokens: 14
        }),
        "the paused reply's 8 generated tokens join the turn"
    );
    let records = assert_anthropic_attempts_equal_wire_requests(&server, &ledger).await;
    assert_eq!(records.len(), 2);
    let totals = ledger.lock().unwrap().totals();
    assert_eq!((totals.in_tokens, totals.out_tokens), (210, 14));
}

// -----------------------------------------------------------------------
// #2313 review: streamed Anthropic attempts follow the state rule — a stream
// that reached `message_stop` is ok; a cut stream, an error event, or an
// interrupt is failed; reported usage attaches either way.
// -----------------------------------------------------------------------

fn anthropic_stream_head(input: u64) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"type": "message_start",
            "message": {"model": "claude-test", "usage": {"input_tokens": input}}}),
        serde_json::json!({"type": "content_block_start",
            "index": 0, "content_block": {"type": "text"}}),
    ]
}

async fn streamed_anthropic_turn(
    responder: impl Respond + 'static,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Vec<crate::attempts::AttemptRecord> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(responder)
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut c = ctx(&uri, &messages, &caveats);
    c.attempt_ledger = Some(&ledger);
    c.cancel = cancel;
    let _ = chat_complete(c, &mut NoMcp).await;
    assert_anthropic_attempts_equal_wire_requests(&server, &ledger).await
}

/// Review finding 3: a clean EOF before `message_stop` is a CUT stream, not a
/// completed one.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn a_cut_anthropic_stream_is_a_failed_attempt() {
    let _env = test_env(true);
    let mut frames = anthropic_stream_head(6);
    frames.push(
        serde_json::json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": "half an answ"}}),
    );
    let records = streamed_anthropic_turn(sse(&frames), None).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, crate::attempts::AttemptState::Failed);
}

/// An error event, then a clean stream.
struct StreamErrorThenAnswer {
    calls: Arc<AtomicUsize>,
}
impl Respond for StreamErrorThenAnswer {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut frames = anthropic_stream_head(6);
            frames.push(serde_json::json!({"type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}}));
            sse(&frames)
        } else {
            sse_text_reply(&["recovered"], 7, 2)
        }
    }
}

/// Review finding 5: an error event before any visible text re-issues the same
/// bytes — ordinal 0 failed, ordinal 1 ok.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn an_anthropic_error_event_before_text_is_a_failed_attempt_then_a_retry() {
    let _env = test_env(true);
    let records = streamed_anthropic_turn(
        StreamErrorThenAnswer {
            calls: Arc::new(AtomicUsize::new(0)),
        },
        None,
    )
    .await;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].key.request, records[1].key.request);
    assert_eq!(records[0].state, crate::attempts::AttemptState::Failed);
    assert_eq!(records[1].state, crate::attempts::AttemptState::Ok);
}

/// Review finding 5: an error event after partial text keeps the partial
/// answer, and the attempt is failed.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn an_anthropic_error_event_after_partial_text_is_a_failed_attempt() {
    let _env = test_env(true);
    let mut frames = anthropic_stream_head(6);
    frames.push(
        serde_json::json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": "partial answer before the break"}}),
    );
    frames.push(serde_json::json!({"type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}}));
    let records = streamed_anthropic_turn(sse(&frames), None).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, crate::attempts::AttemptState::Failed);
}

/// Review round 2, item 4: a failure after `message_delta` has complete usage,
/// and the failed attempt keeps it.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn an_anthropic_error_event_after_message_delta_keeps_the_complete_usage() {
    let _env = test_env(true);
    let mut frames = anthropic_stream_head(6);
    frames.push(
        serde_json::json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": "a whole answer"}}),
    );
    frames.push(serde_json::json!({"type": "message_delta",
        "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}));
    frames.push(serde_json::json!({"type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}}));
    let records = streamed_anthropic_turn(sse(&frames), None).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, crate::attempts::AttemptState::Failed);
    assert_eq!(
        records[0].usage,
        Some(crate::TokenUsage {
            input_tokens: 6,
            output_tokens: 3
        })
    );
}

/// Dispatch one streamed Anthropic round against `serve_stream_parts`, bounded
/// so a broken read loop fails instead of hanging. Returns the round, whether
/// text was shown, and the attempts recorded.
async fn raw_anthropic_round(
    parts: &[&[u8]],
    interrupt: bool,
) -> (
    anthropic_wire::AnthropicRound,
    bool,
    Vec<crate::attempts::AttemptRecord>,
) {
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (url, server) =
        super::http_loop_tests::serve_stream_parts(parts, interrupt.then(|| flag.clone())).await;
    let messages_url = format!("{url}/v1/messages");
    let client = reqwest::Client::new();
    let retry = RetryPolicy {
        max_retries: 0,
        base: std::time::Duration::ZERO,
        max: std::time::Duration::ZERO,
        jitter: false,
    };
    let dispatch = AnthropicDispatch {
        smart_harness: None,
        client: &client,
        stream_client: &client,
        messages_url: &messages_url,
        api_key: None,
        retry: &retry,
        color: false,
        markdown: false,
        retain: None,
    };
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let scope = attempt_capture::AttemptScope {
        ledger: &ledger,
        turn: "prompt:turn",
        model: "claude-test",
        backend: "test-backend",
        cancel: Some(flag.as_ref()),
    };
    let (round, started) = tokio::time::timeout(
        super::http_loop_tests::RAW_STREAM_TEST_BOUND,
        anthropic_dispatch_round(
            &dispatch,
            &serde_json::json!({"stream": true}),
            &[],
            true,
            Some(flag.as_ref()),
            Some(scope),
        ),
    )
    .await
    .expect("the read ends: at message_stop, at EOF, or at the interrupt")
    .expect("the round is not an error")
    .expect("the stream was read, so this is not the send-time cancel");
    server.abort();
    let records = ledger.lock().unwrap().records().cloned().collect();
    (round, started, records)
}

fn anthropic_sse_body(frames: &[serde_json::Value]) -> String {
    frames.iter().map(|f| format!("data: {f}\n\n")).collect()
}

/// Review round 2, item 4, and #2313 (c): an Esc inside the stream's read loop,
/// after the first text delta, is a cancelled attempt. Deterministic: the flag is tripped only
/// after the reader has drained megabytes of the body, so the send-time cancel
/// cannot be what fires. Only `message_start` usage arrived, which is no usage
/// (a known limit: `TokenUsage` cannot say "output unknown").
#[tokio::test]
async fn an_anthropic_stream_interrupted_after_its_first_delta_is_a_cancelled_attempt() {
    let mut frames = anthropic_stream_head(6);
    frames.push(
        serde_json::json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": "the beginning"}}),
    );
    let head = anthropic_sse_body(&frames);
    let (round, started, records) = raw_anthropic_round(&[head.as_bytes()], true).await;
    assert!(started, "the first delta was shown");
    assert_eq!(round.text, "the beginning");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, crate::attempts::AttemptState::Cancelled);
    assert_eq!(records[0].usage, None);
}

const SPLIT_TEXT: &str = "newt 🦎 蠑螈";

fn anthropic_split_text_body() -> String {
    let mut frames = anthropic_stream_head(6);
    frames.push(
        serde_json::json!({"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": SPLIT_TEXT}}),
    );
    frames.push(serde_json::json!({"type": "content_block_stop", "index": 0}));
    frames.push(serde_json::json!({"type": "message_delta",
        "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}));
    frames.push(serde_json::json!({"type": "message_stop"}));
    anthropic_sse_body(&frames)
}

/// Review round 4, item b: a character split across two reads is still that
/// character in the Anthropic stream's text. Decoding each chunk on its own
/// turns both halves into U+FFFD.
#[tokio::test]
async fn an_anthropic_character_split_across_reads_is_not_corrupted() {
    let body = anthropic_split_text_body();
    let bytes = body.as_bytes();
    let cut = bytes
        .iter()
        .position(|&b| b == 0xF0)
        .expect("the emoji's lead byte")
        + 2;
    let (round, _, records) = raw_anthropic_round(&[&bytes[..cut], &bytes[cut..]], false).await;
    assert_eq!(round.text, SPLIT_TEXT);
    assert_eq!(records[0].state, crate::attempts::AttemptState::Ok);
}

/// The same property without a socket, which can coalesce the two writes and
/// make the test above vacuous: at every byte offset, `decode_chunk` feeding the
/// Anthropic accumulator yields the text intact.
#[test]
fn an_anthropic_text_delta_split_at_every_byte_offset_is_not_corrupted() {
    let body = anthropic_split_text_body();
    let bytes = body.as_bytes();
    for cut in 0..=bytes.len() {
        let mut carry = Vec::new();
        let mut acc = anthropic_wire::SseAccumulator::new();
        let mut text = String::new();
        for part in [&bytes[..cut], &bytes[cut..]] {
            for action in acc.feed(&decode_chunk(&mut carry, part)) {
                if let anthropic_wire::StreamAction::TextDelta(t) = action {
                    text.push_str(&t);
                }
            }
        }
        assert_eq!(text, SPLIT_TEXT, "split at byte {cut}");
        assert!(acc.is_done(), "split at byte {cut}");
    }
}

/// Trips the interrupt flag as the response is served.
struct CancelWhileServing {
    flag: Arc<std::sync::atomic::AtomicBool>,
}
impl Respond for CancelWhileServing {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        self.flag.store(true, Ordering::SeqCst);
        sse_text_reply(&["never shown"], 7, 2)
    }
}

/// Review finding 5: a cancel while the request is in flight is never an ok
/// attempt. The flag is set before the headers return, so this covers the
/// send-time cancel; the stream's own interrupt arm is pinned by
/// `an_anthropic_stream_interrupted_after_its_first_delta_is_a_cancelled_attempt`.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn an_anthropic_stream_cancelled_mid_flight_is_never_ok() {
    let _env = test_env(true);
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let records = streamed_anthropic_turn(
        CancelWhileServing { flag: flag.clone() },
        Some(flag.as_ref()),
    )
    .await;
    assert!(!records.is_empty(), "the request was sent");
    assert!(records
        .iter()
        .all(|r| r.state != crate::attempts::AttemptState::Ok));
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

    // #2268: an overflow may retry only after shrinking. Preserve the
    // original success/count assertion with removable history.
    let mut messages = msgs();
    messages.insert(1, MemMessage::user("historical context ".repeat(500)));
    messages.insert(2, MemMessage::assistant("earlier reasoning ".repeat(500)));
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
    let requests = server.received_requests().await.unwrap();
    let rejected = body_json(&requests[0]);
    let recovered = body_json(&requests[1]);
    assert!(
        recovered["messages"].to_string().len() < rejected["messages"].to_string().len(),
        "recovery must not resend the rejected request unchanged"
    );
    assert!(recovered["messages"].to_string().contains("do the thing"));
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

/// #2315: the Anthropic funnel records the structured execution class of a
/// real shell call, like the other three loops (`tool_round_cap::tool_events`).
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn anthropic_funnel_records_the_execution_class() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _env = test_env(false);
    let _lock = env_lock().await;
    let _confined = EnvVar::unset("NEWT_DISABLE_OCAP");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(|req: &Request| {
            if String::from_utf8_lossy(&req.body).contains("tool_result") {
                json_reply(
                    "end_turn",
                    serde_json::json!([{"type": "text", "text": "done"}]),
                    10,
                    2,
                )
            } else {
                json_reply(
                    "tool_use",
                    serde_json::json!([{"type": "tool_use", "id": "toolu_1",
                        "name": "run_command",
                        "input": {"command": "sh -c 'echo diag; exit 101'"}}]),
                    10,
                    2,
                )
            }
        })
        .mount(&server)
        .await;
    let ws = tempfile::TempDir::new().unwrap();
    let workspace = ws.path().to_string_lossy().into_owned();
    let (messages, caveats) = (msgs(), Caveats::top());
    let mut events: Vec<crate::ToolEvent> = Vec::new();
    let uri = server.uri();
    let mut context = ctx(&uri, &messages, &caveats);
    context.workspace = &workspace;
    context.action_nudges = false;
    // The shared ctx allow-lists one MCP tool; this test needs the built-in shell.
    context.persona_tools = None;
    context.tool_events = Some(&mut events);
    chat_complete(context, &mut NoMcp)
        .await
        .expect("the turn completes");
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(!events[0].ok, "{events:?}");
    assert_eq!(
        serde_json::to_value(&events[0]).unwrap()["execution"],
        "failed",
        "{events:?}"
    );
}

// -----------------------------------------------------------------------
// #2341: admission reserves the max_tokens this wire sends
// -----------------------------------------------------------------------

/// A prompt too large for any budget, so the refusal names the budget the loop
/// enforced.
fn oversized_task() -> String {
    format!("OUTPUT-RESERVE {}", "x".repeat(200_000))
}

/// #2341: with a declared 32,768-token window and no explicit allowance the
/// loop sends `max_tokens` (8,192, or `NEWT_ANTHROPIC_MAX_TOKENS`), so admission
/// must leave exactly those tokens free — 24,576 input for the default, not the
/// 26,214 percentage ceiling alone — and the reporting seam must agree.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn a_declared_window_reserves_the_max_tokens_anthropic_sends() {
    for (env_max_tokens, budget) in [(None, 24_576), (Some("12000"), 20_768)] {
        let mut env = test_env(false);
        env.push(match env_max_tokens {
            Some(value) => EnvGuard::set("NEWT_ANTHROPIC_MAX_TOKENS", value),
            None => EnvGuard::unset("NEWT_ANTHROPIC_MAX_TOKENS"),
        });
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let task = oversized_task();
        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user(&task),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.task = &task;
        c.num_ctx = Some(32_768);
        let error = chat_complete(c, &mut NoMcp)
            .await
            .expect_err("the prompt cannot fit")
            .to_string();
        assert!(
            error.contains(&format!("authoritative {budget}-token input budget")),
            "{env_max_tokens:?}: {error}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(
            initial_context_input_budget(
                BackendKind::Anthropic,
                crate::OpenAiApi::ChatCompletions,
                Some(32_768),
                80,
                None,
                None,
                Default::default(),
                crate::model_card::ReasoningReplayScope::Never,
                None,
                None,
            ),
            Some(budget),
            "{env_max_tokens:?}: the reporting seam agrees with what admission enforces"
        );
    }
}

/// #2313 (c): Esc while a `/v1/messages` send waits for its response drops the
/// dispatch future, so nothing can settle the attempt; the handle's drop records
/// it cancelled, with no usage. Both the streamed and the non-streamed send.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn an_interrupted_anthropic_send_is_a_cancelled_attempt() {
    for stream in [true, false] {
        let _env = test_env(stream);
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (url, server) =
            super::http_loop_tests::serve_until_interrupted("/v1/messages", flag.clone()).await;
        let messages = msgs();
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
        assert_eq!(records.len(), 1, "stream={stream}");
        assert_eq!(
            (records[0].state, records[0].usage),
            (crate::attempts::AttemptState::Cancelled, None),
            "stream={stream}"
        );
    }
}

/// #2313 (c): a retry storm on the streamed no-output re-issue. Every stream
/// fails before any text, so the loop re-issues the same bytes until the retry
/// budget is spent: `NEWT_HTTP_MAX_RETRIES` + 1 attempts, ordinals 0.., all
/// failed, and not one more.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn an_anthropic_no_output_reissue_storm_is_one_attempt_per_try() {
    let _env = test_env(true);
    let _retries = EnvGuard::set("NEWT_HTTP_MAX_RETRIES", "2");
    let mut frames = anthropic_stream_head(6);
    frames.push(serde_json::json!({"type": "error",
        "error": {"type": "overloaded_error", "message": "Overloaded"}}));
    let mut records = streamed_anthropic_turn(sse(&frames), None).await;
    records.sort_by_key(|r| r.key.ordinal);
    assert_eq!(records.len(), 3, "the first try and two re-issues");
    for (ordinal, record) in records.iter().enumerate() {
        assert_eq!(record.key.ordinal as usize, ordinal);
        assert_eq!(record.key.request, records[0].key.request, "the same bytes");
        assert_eq!(record.state, crate::attempts::AttemptState::Failed);
    }
}
