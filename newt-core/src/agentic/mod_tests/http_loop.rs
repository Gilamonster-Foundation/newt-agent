use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ]
}

fn ctx<'a>(server_uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    ChatCtx {
        rewrites_history: true,
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

/// Probe (stream:false) answers with plain content; the streaming re-issue
/// (stream:true) returns NDJSON tokens with usage on the `done` chunk.
struct StreamHappyResponder;
impl Respond for StreamHappyResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            ndjson(&[
                serde_json::json!({"message": {"content": "Hello "}, "done": false}),
                serde_json::json!({
                    "message": {"content": "world"}, "done": true,
                    "prompt_eval_count": 7, "eval_count": 3
                }),
            ])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "probe answer"},
                "prompt_eval_count": 5, "eval_count": 2,
            }))
        }
    }
}

#[tokio::test]
async fn ollama_streams_final_answer_and_merges_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(StreamHappyResponder)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, usage, hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

    assert_eq!(reply, "Hello world", "tokens accumulated across chunks");
    assert!(streamed, "the streaming path printed the tokens");
    let u = usage.expect("probe + stream usage merged");
    // SEMANTICS CHANGED in Step 18.1: both requests carried the same
    // conversation, so input is max(5, 7) = 7 — the old sum (12) counted
    // the shared history twice. Output is still 2 + 3 (new generation).
    assert_eq!(u.input_tokens, 7, "max(5 probe, 7 stream), not the sum");
    assert_eq!(u.output_tokens, 5, "2 (probe) + 3 (stream)");
    assert_eq!(hallu, 0);
}

/// Records whether every inference request preserves normal multi-turn wire
/// ordering while also carrying the protected recovery copy near the system
/// prompt.
struct ActivePromptWireOrderResponder {
    exact_task: &'static str,
    requests: Arc<AtomicUsize>,
    order_valid: Arc<AtomicBool>,
}

impl Respond for ActivePromptWireOrderResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let card_index = messages.iter().position(|message| {
            message["role"].as_str() == Some("system")
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(prompt_read::ACTIVE_PROMPT_PREFIX))
        });
        let protected_copy_is_earlier = card_index.is_some_and(|index| {
            index + 1 < messages.len().saturating_sub(1)
                && messages[index + 1]["role"].as_str() == Some("user")
                && messages[index + 1]["content"].as_str() == Some(self.exact_task)
        });
        let live_task_is_newest = messages.last().is_some_and(|message| {
            message["role"].as_str() == Some("user")
                && message["content"].as_str() == Some(self.exact_task)
        });
        let exact_task_copies = messages
            .iter()
            .filter(|message| {
                message["role"].as_str() == Some("user")
                    && message["content"].as_str() == Some(self.exact_task)
            })
            .count();
        self.order_valid.fetch_and(
            protected_copy_is_earlier && live_task_is_newest && exact_task_copies == 2,
            Ordering::SeqCst,
        );
        self.requests.fetch_add(1, Ordering::SeqCst);

        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": "wire order preserved"},
                "done": true,
                "prompt_eval_count": 20,
                "eval_count": 4
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "wire order preserved"},
                "prompt_eval_count": 20,
                "eval_count": 4
            }))
        }
    }
}

#[tokio::test]
async fn multi_turn_wire_keeps_live_operator_task_newest_after_protected_copy() {
    const CURRENT_TASK: &str = "CURRENT OPERATOR TASK: report the exact wire ordering";
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let order_valid = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ActivePromptWireOrderResponder {
            exact_task: CURRENT_TASK,
            requests: requests.clone(),
            order_valid: order_valid.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("an older operator request"),
        MemMessage::assistant("the older request is complete"),
        MemMessage::user(CURRENT_TASK),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.task = CURRENT_TASK;
    let (reply, streamed, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("ordinary multi-turn request should complete");

    assert_eq!(reply, "wire order preserved");
    assert!(streamed);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "both the probe and streaming reissue reached the wire"
    );
    assert!(
        order_valid.load(Ordering::SeqCst),
        "every request must keep the protected copy earlier and the live task newest"
    );
}

#[test]
fn detects_tools_unsupported_400_phrasings() {
    assert!(is_tools_unsupported_error(&anyhow::anyhow!(
        "Ollama 400 Bad Request: registry.ollama.ai/library/deepseek-r1:70b does not support tools"
    )));
    // Looser OpenAI-compatible phrasing.
    assert!(is_tools_unsupported_error(&anyhow::anyhow!(
        "this model does not support tools at this time"
    )));
    // Unrelated 400s must NOT trip the no-tools path.
    assert!(!is_tools_unsupported_error(&anyhow::anyhow!(
        "Ollama 400 Bad Request: context window exceeded"
    )));
}

#[test]
fn detects_ollama_tool_xml_parser_errors() {
    assert!(is_ollama_tool_xml_error(&anyhow::anyhow!(
        "{}",
        r#"Ollama 500 Internal Server Error: {"error":"XML syntax error on line 7: element \u003cparameter\u003e closed by \u003c/function\u003e"}"#
    )));
    assert!(is_ollama_tool_xml_error(&anyhow::anyhow!(
        "{}",
        r#"Ollama 500 Internal Server Error: {"error":"XML syntax error on line 2: element <parameter> closed by </function>"}"#
    )));
    assert!(is_ollama_tool_xml_error(&anyhow::anyhow!(
        "{}",
        r#"Ollama 500 Internal Server Error: {"error":"XML syntax error on line 3: unexpected end element \u003c/parameter\u003e"}"#
    )));
    assert!(!is_ollama_tool_xml_error(&anyhow::anyhow!(
        "Ollama 500 Internal Server Error: model runner crashed"
    )));
    assert!(!is_ollama_tool_xml_error(&anyhow::anyhow!(
        "OpenAI 400 Bad Request: XML syntax error in user supplied file"
    )));
}

/// A model that rejects the `tools` field (deepseek-r1) 400s on the first
/// dispatch; newt must drop tools and re-dispatch, answering normally. The
/// tools-absent retry is the one that succeeds — no tools-400 loop.
struct NoToolsResponder {
    rejections: Arc<AtomicUsize>,
    served_without_tools: Arc<AtomicBool>,
}
impl Respond for NoToolsResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let has_tools = body_json(req).get("tools").is_some();
        if has_tools {
            self.rejections.fetch_add(1, Ordering::SeqCst);
            return ResponseTemplate::new(400).set_body_string(
                "registry.ollama.ai/library/deepseek-r1:70b does not support tools",
            );
        }
        self.served_without_tools.store(true, Ordering::SeqCst);
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": "hello there"}, "done": true,
                "prompt_eval_count": 4, "eval_count": 2
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "probe answer"},
                "prompt_eval_count": 4, "eval_count": 2,
            }))
        }
    }
}

#[tokio::test]
async fn no_tools_model_recovers_by_dropping_tools() {
    let server = MockServer::start().await;
    let rejections = Arc::new(AtomicUsize::new(0));
    let served_without_tools = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(NoToolsResponder {
            rejections: rejections.clone(),
            served_without_tools: served_without_tools.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("a no-tools model still answers a bare prompt");

    assert_eq!(reply, "hello there", "the tools-absent retry answered");
    assert!(streamed);
    assert!(
        served_without_tools.load(Ordering::SeqCst),
        "a request without the tools field was eventually served"
    );
    assert_eq!(
        rejections.load(Ordering::SeqCst),
        1,
        "exactly one tools-bearing request 400s — the drop is self-limiting"
    );
}

/// Ollama can 500 before returning assistant content when its XML parser
/// sees malformed Qwen-style tool-call tags. That is not the same as
/// "model does not support tools": Newt should retry with tools still
/// advertised so the model can make forward progress on the next round.
struct MalformedToolXmlResponder {
    rejections: Arc<AtomicUsize>,
    served_with_tools_after_error: Arc<AtomicBool>,
    served_without_tools: Arc<AtomicBool>,
}
impl Respond for MalformedToolXmlResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            if self.rejections.fetch_add(1, Ordering::SeqCst) == 0 {
                return ResponseTemplate::new(500).set_body_json(serde_json::json!({
                    "error": "XML syntax error on line 7: element <parameter> closed by </function>"
                }));
            }
            self.served_with_tools_after_error
                .store(true, Ordering::SeqCst);
            if is_stream(req) {
                return ndjson(&[serde_json::json!({
                    "message": {"content": "recovered with tools still available"},
                    "done": true,
                    "prompt_eval_count": 4,
                    "eval_count": 3
                })]);
            }
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "probe answer with tools still available"},
                "prompt_eval_count": 4,
                "eval_count": 3,
            }));
        }
        self.served_without_tools.store(true, Ordering::SeqCst);
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": "unexpected no-tools stream"},
                "done": true,
                "prompt_eval_count": 4, "eval_count": 3
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "unexpected no-tools probe"},
                "prompt_eval_count": 4, "eval_count": 3,
            }))
        }
    }
}

#[test]
fn malformed_tool_xml_responder_flags_unexpected_no_tools_request() {
    let rejections = Arc::new(AtomicUsize::new(0));
    let served_with_tools_after_error = Arc::new(AtomicBool::new(false));
    let served_without_tools = Arc::new(AtomicBool::new(false));
    let responder = MalformedToolXmlResponder {
        rejections: rejections.clone(),
        served_with_tools_after_error: served_with_tools_after_error.clone(),
        served_without_tools: served_without_tools.clone(),
    };
    let req = Request {
        url: "http://localhost/api/chat".parse().unwrap(),
        method: "POST".parse().unwrap(),
        headers: Default::default(),
        body: serde_json::json!({ "stream": false })
            .to_string()
            .into_bytes(),
    };

    let _response = responder.respond(&req);

    assert!(
        served_without_tools.load(Ordering::SeqCst),
        "the defensive no-tools branch should be observable"
    );
    assert_eq!(rejections.load(Ordering::SeqCst), 0);
    assert!(!served_with_tools_after_error.load(Ordering::SeqCst));
}

#[tokio::test]
async fn ollama_tool_xml_error_recovers_with_tools_still_available() {
    let server = MockServer::start().await;
    let rejections = Arc::new(AtomicUsize::new(0));
    let served_with_tools_after_error = Arc::new(AtomicBool::new(false));
    let served_without_tools = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(MalformedToolXmlResponder {
            rejections: rejections.clone(),
            served_with_tools_after_error: served_with_tools_after_error.clone(),
            served_without_tools: served_without_tools.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("malformed XML tool-call parser errors should retry with tools");

    assert_eq!(reply, "recovered with tools still available");
    assert!(streamed);
    assert!(
        served_with_tools_after_error.load(Ordering::SeqCst),
        "a tools-bearing request was served after the XML parser failure"
    );
    assert!(
        !served_without_tools.load(Ordering::SeqCst),
        "malformed XML must not disable tools for the turn"
    );
    assert_eq!(
        rejections.load(Ordering::SeqCst),
        3,
        "the XML error probe, retry probe, and streaming re-issue all keep tools advertised"
    );
}

/// The streaming re-issue produces no tokens — the loop must fall back to
/// the probe round's content rather than returning silence.
struct EmptyStreamResponder;
impl Respond for EmptyStreamResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            ndjson(&[serde_json::json!({"message": {"content": ""}, "done": true})])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "probe says hi"},
                "prompt_eval_count": 5, "eval_count": 2,
            }))
        }
    }
}

#[tokio::test]
async fn empty_stream_falls_back_to_probe_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(EmptyStreamResponder)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

    assert_eq!(reply, "probe says hi");
    assert!(!streamed, "fallback content was never streamed");
    assert_eq!(usage.unwrap().input_tokens, 5);
}

/// Regression for the DGX wedge: the non-streamed probe said "Let me verify
/// by looking...", then the streaming re-issue returned no tokens. The probe
/// fallback must still go through the no-tool nudge gate instead of ending
/// the turn and forcing the operator to type "continue".
struct EmptyStreamPendingActionResponder {
    probes: Arc<AtomicUsize>,
}
impl Respond for EmptyStreamPendingActionResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            return ndjson(&[serde_json::json!({
                "message": {"content": ""},
                "done": true,
                "prompt_eval_count": 6,
                "eval_count": 0
            })]);
        }
        let probe = self.probes.fetch_add(1, Ordering::SeqCst);
        let content = if probe == 0 {
            "Now I understand the issue. Let me verify by looking at format_rollup_detail."
        } else {
            "Verified after the automatic continue."
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": content},
            "prompt_eval_count": 5 + probe as u32,
            "eval_count": 2,
        }))
    }
}

#[tokio::test]
async fn empty_stream_probe_fallback_pending_action_nudges_and_continues() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(EmptyStreamPendingActionResponder {
            probes: probes.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should auto-continue after pending probe fallback");

    assert_eq!(
        probes.load(Ordering::SeqCst),
        2,
        "the nudge ran a second probe"
    );
    assert_eq!(reply, "Verified after the automatic continue.");
    assert!(!streamed, "the second answer also came from probe fallback");
    assert!(
        !reply.contains("Let me verify"),
        "must not return the pending-action narration"
    );
}

/// Probe AND stream both empty, with no safe-context hint → the loop gives
/// the explicit empty-response diagnostic instead of silence.
struct AllEmptyResponder;
impl Respond for AllEmptyResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            ndjson(&[serde_json::json!({"message": {"content": ""}, "done": true})])
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": ""}}))
        }
    }
}

#[tokio::test]
async fn fully_empty_response_yields_diagnostic_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(AllEmptyResponder)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

    assert!(
        reply.contains("model returned an empty response"),
        "got: {reply}"
    );
    assert!(reply.contains("newt doctor"), "points at diagnostics");
    assert!(!streamed);
}

struct SuspiciousEmptyThenRecover {
    probes: Arc<AtomicUsize>,
    saw_nudge: Arc<AtomicBool>,
}
impl Respond for SuspiciousEmptyThenRecover {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            if self.probes.load(Ordering::SeqCst) <= 1 {
                ndjson(&[serde_json::json!({
                    "message": {"content": ""},
                    "done": true,
                    "prompt_eval_count": 9,
                    "eval_count": 4
                })])
            } else {
                ndjson(&[
                    serde_json::json!({"message": {"content": "recovered "}, "done": false}),
                    serde_json::json!({
                        "message": {"content": "after empty retry"},
                        "done": true,
                        "prompt_eval_count": 5,
                        "eval_count": 3
                    }),
                ])
            }
        } else {
            let body = body_json(req);
            if body["messages"].as_array().into_iter().flatten().any(|m| {
                m["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("no assistant-visible content")
            }) {
                self.saw_nudge.store(true, Ordering::SeqCst);
            }
            let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "thinking": "I know what to do but did not emit final text."
                    },
                    "prompt_eval_count": 10,
                    "eval_count": 2559,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered after empty retry"},
                    "prompt_eval_count": 5,
                    "eval_count": 3,
                }))
            }
        }
    }
}

#[tokio::test]
async fn suspicious_empty_generated_output_retries_with_nudge() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    let saw_nudge = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SuspiciousEmptyThenRecover {
            probes: probes.clone(),
            saw_nudge: saw_nudge.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

    assert_eq!(reply, "recovered after empty retry");
    assert!(streamed);
    assert_eq!(probes.load(Ordering::SeqCst), 2);
    assert!(saw_nudge.load(Ordering::SeqCst));
    assert!(
        usage
            .expect("usage survives suspicious retry")
            .output_tokens
            >= 2566,
        "usage from the suspicious empty round must be preserved"
    );
}

struct SuspiciousEmptyTwiceThenRecover {
    probes: Arc<AtomicUsize>,
    saw_strong_nudge: Arc<AtomicBool>,
}
impl Respond for SuspiciousEmptyTwiceThenRecover {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            if self.probes.load(Ordering::SeqCst) <= 2 {
                ndjson(&[serde_json::json!({
                    "message": {"content": ""},
                    "done": true,
                    "prompt_eval_count": 9,
                    "eval_count": 4
                })])
            } else {
                ndjson(&[serde_json::json!({
                    "message": {"content": "recovered after strong hidden-only nudge"},
                    "done": true,
                    "prompt_eval_count": 5,
                    "eval_count": 3
                })])
            }
        } else {
            let body = body_json(req);
            if body["messages"].as_array().into_iter().flatten().any(|m| {
                m["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Hidden thinking is not an action")
            }) {
                self.saw_strong_nudge.store(true, Ordering::SeqCst);
            }
            let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= 2 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "thinking": "I know the next action but did not emit it."
                    },
                    "prompt_eval_count": 10,
                    "eval_count": 2559,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered after strong hidden-only nudge"},
                    "prompt_eval_count": 5,
                    "eval_count": 3,
                }))
            }
        }
    }
}

#[tokio::test]
async fn repeated_thinking_only_gets_stronger_second_nudge() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    let saw_strong_nudge = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SuspiciousEmptyTwiceThenRecover {
            probes: probes.clone(),
            saw_strong_nudge: saw_strong_nudge.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("second hidden-only nudge should recover the turn");

    assert_eq!(reply, "recovered after strong hidden-only nudge");
    assert!(streamed);
    assert_eq!(probes.load(Ordering::SeqCst), 3);
    assert!(saw_strong_nudge.load(Ordering::SeqCst));
}

struct SuspiciousEmptyStaysEmpty;
impl Respond for SuspiciousEmptyStaysEmpty {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": ""},
                "done": true,
                "prompt_eval_count": 9,
                "eval_count": 4
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "reasoning_content": "internal-only response"
                },
                "prompt_eval_count": 10,
                "eval_count": 12,
            }))
        }
    }
}

#[tokio::test]
async fn suspicious_empty_generated_output_reports_targeted_diagnostic() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SuspiciousEmptyStaysEmpty)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.trace = true;
    let (reply, streamed, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(reply.contains("generated output tokens"), "got: {reply}");
    assert!(
        reply.contains("reasoning_content"),
        "diagnostic should name the non-content field: {reply}"
    );
    assert!(reply.contains("--trace"), "points at trace diagnostics");
    assert!(!streamed);
}

/// First round: empty content with token usage near the safe-context
/// ceiling → the loop must emit the overflow notice, trim, and retry.
/// Second round: a real answer.
struct OverflowThenRecover {
    probes: Arc<AtomicUsize>,
    /// Reported prompt size of the empty overflow round — set ≥85% of the
    /// safe-context window so the silent-overflow gate fires.
    overflow_prompt: u32,
}
impl Respond for OverflowThenRecover {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let overflow_prompt = self.overflow_prompt;
        if is_stream(req) {
            // Streams mirror the probe sequence: empty first, content after.
            if self.probes.load(Ordering::SeqCst) <= 1 {
                ndjson(&[serde_json::json!({
                    "message": {"content": ""}, "done": true,
                    "prompt_eval_count": overflow_prompt, "eval_count": 1
                })])
            } else {
                ndjson(&[
                    serde_json::json!({"message": {"content": "recovered "}, "done": false}),
                    serde_json::json!({
                        "message": {"content": "after trim"}, "done": true,
                        "prompt_eval_count": 12, "eval_count": 4
                    }),
                ])
            }
        } else {
            let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": ""},
                    "prompt_eval_count": overflow_prompt, "eval_count": 1,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered after trim"},
                    "prompt_eval_count": 12, "eval_count": 4,
                }))
            }
        }
    }
}

#[tokio::test]
async fn context_overflow_trims_and_retries_then_recovers() {
    let server = MockServer::start().await;
    // Derive the safe window from the live catalog: the exact active prompt and
    // expanded tool catalog must fit, so reserve ~311 tokens of headroom above
    // the catalog (a catalog-INDEPENDENT figure for the tiny messages/card) as
    // the window. The empty round's reported prompt is then pinned at 88% of
    // that window — comfortably ≥85% — so the silent-overflow gate keeps firing
    // as the catalog grows. (Reproduces the historical 4,096 window / ~3,600
    // report at today's catalog size.)
    // (Step 18.1: the check compares the largest single prompt against the
    // window — the old multi-round sum, 180 here, inflated past 85% after
    // two rounds on EVERY long turn, firing spurious overflow retries.)
    let safe_context = (builtin_catalog_tokens(PromptDisposition::Act)
        + prompt_read::response_repository_policy_tokens()
        + 311) as u32;
    let overflow_prompt = safe_context * 88 / 100; // ≥85% of the window
    let probes = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OverflowThenRecover {
            probes: probes.clone(),
            overflow_prompt,
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.safe_context = Some(safe_context);
    let (reply, streamed, usage, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert_eq!(
        probes.load(Ordering::SeqCst),
        2,
        "overflow must trigger exactly one trim-and-retry probe"
    );
    assert_eq!(reply, "recovered after trim");
    assert!(streamed);
    assert_eq!(
        usage
            .expect("accumulated usage survives the retry")
            .input_tokens,
        overflow_prompt,
        "largest single prompt across the overflowed + recovered rounds"
    );
}

/// Tool calls every round with a tiny trim threshold: the mid-loop
/// compression must fire — observable as the compaction marker (NOT the
/// old amputation placeholder) reaching the model. With no summarizer
/// injected, this is the static-fallback path (Step 18.4).
struct TrimObservingResponder {
    marker_seen: Arc<AtomicBool>,
    old_placeholder_seen: Arc<AtomicBool>,
}
impl Respond for TrimObservingResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let contains = |needle: &str| {
            body["messages"]
                .as_array()
                .map(|m| {
                    m.iter().any(|msg| {
                        msg["content"]
                            .as_str()
                            .map(|c| c.contains(needle))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        };
        if contains(SUMMARY_PREFIX) && contains("Summary generation was unavailable.") {
            self.marker_seen.store(true, Ordering::SeqCst);
        }
        if body.get("tools").is_some() && contains("earlier tool-call messages omitted") {
            self.old_placeholder_seen.store(true, Ordering::SeqCst);
        }
        if body.get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]}
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": "final after trim"}}))
        }
    }
}

#[tokio::test]
async fn mid_loop_compression_fires_when_message_list_grows() {
    let server = MockServer::start().await;
    let marker_seen = Arc::new(AtomicBool::new(false));
    let old_placeholder_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(TrimObservingResponder {
            marker_seen: marker_seen.clone(),
            old_placeholder_seen: old_placeholder_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 3;
    c.mid_loop_trim_threshold = 4;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(
        marker_seen.load(Ordering::SeqCst),
        "the static compaction marker must have reached the model mid-loop"
    );
    assert!(
        !old_placeholder_seen.load(Ordering::SeqCst),
        "the pre-18.4 amputation placeholder must never be emitted"
    );
    assert_eq!(reply, "final after trim");
}

/// OpenAI-path transcript regression for the 2026-07-16 amnesia failure:
/// after turn A completed, turn B grew large enough to compact mid-loop. The
/// continuation must point at the authoritative protected prompt B, never
/// rediscover the first user message (A) from retained conversation history.
struct OpenAiTaskAnchoringResponder {
    directive_seen: Arc<AtomicBool>,
    current_task_in_directive: Arc<AtomicBool>,
    historical_task_in_directive: Arc<AtomicBool>,
}

impl Respond for OpenAiTaskAnchoringResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let messages = body["messages"].as_array();
        let has_summary = messages.is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains(SUMMARY_PREFIX))
            })
        });

        if has_summary {
            let directive = messages.and_then(|messages| {
                messages.iter().find_map(|message| {
                    message["content"]
                        .as_str()
                        .filter(|content| content.starts_with(compress::CONTINUATION_PREFIX))
                })
            });
            if let Some(directive) = directive {
                self.directive_seen.store(true, Ordering::SeqCst);
                if directive.contains("prompt_read") {
                    self.current_task_in_directive.store(true, Ordering::SeqCst);
                }
                if directive.contains(HISTORICAL_TASK) {
                    self.historical_task_in_directive
                        .store(true, Ordering::SeqCst);
                }
            }
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "continued the current task"}}]
            }));
        }

        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "definitely_not_a_real_tool",
                        "arguments": "{}"
                    }
                }]
            }}]
        }))
    }
}

const HISTORICAL_TASK: &str = "OLD TASK: probe every ambient MCP server and report its health";
const CURRENT_TASK: &str =
    "CURRENT TASK: modify the newt-agent source code and implement MCP management";

#[tokio::test]
async fn openai_mid_loop_compaction_anchors_the_current_turn_not_historical_prompt() {
    let server = MockServer::start().await;
    let directive_seen = Arc::new(AtomicBool::new(false));
    let current_task_in_directive = Arc::new(AtomicBool::new(false));
    let historical_task_in_directive = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiTaskAnchoringResponder {
            directive_seen: directive_seen.clone(),
            current_task_in_directive: current_task_in_directive.clone(),
            historical_task_in_directive: historical_task_in_directive.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user(HISTORICAL_TASK),
        MemMessage::assistant("Ten ambient MCP servers are reachable."),
        MemMessage::user(CURRENT_TASK),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = CURRENT_TASK;
    c.max_tool_rounds = 3;
    // The protected active-prompt pair and four historical messages fit on
    // round 0; the first tool exchange grows the list past seven, forcing the
    // regression's MID-TURN compaction.
    c.mid_loop_trim_threshold = 7;

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai loop should continue after compaction");

    assert_eq!(reply, "continued the current task");
    assert!(
        directive_seen.load(Ordering::SeqCst),
        "the post-compaction directive must reach the wire"
    );
    assert!(
        current_task_in_directive.load(Ordering::SeqCst),
        "the wire continuation must point back to the protected current prompt"
    );
    assert!(
        !historical_task_in_directive.load(Ordering::SeqCst),
        "the wire continuation must not relabel historical task A as current"
    );
}

/// The cap-exit summary round returns 200 with EMPTY content: the loop
/// must surface the named fallback, not the empty string.
struct EmptyFinalSummary;
impl Respond for EmptyFinalSummary {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]}
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": ""}}))
        }
    }
}

#[tokio::test]
async fn empty_final_summary_yields_cap_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(EmptyFinalSummary)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 2;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(reply.contains("tool-call limit of 2"), "got: {reply}");
    assert!(
        reply.contains("increase the tool-round limit"),
        "gives caller-neutral recovery advice"
    );
}

/// #867 regression: the cap-exit summary cites a file that does not
/// exist (the forensic transcript's exact shape — evidence trimmed, the
/// model reconstructs a plausible path). The claim check must append a
/// visible refutation naming the path, while leaving the model's prose
/// intact as a prefix.
struct HallucinatingFinalSummary;
impl Respond for HallucinatingFinalSummary {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]}
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content":
                    "The /end command is defined in newt-tui/src/commands.rs \
                     (lines 38-40) as enum variants."}
            }))
        }
    }
}

#[tokio::test]
async fn cap_exit_hallucinated_path_gets_claim_check_refutation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(HallucinatingFinalSummary)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 2;
    // `ctx` sets workspace = "." (this crate's dir under cargo test), so
    // the cited `newt-tui/src/commands.rs` provably does not exist there.
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(
        reply.contains("newt-tui/src/commands.rs (lines 38-40)"),
        "the model's prose is preserved verbatim: {reply}"
    );
    assert!(reply.contains("⚠ claim check (#867)"), "got: {reply}");
    assert!(
        reply.contains("`newt-tui/src/commands.rs`"),
        "the fabricated path is named in the refutation: {reply}"
    );
}

// -----------------------------------------------------------------------
// OpenAI-path coverage
// -----------------------------------------------------------------------

/// Large MCP surface from the field transcript: 169 remote definitions before
/// Newt's built-ins. None is invoked; this fixture grounds the request-envelope
/// regression without a live MCP server.
struct ManyToolsMcp {
    count: usize,
}

#[async_trait::async_trait]
impl McpTools for ManyToolsMcp {
    fn handles(&self, _name: &str) -> bool {
        false
    }

    fn tool_defs(&self) -> Vec<serde_json::Value> {
        (0..self.count)
            .map(|index| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": format!("field_mcp__tool_{index:03}"),
                        "description": "Synthetic remote tool for the provider-envelope regression.",
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect()
    }

    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        "unexpected MCP call".to_string()
    }
}

/// Simulates an OpenAI-compatible gateway that rejects more than 128 function
/// tools with the same opaque invalid_argument shape seen in the transcript.
struct OpenAiToolEnvelopeResponder {
    max_seen: Arc<AtomicUsize>,
    kernel_seen: Arc<AtomicBool>,
}

impl Respond for OpenAiToolEnvelopeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let tools = body["tools"].as_array().cloned().unwrap_or_default();
        self.max_seen.fetch_max(tools.len(), Ordering::SeqCst);
        let names = tools
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<Vec<_>>();
        self.kernel_seen.store(
            names.contains(&"run_command") && names.contains(&"tool_search"),
            Ordering::SeqCst,
        );
        if tools.len() > crate::agentic::tools::exposure::OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "code": "invalid_argument",
                    "message": "an internal error occurred",
                    "type": "invalid_request_error"
                }
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "tool envelope accepted"}}]
        }))
    }
}

#[tokio::test]
async fn openai_large_mcp_catalog_stays_within_wire_contract_and_keeps_shell() {
    let server = MockServer::start().await;
    let max_seen = Arc::new(AtomicUsize::new(0));
    let kernel_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiToolEnvelopeResponder {
            max_seen: max_seen.clone(),
            kernel_seen: kernel_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = "test \"gh auth status\" now";
    let mut mcp = ManyToolsMcp { count: 169 };

    let (reply, _, _, _) = chat_complete(c, &mut mcp)
        .await
        .expect("the provider-compatible request should reach inference");

    assert_eq!(reply, "tool envelope accepted");
    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        crate::agentic::tools::exposure::OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS
    );
    assert!(
        kernel_seen.load(Ordering::SeqCst),
        "run_command and tool_search must survive optional MCP clipping"
    );
}

#[tokio::test]
async fn chat_complete_dispatches_openai_kind_and_returns_first_round_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "openai says hi"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4},
        })))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    // Calling chat_complete (not openai_chat_complete) pins the dispatch.
    let (reply, streamed, usage, hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai dispatch should succeed");

    assert_eq!(reply, "openai says hi");
    assert!(!streamed, "openai path is non-streaming");
    let u = usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (10, 4));
    assert_eq!(hallu, 0);
}

#[test]
fn inference_endpoint_locality_distinguishes_hosted_from_home_lab_names() {
    assert!(!inference_endpoint_is_local("https://api.moonshot.ai"));
    assert!(inference_endpoint_is_local("http://dgx1.home.lab:8080"));
    assert!(inference_endpoint_is_local("http://dgx1:8000"));
    assert!(inference_endpoint_is_local("http://127.0.0.1:8000"));
    assert!(inference_endpoint_is_local("http://[fd00::1]:8000"));
    assert!(inference_endpoint_is_local("http://[fe80::1]:8000"));
}

#[test]
fn openai_progress_label_exposes_attempt_and_deadline() {
    assert_eq!(
        inference_progress_label("kimi-k3", 2, 2, 120),
        "waiting for kimi-k3 · attempt 2/2 · 120s deadline…"
    );
}

/// Regression for the live full-access incident: the model narrated an exec
/// denial without ever calling the advertised tool. The harness must spend one
/// bounded round demanding a real probe instead of presenting that invention
/// as the final answer.
#[tokio::test]
async fn openai_unverified_run_command_blocker_gets_ground_truth_retry() {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script: vec![
                serde_json::json!({
                    "content": "I hit a capability wall: run_command is permission-denied; exec not granted."
                }),
                serde_json::json!({
                    "content": "I will inspect an actual run_command result before reporting a denial."
                }),
            ],
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = "you should have a \"gh\" command ... test \"gh auth status\" now to tell me if you can use it?";
    c.max_tool_rounds = 2;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");

    assert_eq!(round.load(Ordering::SeqCst), 2, "one corrective retry");
    assert!(
        reply.contains("actual run_command result"),
        "final reply: {reply}"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    let second = body_json(&requests[1]).to_string();
    assert!(
        second.contains("no returned run_command result this turn contains an exec denial"),
        "corrective request: {second}"
    );
    assert!(
        second.contains("Report a denial only when an actual returned result contains one"),
        "corrective request: {second}"
    );
}

/// Mirrors vLLM chat templates that accept exactly one system message and
/// require it to be the first message in the request.
struct StrictVllmSystemResponder;

impl Respond for StrictVllmSystemResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let system_messages: Vec<(usize, &serde_json::Value)> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message["role"].as_str() == Some("system"))
            .collect();
        let valid_system = system_messages.len() == 1
            && system_messages[0].0 == 0
            && system_messages[0].1["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("you are a test")
                        && content.contains(prompt_read::ACTIVE_PROMPT_PREFIX)
                });
        let expected_tail = [
            ("user", "hello"),
            ("user", "an earlier request"),
            ("assistant", "the earlier request is complete"),
            ("user", "hello"),
        ];
        let valid_tail = messages.get(1..).is_some_and(|tail| {
            tail.len() == expected_tail.len()
                && tail
                    .iter()
                    .zip(expected_tail)
                    .all(|(message, (role, content))| {
                        message["role"].as_str() == Some(role)
                            && message["content"].as_str() == Some(content)
                    })
        });

        if !valid_system || !valid_tail {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "message": "System message must be at the beginning.",
                    "type": "BadRequestError"
                }
            }));
        }

        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "strict vllm accepted the request"}}]
        }))
    }
}

#[tokio::test]
async fn openai_chat_coalesces_system_cards_for_strict_vllm_templates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(StrictVllmSystemResponder)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("an earlier request"),
        MemMessage::assistant("the earlier request is complete"),
        MemMessage::user("hello"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = "hello";

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("strict vLLM chat template should accept the next message after a backend switch");

    assert_eq!(reply, "strict vllm accepted the request");
}

#[test]
fn openai_chat_rejects_system_messages_after_conversation_history() {
    let messages = vec![
        serde_json::json!({"role": "system", "content": "base policy"}),
        serde_json::json!({"role": "user", "content": "hello"}),
        serde_json::json!({"role": "system", "content": "late policy"}),
    ];

    let error = openai_chat_wire_messages(&messages)
        .expect_err("a late system message must not be promoted across user history");

    assert_eq!(
        error.to_string(),
        "invalid OpenAI chat message order: system messages must precede conversation history"
    );
}

#[tokio::test]
async fn openai_strips_inline_think_and_never_returns_reasoning_content() {
    // #857: a reasoning model served with the parser OFF puts its CoT inline as
    // <think>…</think> in content; served with the parser ON it lands in a
    // separate reasoning_content field. Either way the returned answer must be
    // ONLY the clean content — no <think> markers, no CoT, no reasoning_content.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "content": "<think>secret chain of thought</think>The final answer.",
                "reasoning_content": "separate-channel reasoning"
            }}]
        })))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let (reply, _streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai dispatch should succeed");

    assert_eq!(reply, "The final answer.", "answer is the stripped content");
    assert!(!reply.contains("<think>"), "no think markers: {reply}");
    assert!(
        !reply.contains("secret chain of thought"),
        "inline CoT must not leak: {reply}"
    );
    assert!(
        !reply.contains("separate-channel reasoning"),
        "reasoning_content must not leak into the reply: {reply}"
    );
}

struct OpenAiReasoningReplayResponder {
    round: AtomicUsize,
    second_request: Arc<Mutex<Option<serde_json::Value>>>,
}

impl Respond for OpenAiReasoningReplayResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if self.round.fetch_add(1, Ordering::SeqCst) == 0 {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "content": "<think>inspect the first result before continuing</think>",
                    "reasoning_content": "read the first result, then choose the next action",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "definitely_not_a_real_tool",
                            "arguments": "{}"
                        }
                    }]
                }}]
            }));
        }

        *self.second_request.lock().expect("capture lock") = Some(body_json(req));
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": "finished after the tool result"
            }}]
        }))
    }
}

#[tokio::test]
async fn openai_replays_reasoning_content_within_the_current_user_turn() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningReplayResponder {
            round: AtomicUsize::new(0),
            second_request: second_request.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("two-round OpenAI dispatch succeeds");

    assert_eq!(reply, "finished after the tool result");
    let request = second_request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("second request captured");
    let replayed_messages = request["messages"].as_array().expect("messages array");
    let replayed_index = replayed_messages
        .iter()
        .position(|message| {
            message["role"] == "assistant"
                && message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
        })
        .expect("assistant tool-call message replayed");
    let replayed_assistant = &replayed_messages[replayed_index];
    assert_eq!(
        replayed_assistant["reasoning_content"],
        "read the first result, then choose the next action"
    );
    assert_eq!(
        replayed_assistant["content"],
        "<think>inspect the first result before continuing</think>"
    );

    assert_eq!(replayed_messages[replayed_index + 1]["role"], "tool");
}

#[tokio::test]
async fn openai_default_scope_redacts_reasoning_from_tool_replay() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningReplayResponder {
            round: AtomicUsize::new(0),
            second_request: second_request.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;

    chat_complete(c, &mut NoMcp)
        .await
        .expect("default OpenAI dispatch succeeds");

    let request = second_request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("second request captured");
    let replayed_assistant = request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| {
            message["role"] == "assistant"
                && message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
        })
        .expect("assistant tool-call message replayed");
    assert_eq!(replayed_assistant["content"], "");
    assert!(replayed_assistant.get("reasoning_content").is_none());
}

struct CaptureOpenAiRequestResponder {
    request: Arc<Mutex<Option<serde_json::Value>>>,
}

impl Respond for CaptureOpenAiRequestResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        *self.request.lock().expect("capture lock") = Some(body_json(req));
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": "current turn complete"
            }}]
        }))
    }
}

#[tokio::test]
async fn openai_current_turn_scope_redacts_inline_reasoning_from_restored_history() {
    let server = MockServer::start().await;
    let request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureOpenAiRequestResponder {
            request: request.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("an earlier task"),
        MemMessage::assistant("<think>private old plan</think>visible old answer"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;

    chat_complete(c, &mut NoMcp)
        .await
        .expect("restored-history dispatch succeeds");

    let request = request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("request captured");
    let old_assistant = request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("restored assistant message present");
    assert_eq!(old_assistant["content"], "visible old answer");
    assert!(!request.to_string().contains("private old plan"));
}

#[tokio::test]
async fn openai_chat_projects_cognition_only_for_an_explicitly_capable_endpoint() {
    let server = MockServer::start().await;
    let request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureOpenAiRequestResponder {
            request: request.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.cognition = Some(crate::role_profile::Cognition::Deliberating);
    c.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        chat_template_kwargs: Some(true),
        parallel_tool_calls: Some(false),
        bounded_reasoning_continuation: Some(true),
    };

    chat_complete(c, &mut NoMcp)
        .await
        .expect("capable OpenAI-compatible dispatch succeeds");

    let request = request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("request captured");
    assert_eq!(request["max_tokens"], 10_000);
    assert_eq!(request["temperature"], 0.6);
    assert_eq!(request["top_p"], 0.95);
    assert_eq!(request["parallel_tool_calls"], false);
    assert_eq!(request["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(
        request["chat_template_kwargs"]["truncate_history_thinking"],
        true
    );
}

#[tokio::test]
async fn openai_chat_omits_local_cognition_fields_for_an_unknown_endpoint() {
    let server = MockServer::start().await;
    let request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureOpenAiRequestResponder {
            request: request.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.cognition = Some(crate::role_profile::Cognition::Contemplating);

    chat_complete(c, &mut NoMcp)
        .await
        .expect("strict-compatible dispatch succeeds");

    let request = request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("request captured");
    for field in [
        "max_tokens",
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "chat_template_kwargs",
    ] {
        assert!(
            request.get(field).is_none(),
            "unknown endpoints must not receive `{field}`"
        );
    }
}

struct OpenAiReasoningOverflowResponder {
    round: AtomicUsize,
    second_request: Arc<Mutex<Option<serde_json::Value>>>,
    overflow_twice: bool,
    inline_reasoning: bool,
}

impl Respond for OpenAiReasoningOverflowResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        if round > 0 {
            *self.second_request.lock().expect("capture lock") = Some(body_json(req));
        }
        if round == 0 || self.overflow_twice {
            let message = if self.inline_reasoning {
                serde_json::json!({
                    "role": "assistant",
                    "content": format!("<think>unfinished inline plan {round}")
                })
            } else {
                serde_json::json!({
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": format!("unfinished plan {round}")
                })
            };
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "finish_reason": "length",
                    "message": message
                }],
                "usage": {"prompt_tokens": 20, "completion_tokens": 8}
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "completed after bounded continuation"
                }
            }],
            "usage": {"prompt_tokens": 24, "completion_tokens": 5}
        }))
    }
}

#[tokio::test]
async fn openai_reasoning_overflow_continues_once_with_the_current_plan() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    let responder = OpenAiReasoningOverflowResponder {
        round: AtomicUsize::new(0),
        second_request: second_request.clone(),
        overflow_twice: false,
        inline_reasoning: false,
    };
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(responder)
        .expect(2)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    c.chat_completions_capability.bounded_reasoning_continuation = Some(true);
    c.solve_obs = Some(&mut observation);

    let (reply, _, usage, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("bounded continuation succeeds");

    assert_eq!(reply, "completed after bounded continuation");
    assert_eq!(usage.expect("usage accumulated").output_tokens, 13);
    let request = second_request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("second request captured");
    let replayed = request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("partial assistant message replayed");
    assert_eq!(replayed["reasoning_content"], "unfinished plan 0");
    assert!(!reply.contains("unfinished plan"));
    assert!(observation.behavior_signals.iter().any(|signal| matches!(
        signal,
        crate::agentic::observability::BehaviorSignal::ReasoningOverflow {
            continuation_attempted: true,
            continuation_succeeded: true,
            ..
        }
    )));
    assert_eq!(
        observation
            .behavior_signals
            .iter()
            .filter_map(|signal| match signal {
                crate::agentic::observability::BehaviorSignal::ChatCompletionFinish {
                    finish_reason,
                    ..
                } => finish_reason.as_deref(),
                _ => None,
            })
            .collect::<Vec<_>>(),
        ["length", "stop"]
    );
}

#[tokio::test]
async fn openai_reasoning_overflow_stops_after_one_failed_continuation() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningOverflowResponder {
            round: AtomicUsize::new(0),
            second_request,
            overflow_twice: true,
            inline_reasoning: false,
        })
        .expect(2)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    c.chat_completions_capability.bounded_reasoning_continuation = Some(true);
    c.solve_obs = Some(&mut observation);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("second overflow is classified, not retried forever");

    assert!(
        reply.contains("empty response"),
        "honest terminal result: {reply}"
    );
    assert!(observation.behavior_signals.iter().any(|signal| matches!(
        signal,
        crate::agentic::observability::BehaviorSignal::ReasoningOverflow {
            continuation_attempted: true,
            continuation_succeeded: false,
            ..
        }
    )));
}

#[tokio::test]
async fn openai_inline_reasoning_overflow_uses_the_same_bounded_continuation() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningOverflowResponder {
            round: AtomicUsize::new(0),
            second_request: second_request.clone(),
            overflow_twice: false,
            inline_reasoning: true,
        })
        .expect(2)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    c.chat_completions_capability.bounded_reasoning_continuation = Some(true);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("inline bounded continuation succeeds");

    assert_eq!(reply, "completed after bounded continuation");
    assert!(!reply.contains("inline plan"));
    let request = second_request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("second request captured");
    let replayed = request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("inline assistant partial replayed");
    assert_eq!(replayed["content"], "<think>unfinished inline plan 0");
    assert!(replayed.get("reasoning_content").is_none());
}

#[tokio::test]
async fn openai_reasoning_overflow_does_not_retry_an_unknown_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "unfinished private plan"
                }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.solve_obs = Some(&mut observation);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("unknown endpoint stops without an unsafe retry");

    assert!(reply.contains("empty response"));
    assert!(!reply.contains("private plan"));
    assert!(observation.behavior_signals.iter().any(|signal| matches!(
        signal,
        crate::agentic::observability::BehaviorSignal::ReasoningOverflow {
            continuation_attempted: false,
            continuation_succeeded: false,
            ..
        }
    )));
}

#[tokio::test]
async fn openai_reasoning_only_stop_is_not_misclassified_as_overflow() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "private reasoning with a normal stop"
                }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = crate::agentic::observability::SolveObservation::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    c.chat_completions_capability.bounded_reasoning_continuation = Some(true);
    c.solve_obs = Some(&mut observation);

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("ordinary stop remains a terminal empty response");

    assert!(reply.contains("empty response"));
    assert!(!reply.contains("private reasoning"));
    assert!(observation.behavior_signals.iter().all(|signal| !matches!(
        signal,
        crate::agentic::observability::BehaviorSignal::ReasoningOverflow { .. }
    )));
}

#[test]
fn openai_current_turn_scope_strips_reasoning_from_an_older_turn() {
    let message = serde_json::json!({
        "role": "assistant",
        "content": "<think>old inline plan</think>visible answer",
        "reasoning_content": "old split plan",
        "tool_calls": [{
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        }]
    });

    let replay = prepare_openai_assistant_replay(
        &message,
        "visible answer",
        crate::model_card::ReasoningReplayScope::CurrentUserTurn,
        false,
    );

    assert_eq!(replay["content"], "visible answer");
    assert!(replay.get("reasoning_content").is_none());
    assert_eq!(replay["tool_calls"], message["tool_calls"]);
}

#[tokio::test]
async fn openai_empty_content_yields_diagnostic_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": ""}}]
        })))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.expect("should succeed");
    assert!(
        reply.contains("model returned an empty response"),
        "got: {reply}"
    );
}

/// Mock MCP that handles exactly one namespaced tool for routing tests.
struct OneToolMcp {
    name: &'static str,
    result: &'static str,
}
#[async_trait::async_trait]
impl McpTools for OneToolMcp {
    fn handles(&self, name: &str) -> bool {
        name == self.name
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        self.result.to_string()
    }
}

/// An API proxy may put Anthropic-native tool-use blocks
/// (`{"name":"…","input":{}}`) inside the OpenAI `tool_calls` array
/// instead of converting them to `{"function":{"name":"…","arguments":"…"}}`.
/// The loop must detect the missing `function` key, fall back to the
/// Anthropic-native fields, and route the call correctly.
#[tokio::test]
async fn openai_anthropic_native_tool_calls_route_correctly() {
    let server = MockServer::start().await;
    // Round 1: Anthropic-native tool-use block in the tool_calls array.
    // Round 2: plain text final answer after receiving the tool result.
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cc = call_count.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_req: &Request| {
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "tool_calls": [{
                            "type": "tool_use",
                            "id": "toolu_01ABC",
                            "name": "my_server__my_tool",
                            "input": {"key": "value"}
                        }]
                    }}],
                    "usage": {"prompt_tokens": 50, "completion_tokens": 10}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "done after anthropic-native tool"}}],
                    "usage": {"prompt_tokens": 60, "completion_tokens": 8}
                }))
            }
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let mut mcp = OneToolMcp {
        name: "my_server__my_tool",
        result: "tool-result-text",
    };
    let (reply, _, _, hallu) = chat_complete(c, &mut mcp)
        .await
        .expect("should succeed with anthropic-native tool format");
    assert_eq!(reply, "done after anthropic-native tool");
    assert_eq!(hallu, 0, "should not be counted as hallucination");
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "must have done both rounds (tool call + final answer)"
    );
}

/// Regression: some OpenAI-compatible API proxies normalise hyphens to
/// underscores in tool names (`acme-server` → `acme_server`).  Verify that
/// the underscore form routes through MCP rather than falling to "unknown tool".
#[tokio::test]
async fn openai_hyphenated_server_name_routes_through_mcp() {
    let server = MockServer::start().await;
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cc = call_count.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_req: &Request| {
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                // Proxy returns the underscore-normalised form of the
                // server prefix even though we advertised hyphens.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": "",
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "{}",
                                "name": "acme_server__probe_tool"
                            },
                            "id": "call_probe_01",
                            "type": "function"
                        }]
                    }}],
                    "usage": {"prompt_tokens": 599, "completion_tokens": 30}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "outlook routed correctly"}}]
                }))
            }
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    // OneToolMcp.handles() must match the underscore form the proxy returns.
    let mut mcp = OneToolMcp {
        name: "acme_server__probe_tool",
        result: "ok",
    };
    let (reply, _, _, _) = chat_complete(c, &mut mcp)
        .await
        .expect("should route hyphenated server name through mcp");
    assert_eq!(reply, "outlook routed correctly");
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "must have completed both rounds"
    );
}

/// OpenAI mirror of the Ollama cap-exit fallback: tool calls until the cap,
/// then a 400 on the tools-disabled summary → the named fallback.
struct OpenAiErrOnFinal;
impl Respond for OpenAiErrOnFinal {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                    }]
                }}]
            }))
        } else {
            ResponseTemplate::new(400).set_body_string("bad request")
        }
    }
}

#[tokio::test]
async fn openai_cap_exit_fallback_when_final_summary_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiErrOnFinal)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.max_tool_rounds = 2;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("must succeed even when the summary errors");
    assert!(reply.contains("tool-call limit of 2"), "got: {reply}");
    assert!(reply.contains("increase the tool-round limit"));
}

// -- Narrate-then-stop rescue: bounded no-tool-call auto-continue ---------

/// OpenAI responder that serves a scripted `choices[0].message` per request
/// (by order); out-of-range requests repeat the last scripted entry.
struct ScriptedOpenAi {
    round: Arc<AtomicUsize>,
    script: Vec<serde_json::Value>,
}
impl Respond for ScriptedOpenAi {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let i = self.round.fetch_add(1, Ordering::SeqCst);
        let msg = self
            .script
            .get(i)
            .or_else(|| self.script.last())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "content": "final." }));
        ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({ "choices": [{ "message": msg }] }))
    }
}

/// Drive the OpenAI loop over a per-round script; return `(reply, requests)`.
async fn run_openai_script_with_ledger(
    script: Vec<serde_json::Value>,
    step_ledger: Option<&dyn StepLedger>,
) -> (String, usize) {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script,
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.step_ledger = step_ledger;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    (reply, round.load(Ordering::SeqCst))
}

async fn run_openai_script(script: Vec<serde_json::Value>) -> (String, usize) {
    run_openai_script_with_ledger(script, None).await
}

/// #1259: `request_user_input` is a legitimate escalation in an **Explain**
/// turn — the boxed-in model formally asks the human instead of being forced
/// into penalized narration (the #1257 double-bind). Headless (no gate), the
/// dispatch returns the recoverable no-human message — never the
/// disposition-refusal, never a hang — and the turn completes normally.
/// Contrast pin in the same run: an Act-only tool (`run_command`) under the
/// same Explain turn still gets the disposition refusal (the boundary holds).
#[tokio::test]
async fn explain_turn_request_user_input_dispatches_and_completes() {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script: vec![
                serde_json::json!({
                    "content": null,
                    "tool_calls": [{
                        "id": "c1", "type": "function",
                        "function": { "name": "request_user_input",
                                       "arguments": "{\"question\":\"Which directory should I size?\"}" }
                    }]
                }),
                serde_json::json!({
                    "content": null,
                    "tool_calls": [{
                        "id": "c2", "type": "function",
                        "function": { "name": "run_command",
                                       "arguments": "{\"command\":\"du -sh .\"}" }
                    }]
                }),
                serde_json::json!({ "content": "Understood — proceeding with the workspace root." }),
            ],
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.prompt_disposition = PromptDisposition::Explain;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp)
        .await
        .expect("an Explain turn asking the human completes, never errors");
    assert!(
        reply.contains("proceeding with the workspace root"),
        "the turn ends on the final answer: {reply}"
    );

    // Wire-level: what the loop fed back for each tool call.
    let requests = server.received_requests().await.expect("recorded");
    let bodies: Vec<String> = requests
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    let all = bodies.join("\n---\n");
    // The escalation DISPATCHED: its result is the recoverable headless
    // message, not the disposition refusal.
    assert!(
        all.contains("no human available this session"),
        "request_user_input must dispatch (headless => the recoverable no-human message): {all}"
    );
    assert!(
        !all.contains("Tool `request_user_input` is unavailable"),
        "request_user_input must NOT be disposition-refused in an Explain turn"
    );
    // The boundary still holds for Act-only tools in the SAME turn.
    assert!(
        all.contains("Tool `run_command` is unavailable"),
        "run_command must stay disposition-refused in an Explain turn: {all}"
    );
}

/// Like [`run_openai_script`] but with a configured narrate-then-stop
/// rescue budget (`[tui] narration_nudge_cap`, lever L3).
async fn run_openai_script_with_cap(
    script: Vec<serde_json::Value>,
    narration_nudge_cap: usize,
) -> (String, usize) {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script,
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.narration_nudge_cap = narration_nudge_cap;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    (reply, round.load(Ordering::SeqCst))
}

#[test]
fn strip_trailing_nudge_keeps_only_the_current_correction() {
    // #1158: successive nudges must REPLACE, not pile up — otherwise the
    // model's own accumulated dithering drives the "genuinely finished"
    // defense loop. After stripping, the tail nudge pair is gone; a
    // non-nudge tail is untouched.
    use serde_json::json;
    let guidance = format!("{} act now", compress::LOOP_GUIDANCE_PREFIX);
    let mut msgs = vec![
        json!({"role": "user", "content": "do the thing"}),
        json!({"role": "assistant", "content": "Let me start."}),
        json!({"role": "user", "content": guidance.clone()}),
    ];
    strip_trailing_nudge_exchange(&mut msgs);
    assert_eq!(msgs.len(), 1, "the narration + nudge pair is removed");
    assert_eq!(msgs[0]["content"], "do the thing");

    let mut clean = vec![
        json!({"role": "user", "content": "do the thing"}),
        json!({"role": "assistant", "content": "here is the answer"}),
    ];
    let before = clean.clone();
    strip_trailing_nudge_exchange(&mut clean);
    assert_eq!(clean, before, "a real answer tail is never stripped");

    let mut loop_msgs = vec![json!({"role": "user", "content": "fix it"})];
    for i in 0..3 {
        strip_trailing_nudge_exchange(&mut loop_msgs);
        loop_msgs.push(json!({"role": "assistant", "content": format!("narration {i}")}));
        loop_msgs.push(json!({"role": "user", "content": guidance.clone()}));
    }
    assert_eq!(
        loop_msgs.len(),
        3,
        "user task + exactly one (narration, nudge) pair — not three"
    );
}

#[tokio::test]
async fn question_turns_are_never_nudged() {
    // Regression #1152/#1162 (the 2026-07-14 Opus session): the user asked
    // a QUESTION; the model's narrated answer classified as pending-action
    // and got nudged, seeding the "I'm genuinely finished" defense loop
    // (#1158). With the intent gate, a question turn takes its narration
    // as the final answer on ROUND ONE — no rescue, no residue.
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            // Phrasing the classifier reads as pending-action ("Let me…").
            script: vec![
                serde_json::json!({ "content": "Let me look into the harness next." }),
                serde_json::json!({ "content": "SHOULD NEVER BE REQUESTED" }),
            ],
        })
        .mount(&server)
        .await;
    let question =
        "Give me your top 5 improvements to make LLM effectiveness better inside this harness please?";
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user(question),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = question;
    c.narration_nudge_cap = 2; // budget available — the GATE must stop it
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(
        round.load(Ordering::SeqCst),
        1,
        "a question turn must never consume a rescue round"
    );
    assert!(
        reply.contains("look into"),
        "narration IS the answer: {reply}"
    );
}

#[tokio::test]
async fn narrated_intent_with_no_tool_call_nudges_and_continues() {
    // The model narrates intent to act but calls no tool. Instead of ending
    // the turn (the bug), the loop nudges and runs another round, returning
    // the post-nudge answer.
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({ "content": "Let me edit the file now." }),
        serde_json::json!({ "content": "All done — the edit is complete." }),
    ])
    .await;
    assert_eq!(rounds, 2, "must run a second round after the nudge");
    assert!(
        reply.contains("complete"),
        "returns the post-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("Let me edit"),
        "must not return the narration: {reply}"
    );
}

#[tokio::test]
async fn narration_auto_continue_is_bounded_by_the_cap() {
    // The model narrates intent EVERY round. The cap (1) allows exactly one
    // nudge, then the narration is accepted as the final answer — no loop.
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({ "content": "Let me keep editing now." }),
        serde_json::json!({ "content": "Let me keep editing now." }),
        serde_json::json!({ "content": "Let me keep editing now." }),
    ])
    .await;
    assert_eq!(
        rounds, 2,
        "exactly one nudge (cap=1), then accept, got {rounds}"
    );
    assert!(
        reply.contains("editing"),
        "narration accepted as final: {reply}"
    );
}

#[tokio::test]
async fn narration_nudge_cap_two_allows_a_second_escalated_rescue() {
    // Lever L3: with `narration_nudge_cap = 2` a chronic narrator gets TWO
    // rescues (the second escalated), and the post-rescue answer is
    // returned; a genuine recovery on round 3 proves the extra budget is
    // what converts the stall.
    let (reply, rounds) = run_openai_script_with_cap(
        vec![
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "All done — the edit is complete." }),
        ],
        2,
    )
    .await;
    assert_eq!(
        rounds, 3,
        "two nudges (cap=2) before the recovery, got {rounds}"
    );
    assert!(
        reply.contains("complete"),
        "returns the post-nudge answer: {reply}"
    );
}

#[tokio::test]
async fn narration_nudge_cap_two_still_accepts_after_exhaustion() {
    // The raised cap is still a cap: a model that narrates through both
    // rescues has its third narration accepted as the final answer.
    let (reply, rounds) = run_openai_script_with_cap(
        vec![
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "Let me keep editing now." }),
            serde_json::json!({ "content": "Let me keep editing now." }),
        ],
        2,
    )
    .await;
    assert_eq!(rounds, 3, "two nudges, then accept, got {rounds}");
    assert!(
        reply.contains("editing"),
        "narration accepted as final: {reply}"
    );
}

#[test]
fn escalated_narration_nudge_names_attempt_cap_and_active_step() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};

    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![
            Step {
                description: "inspect".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "fix conflict markers".to_string(),
                status: StepStatus::Active,
            },
        ],
    });
    let text = escalated_narration_action_nudge(2, 3, Some(&ledger as &dyn StepLedger));
    assert!(text.contains("Reminder 2/3"), "{text}");
    assert!(text.contains("fix conflict markers"), "{text}");
    assert!(text.contains("tool call"), "{text}");

    // No ledger: no step clause, the demand still stands.
    let bare = escalated_narration_action_nudge(2, 2, None);
    assert!(bare.contains("Reminder 2/2"), "{bare}");
    assert!(!bare.contains("Active step"), "{bare}");
}

#[tokio::test]
async fn accepted_narration_reports_cap_exhausted_end_reason() {
    // The acceptance-forensics record: a narration that exhausts the
    // rescue budget ends the turn with a visible reason instead of
    // masquerading as a normal completion.
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script: vec![
                serde_json::json!({ "content": "Let me keep editing now." }),
                serde_json::json!({ "content": "Let me keep editing now." }),
            ],
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.end_reason = Some(&mut end_reason);
    let _ = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(
        end_reason,
        Some(crate::TurnEndReason::NarrationCapExhausted)
    );
}

#[tokio::test]
async fn explain_turn_narration_reports_completed_not_cap_exhausted() {
    // #1261 regression (the diagnosed ornith:35b footer): in a NON-Act turn the
    // narration rescue can never arm (`action_nudges` is forced false, so
    // `action_turn` is false) — the budget is untouched and no cap value could
    // change anything. Ending on pending-action-looking prose is therefore this
    // turn's LEGITIMATE completion. Before the fix the end reason was computed
    // WITHOUT the gate's `action_turn` guard, reporting NarrationCapExhausted —
    // rendered as "⚠ ended on narration (rescue budget spent)", blaming the
    // model for a harness decision.
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script: vec![
                // The same pending-action phrasing the Act-turn test uses — the
                // classifier sees "pending action" either way; only the turn
                // type differs.
                serde_json::json!({ "content": "Let me keep editing now." }),
            ],
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.prompt_disposition = PromptDisposition::Explain;
    c.end_reason = Some(&mut end_reason);
    let _ = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(
        end_reason,
        Some(crate::TurnEndReason::Completed),
        "a non-Act turn ending on prose is a completion, never 'rescue budget spent'"
    );
    // The footer stays clean too — the ⚠ never renders for Completed.
    let metrics = crate::TurnMetrics {
        end_reason,
        ..Default::default()
    };
    assert!(
        !metrics.display_line().contains('⚠'),
        "no warning may render: {}",
        metrics.display_line()
    );
}

#[tokio::test]
async fn genuine_completion_reports_completed_end_reason() {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script: vec![serde_json::json!({ "content": "The capital of France is Paris." })],
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.end_reason = Some(&mut end_reason);
    let _ = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert_eq!(end_reason, Some(crate::TurnEndReason::Completed));
}

#[tokio::test]
async fn narration_nudge_reaches_the_wire_tagged_as_loop_guidance() {
    // The rescue nudge must arrive tagged so the compaction pipeline can
    // keep it (and the model's echo of it) out of later summaries.
    let server = MockServer::start().await;
    let saw_tag = Arc::new(AtomicBool::new(false));

    struct TagProbe {
        saw_tag: Arc<AtomicBool>,
    }
    impl Respond for TagProbe {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let tagged = body["messages"].as_array().is_some_and(|ms| {
                ms.iter().any(|m| {
                    m["role"] == "user"
                        && m["content"]
                            .as_str()
                            .is_some_and(|c| c.starts_with(compress::LOOP_GUIDANCE_PREFIX))
                })
            });
            if tagged {
                self.saw_tag.store(true, Ordering::SeqCst);
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "All done — edit complete." } }]
                }));
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "Let me edit the file now." } }]
            }))
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(TagProbe {
            saw_tag: saw_tag.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert!(
        saw_tag.load(Ordering::SeqCst),
        "the narration nudge must carry LOOP_GUIDANCE_PREFIX on the wire"
    );
    assert!(reply.contains("complete"), "{reply}");
}

#[tokio::test]
async fn ollama_loop_honors_cap_two_and_escalates_the_second_nudge() {
    // Ollama-path parity for lever L3 (the macro chain is separate code
    // from the OpenAI inline chain): with narration_nudge_cap = 2 the
    // first rescue carries the [loop-guidance]-tagged generic corrective
    // and the SECOND carries the escalated "Reminder 2/2" variant — both
    // observed on the wire — before the model recovers.
    let server = MockServer::start().await;
    let saw_first = Arc::new(AtomicBool::new(false));
    let saw_escalated = Arc::new(AtomicBool::new(false));

    struct EscalationProbe {
        saw_first: Arc<AtomicBool>,
        saw_escalated: Arc<AtomicBool>,
    }
    impl Respond for EscalationProbe {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let has = |needle: &str| {
                body["messages"].as_array().is_some_and(|ms| {
                    ms.iter()
                        .any(|m| m["content"].as_str().is_some_and(|c| c.contains(needle)))
                })
            };
            if has("Reminder 2/2") {
                self.saw_escalated.store(true, Ordering::SeqCst);
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "All done — the edit is complete." }
                }));
            }
            if has(compress::LOOP_GUIDANCE_PREFIX) {
                self.saw_first.store(true, Ordering::SeqCst);
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "Let me keep editing now." }
            }))
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(EscalationProbe {
            saw_first: saw_first.clone(),
            saw_escalated: saw_escalated.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Ollama;
    c.narration_nudge_cap = 2;
    let (reply, _s, _u, _h) = chat_complete(c, &mut NoMcp).await.expect("dispatch");
    assert!(
        saw_first.load(Ordering::SeqCst),
        "the first nudge must reach the Ollama wire tagged [loop-guidance]"
    );
    assert!(
        saw_escalated.load(Ordering::SeqCst),
        "the second nudge must be the escalated Reminder 2/2 variant"
    );
    assert!(reply.contains("complete"), "{reply}");
}

#[test]
fn post_compaction_continuation_reanchors_on_ground_truth() {
    // Regression #1163 (2026-07-14 Opus session): after a mid-turn
    // compaction the model saw an empty `git diff`, concluded "the
    // worktree is clean, start fresh", DISOWNED its own branch+commit and
    // repeated finished work. The continuation directive must order a
    // ground-truth re-anchor and state the clean-tree≠no-work rule.
    let d = post_compaction_continuation(None, prompt_read::PromptReadContext::new(None, "", None));
    assert!(d.contains("re-anchor on ground truth"), "{d}");
    assert!(d.contains("git branch"), "{d}");
    assert!(
        d.contains("clean working tree does NOT mean no work happened"),
        "{d}"
    );
    assert!(d.contains("artifact_read {\"address\":\"root\"}"), "{d}");
    assert!(d.contains("do not repeat work"), "{d}");
}

#[test]
fn post_compaction_continuation_reinjects_the_full_plan_advance_not_rewrite() {
    // #1163 (F): the corporate-box repro showed the model REWRITE its own
    // plan post-compaction (dropping the implement steps for "stop
    // implementation"). The directive must re-inject the WHOLE plan (every
    // step + status) and order advance-not-rewrite, so the plan is an
    // anchor the model continues from.
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};
    let ledger = SessionStepLedger::default();
    ledger.set_plan(&[
        "verify state".to_string(),
        "wire the lazy-emission guard".to_string(),
        "wire nudger profiles".to_string(),
    ]);
    ledger.advance(); // step 1 done, step 2 active
    let d = post_compaction_continuation(
        Some(&ledger),
        prompt_read::PromptReadContext::new(None, "", None),
    );
    // The full plan is present — including the not-yet-reached step.
    assert!(
        d.contains("wire the lazy-emission guard"),
        "active step: {d}"
    );
    assert!(
        d.contains("wire nudger profiles"),
        "future step present: {d}"
    );
    assert!(d.contains("verify state"), "done step present: {d}");
    // The advance-not-rewrite instruction.
    assert!(
        d.contains("NEVER to \\\n                 replace") || d.contains("NEVER to replace"),
        "{d}"
    );
    assert!(d.contains("advance"), "{d}");
    // No plan → no plan clause (and no panic).
    let empty =
        post_compaction_continuation(None, prompt_read::PromptReadContext::new(None, "", None));
    assert!(!empty.contains("active plan is below"), "{empty}");
}

#[test]
fn post_compaction_continuation_points_to_the_immutable_prompt_without_quoting_it() {
    // Regression #1163 (second repro, corporate-box Opus 2026-07-14):
    // compaction summarized the middle and the model re-derived a WRONG
    // task ("deliver a report") from the summary, dropping the operator's
    // actual instruction and confabulating. The exact instruction now lives
    // in a protected user-priority pair, so this directive points to its
    // immutable receipt rather than injecting a truncated user-role quote.
    let task = "make a plan, make a branch, write me a commit for each suggestion";
    let turn = crate::TurnPromptContext::ephemeral_operator("conv", task, task);
    let d = post_compaction_continuation(
        None,
        prompt_read::PromptReadContext::new(Some(&turn), task, None),
    );
    assert!(d.contains(&turn.active().id().to_string()), "{d}");
    assert!(d.contains(turn.active().model_digest()), "{d}");
    assert!(d.contains("prompt_read"), "{d}");
    assert!(!d.contains(task), "must not duplicate operator text: {d}");
    assert!(d.contains("do not narrow the task"), "{d}");
}

#[test]
fn post_compaction_uses_the_current_turn_task_not_the_first_conversation_prompt() {
    // Regression (2026-07-16 Opus transcript): turn A asked Newt to probe
    // ambient MCP servers. Turn B asked it to implement MCP management.
    // After a mid-turn compaction the harness rediscovered turn A with
    // `find()` and injected it as "the instruction for this turn", causing
    // the model to abandon the repository work and repeat the old probes.
    let old_task = "can you access any of the ambient MCP servers?";
    let current_task = "modify the newt-agent source code and implement MCP management";
    let turn = crate::TurnPromptContext::ephemeral_operator("conv", current_task, current_task);
    let prompt_context = prompt_read::PromptReadContext::new(Some(&turn), current_task, None);
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "you are newt"}),
        serde_json::json!({"role": "user", "content": old_task}),
        serde_json::json!({"role": "assistant", "content": "Ten servers are reachable."}),
        serde_json::json!({"role": "user", "content": current_task}),
        serde_json::json!({"role": "assistant", "content": "I will inspect the repository."}),
    ];
    let mut nudges = 1usize;

    apply_post_compaction_continuation(
        &mut messages,
        &mut nudges,
        CompressAction::Summarized,
        None,
        prompt_context,
        true,
        true,
    );

    let directive = messages
        .last()
        .and_then(|message| message["content"].as_str())
        .expect("post-compaction continuation");
    assert!(
        directive.contains(&turn.active().id().to_string()),
        "{directive}"
    );
    assert!(
        directive.contains(turn.active().model_digest()),
        "{directive}"
    );
    assert!(!directive.contains(current_task), "{directive}");
    assert!(
        !directive.contains(old_task),
        "the first conversation prompt must not be relabeled as current: {directive}"
    );
}

#[test]
fn post_compaction_refunds_rescue_budget_and_appends_one_directive() {
    let directive_count = |messages: &[serde_json::Value]| {
        messages
            .iter()
            .filter(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.starts_with(compress::CONTINUATION_PREFIX))
            })
            .count()
    };
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "you are a test"}),
        serde_json::json!({"role": "user", "content": "do the thing"}),
        serde_json::json!({
            "role": "user",
            "content": format!("{} stale directive", compress::CONTINUATION_PREFIX)
        }),
    ];
    let mut nudges = 1usize;
    let prompt_context = prompt_read::PromptReadContext::new(None, "do the thing", None);

    // Prune-only passes keep the corrective text: no refund, no anchor.
    apply_post_compaction_continuation(
        &mut messages,
        &mut nudges,
        CompressAction::Pruned,
        None,
        prompt_context,
        true,
        true,
    );
    assert_eq!(nudges, 1, "prune must not refund the rescue budget");
    assert_eq!(messages.len(), 3, "prune must not touch the directive");

    // Round 0 (a FRESH turn whose between-turn growth fired the pre-send
    // compaction): no directive — "You are mid-task … do not summarize"
    // would countermand the operator's brand-new ask sitting above it.
    apply_post_compaction_continuation(
        &mut messages,
        &mut nudges,
        CompressAction::Summarized,
        None,
        prompt_context,
        false,
        true,
    );
    assert_eq!(nudges, 1, "round 0 must not touch the rescue budget");
    assert_eq!(messages.len(), 3, "round 0 must not inject the directive");

    // A MID-TURN summarization refunds the budget, drops the stale
    // directive, and appends exactly one fresh act-now anchor as the last
    // user message.
    apply_post_compaction_continuation(
        &mut messages,
        &mut nudges,
        CompressAction::Summarized,
        None,
        prompt_context,
        true,
        true,
    );
    assert_eq!(nudges, 0, "summarization refunds the rescue budget");
    assert_eq!(directive_count(&messages), 1, "at most one directive alive");
    let last = messages.last().unwrap();
    assert_eq!(last["role"], "user");
    let content = last["content"].as_str().unwrap();
    assert!(
        content.starts_with(compress::CONTINUATION_PREFIX),
        "{content}"
    );
    assert!(content.contains("tool call"), "{content}");
    assert!(!content.contains("stale directive"), "{content}");
}

#[tokio::test]
async fn genuine_final_answer_is_not_nudged() {
    // No prior tool call and no intent-to-act cue → a real answer returns
    // immediately, un-nudged (no wasted round).
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({ "content": "The capital of France is Paris." }),
    ])
    .await;
    assert_eq!(
        rounds, 1,
        "a plain final answer is not nudged, got {rounds}"
    );
    assert!(reply.contains("Paris"), "returns the answer: {reply}");
}

#[tokio::test]
async fn final_answer_after_a_tool_call_is_not_nudged() {
    // The normal "act, then conclude" turn: a tool call, then a cue-less
    // final answer. The rescue must NOT fire (no intent cue) — else every
    // ordinary tool-using turn would waste a round.
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({
            "content": null,
            "tool_calls": [{
                "id": "c1", "type": "function",
                "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
            }]
        }),
        serde_json::json!({ "content": "The files were examined; everything checks out." }),
    ])
    .await;
    assert_eq!(
        rounds, 2,
        "tool call (r0) then final answer (r1) — no extra round, got {rounds}"
    );
    assert!(
        reply.contains("checks out"),
        "returns the final answer as-is: {reply}"
    );
}

#[tokio::test]
async fn observed_fix_intent_after_a_tool_call_nudges_and_continues() {
    // Live repro: after a read-only observation, the model identified the
    // exact edit but stopped on prose instead of calling the edit tool.
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({
            "content": null,
            "tool_calls": [{
                "id": "c1", "type": "function",
                "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
            }]
        }),
        serde_json::json!({
            "content": "I found the issue - there's an extra closing brace } on line 809 of help_sections.rs that's causing a syntax error. I need to remove this stray brace."
        }),
        serde_json::json!({ "content": "The stray brace is removed and the compile error is fixed." }),
    ])
    .await;
    assert_eq!(
        rounds, 3,
        "tool call, narrated edit intent, then post-nudge answer; got {rounds}"
    );
    assert!(
        reply.contains("compile error is fixed"),
        "returns the post-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("I need to remove"),
        "must not stop on the narrated edit intent: {reply}"
    );
}

#[tokio::test]
async fn pending_plan_final_answer_nudges_before_handoff() {
    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![
            Step {
                description: "convert help sections".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "fix format_command_list and update lib.rs".to_string(),
                status: StepStatus::Active,
            },
            Step {
                description: "add tests".to_string(),
                status: StepStatus::Todo,
            },
        ],
    });
    let (reply, rounds) = run_openai_script_with_ledger(
        vec![
            serde_json::json!({
                "content": "I need to finish Step 2, then Steps 3-5."
            }),
            serde_json::json!({
                "content": "Plan updated; continuing with the active step."
            }),
            serde_json::json!({
                "content": "The active step is now complete."
            }),
        ],
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert_eq!(
        rounds, 3,
        "open plan should force a completion-gate round and action-nudge follow-on narration"
    );
    assert!(
        reply.contains("complete"),
        "returns the post-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("I need to finish"),
        "must not accept a plain handoff while plan is open: {reply}"
    );
}

#[tokio::test]
async fn findings_summary_with_stale_plan_nudges_update_plan_then_continues() {
    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![
            Step {
                description: "convert help sections".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "wire progressive dispatch in lib.rs".to_string(),
                status: StepStatus::Active,
            },
            Step {
                description: "add tests".to_string(),
                status: StepStatus::Todo,
            },
        ],
    });
    let findings = "\
Summary of Findings

Across the tool calls, I observed two issues in newt-tui/src/help_sections.rs:
1. Duplicate function definitions
2. Stray closing brace

Current Status

The build is broken due to these syntax errors. The plan was at step 2, but we need to fix the immediate compilation issues first before proceeding with feature work.

Next Steps Required

To continue, I would need to remove the duplicate function using edit_file, locate and remove the stray brace, verify cargo check, then proceed with step 2 of the plan.

However, I've reached the tool-call limit and cannot make these edits now.";
    let (reply, rounds) = run_openai_script_with_ledger(
        vec![
            serde_json::json!({ "content": findings }),
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "plan_1",
                    "type": "function",
                    "function": {
                        "name": "update_plan",
                        "arguments": serde_json::json!({
                            "plan": [
                                {"step": "fix duplicate help rollup functions and stray brace", "status": "in_progress"},
                                {"step": "wire progressive dispatch in lib.rs", "status": "pending"},
                                {"step": "add rollup tests", "status": "pending"}
                            ]
                        }).to_string()
                    }
                }]
            }),
            serde_json::json!({
                "content": null,
                "tool_calls": [{
                    "id": "edit_1",
                    "type": "function",
                    "function": {
                        "name": "definitely_not_a_real_tool",
                        "arguments": "{}"
                    }
                }]
            }),
            serde_json::json!({ "content": "Done." }),
        ],
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert_eq!(
        rounds, 4,
        "findings summary should be nudged into update_plan, then a concrete tool"
    );
    assert_eq!(reply, "Done.");
    assert!(
        !reply.contains("tool-call limit"),
        "must not accept the handoff summary: {reply}"
    );
    let snap = ledger.snapshot();
    assert_eq!(
        snap.steps[0].description,
        "fix duplicate help rollup functions and stray brace"
    );
    assert_eq!(snap.steps[0].status, StepStatus::Active);
}

#[tokio::test]
async fn completed_plan_final_answer_is_accepted() {
    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![Step {
            description: "done".to_string(),
            status: StepStatus::Done,
        }],
    });
    let (reply, rounds) = run_openai_script_with_ledger(
        vec![serde_json::json!({
            "content": "All plan steps are complete."
        })],
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert_eq!(rounds, 1, "completed plan must not be nudged");
    assert!(reply.contains("complete"), "returns final answer: {reply}");
}

#[tokio::test]
async fn continuing_with_active_step_after_plan_nudge_gets_action_nudge() {
    let ledger = SessionStepLedger::default();
    ledger.restore(&PlanSnapshot {
        steps: vec![
            Step {
                description: "convert help sections".to_string(),
                status: StepStatus::Done,
            },
            Step {
                description: "insert progressive dispatch".to_string(),
                status: StepStatus::Active,
            },
            Step {
                description: "add tests".to_string(),
                status: StepStatus::Todo,
            },
        ],
    });
    let (reply, rounds) = run_openai_script_with_ledger(
        vec![
            serde_json::json!({
                "content": "I need to finish Step 2, then Steps 3-5."
            }),
            serde_json::json!({
                "content": "Plan is current — no update needed. Continuing with step 2: inserting the progressive dispatch into lib.rs."
            }),
            serde_json::json!({
                "content": "The edit is now complete."
            }),
        ],
        Some(&ledger as &dyn StepLedger),
    )
    .await;
    assert_eq!(
        rounds, 3,
        "plan nudge should be followed by an action nudge for continuing-with narration"
    );
    assert!(
        reply.contains("complete"),
        "returns the post-action-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("Continuing with step 2"),
        "must not stop on the continuing-with narration: {reply}"
    );
}

#[tokio::test]
async fn stale_file_blocker_nudges_ground_truth_check_and_continues() {
    let blocker = "\
Summary

What happened: The lib.rs file I was editing grew from ~9400 to ~16808 lines \
between reads — likely modified concurrently by another agent or tool. This \
means my old edit contexts are stale.

Why I'm blocked: I cannot safely use edit_file on lib.rs because the file has \
been modified out from under me. My old line references and context are invalid \
for an 8400-line larger file.

Final Answer / Recommendation

The operator should restore lib.rs to a known-good state (e.g., git checkout \
newt-tui/src/lib.rs).";
    let (reply, rounds) = run_openai_script(vec![
        serde_json::json!({ "content": blocker }),
        serde_json::json!({ "content": "Ground truth checked; lib.rs is clean, so I am continuing." }),
    ])
    .await;
    assert_eq!(
        rounds, 2,
        "stale-file blocker should get one verification nudge"
    );
    assert!(
        reply.contains("lib.rs is clean"),
        "returns the post-nudge answer: {reply}"
    );
    assert!(
        !reply.contains("git checkout"),
        "must not accept the unverified revert recommendation: {reply}"
    );
}

#[test]
fn looks_like_intent_to_act_separates_narration_from_final_answers() {
    // Real repro narrations that ended a turn — must read as intent-to-act.
    assert!(looks_like_intent_to_act(
        "Now I have everything I need. Let me make both edits now."
    ));
    assert!(looks_like_intent_to_act(
        "Now I'll add the --home flag to the Cli struct."
    ));
    assert!(looks_like_intent_to_act("Let me keep editing now."));
    assert!(looks_like_intent_to_act(
        "I'm going to edit the config file."
    ));
    assert!(looks_like_intent_to_act(
        "Let me understand what was already done on this branch and compare it with the issue requirements."
    ));
    assert!(looks_like_intent_to_act(
        "Let me check the current implementation and identify any gaps."
    ));
    assert!(looks_like_intent_to_act(
        "The help section logic itself has no tests yet.\n\nLet me commit this first step, then move on:"
    ));
    assert!(looks_like_intent_to_act(
        "Plan is current — no update needed. Continuing with step 2: inserting the progressive dispatch into lib.rs."
    ));
    assert!(looks_like_intent_to_act(
        "I found the issue - there's an extra closing brace } on line 809 of help_sections.rs that's causing a syntax error. I need to remove this stray brace."
    ));
    // Genuine sign-offs / answers — must NOT be nudged.
    assert!(!looks_like_intent_to_act("The capital of France is Paris."));
    assert!(!looks_like_intent_to_act(
        "I have finished editing the file and the tests pass."
    ));
    assert!(!looks_like_intent_to_act(
        "Here is a summary of what I found across the tool calls."
    ));
    // Borrowed-cue sign-off ("let me know" + a verb) — must NOT be nudged.
    assert!(!looks_like_intent_to_act(
        "Done. Let me know if you want any further changes."
    ));
    // A long narration whose 400-byte tail cut lands mid-multibyte-glyph
    // (each `…` is 3 bytes; 200 of them puts the cut at byte 211, not a char
    // boundary) must not panic the slice — and still classify as intent.
    let multibyte = format!("{}let me edit", "…".repeat(200));
    assert!(looks_like_intent_to_act(&multibyte));
}

#[test]
fn looks_like_unverified_stale_file_blocker_requires_file_stale_and_blocker_cues() {
    assert!(looks_like_unverified_stale_file_blocker(
        "The lib.rs file I was editing grew from ~9400 to ~16808 lines between reads. \
         Why I'm blocked: I cannot safely use edit_file because the file has been \
         modified out from under me. The operator should restore lib.rs."
    ));
    assert!(looks_like_unverified_stale_file_blocker(
        "My old line references are invalid and the context is stale. Any edit could \
         land in the wrong place and corrupt the code; recommendation: restore the file."
    ));
    assert!(!looks_like_unverified_stale_file_blocker(
        "The cache entry is stale, so I refreshed it and continued."
    ));
    assert!(!looks_like_unverified_stale_file_blocker(
        "I checked git diff and the file is clean, so I can continue from the verified contents."
    ));
}

#[test]
fn stale_file_ground_truth_nudge_names_read_only_checks_and_revert_guard() {
    let nudge = stale_file_ground_truth_nudge();
    assert!(nudge.contains("git status --short"), "{nudge}");
    assert!(nudge.contains("git diff -- <file>"), "{nudge}");
    assert!(nudge.contains("wc -l <file>"), "{nudge}");
    assert!(nudge.contains("re-read the exact target range"), "{nudge}");
    assert!(
        nudge.contains("Never recommend git checkout/revert"),
        "{nudge}"
    );
}
