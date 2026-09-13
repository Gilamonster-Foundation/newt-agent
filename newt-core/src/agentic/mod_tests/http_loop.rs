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
        smart_harness: None,
        rewrites_history: true,
        url: server_uri,
        model: "test-model",
        kind: BackendKind::Ollama,
        api_key: None,
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

/// #123: an OpenAI-compatible SSE body replaying one already-accepted answer,
/// for the streaming re-issue of the round that ends the turn. A responder
/// serves this WITHOUT advancing its round counter — the re-issue re-asks a
/// question the loop already had answered, so counting it would turn a
/// `rounds` assertion into a request count.
fn sse_replay(text: &str) -> ResponseTemplate {
    let frame =
        serde_json::json!({"choices": [{"delta": {"content": text}, "finish_reason":"stop"}]});
    let body = format!("data: {frame}\n\ndata: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream")
}

/// One terminal response authorizes one matching display reissue. A changed
/// request invalidates the pending replay; every later logical turn advances.
#[derive(Default)]
pub(super) struct DisplayReplay(Mutex<Option<Vec<u8>>>);

impl DisplayReplay {
    pub(super) fn arm(&self, request: &Request) {
        *self.0.lock().unwrap() = Some(request.body.clone());
    }

    pub(super) fn take(&self, request: &Request) -> bool {
        self.0
            .lock()
            .unwrap()
            .take()
            .is_some_and(|body| is_stream(request) && body == request.body)
    }
}

fn ndjson(lines: &[serde_json::Value]) -> ResponseTemplate {
    let body: String = lines
        .iter()
        .map(|l| format!("{l}\n"))
        .collect::<Vec<_>>()
        .join("");
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
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

// -- Narrate-then-stop rescue: bounded no-tool-call auto-continue ---------

/// OpenAI responder that serves one scripted message per logical round.
/// Primary calls stream too, so only an identical request immediately after a
/// no-tool response can consume its one pending display replay. A changed
/// request advances the script; out-of-range rounds repeat its last entry.
struct ScriptedOpenAi {
    round: Arc<AtomicUsize>,
    script: Vec<serde_json::Value>,
    /// Exact request bytes and message eligible for one display reissue.
    pending_replay: Mutex<Option<(Vec<u8>, serde_json::Value)>>,
}
impl Respond for ScriptedOpenAi {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        // Consume or invalidate the pending replay before deciding the round.
        // Keeping it after replay would swallow a later identical turn.
        let mut pending = self.pending_replay.lock().unwrap();
        if let Some((body, message)) = pending.take() {
            if is_stream(req) && body == req.body {
                return scripted_openai_response(req, message);
            }
        }
        let i = self.round.fetch_add(1, Ordering::SeqCst);
        let msg = self
            .script
            .get(i)
            .or_else(|| self.script.last())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "content": "final." }));
        if !msg["tool_calls"]
            .as_array()
            .is_some_and(|calls| !calls.is_empty())
        {
            *pending = Some((req.body.clone(), msg.clone()));
        }
        scripted_openai_response(req, msg)
    }
}

/// Change only the wire envelope: retain script text, tool identities and
/// arguments verbatim, adding the positional index required by SSE deltas.
fn scripted_openai_response(req: &Request, mut message: serde_json::Value) -> ResponseTemplate {
    if !is_stream(req) {
        return ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"choices":[{"message":message}]}));
    }
    let finish = if let Some(calls) = message["tool_calls"]
        .as_array_mut()
        .filter(|c| !c.is_empty())
    {
        for (index, call) in calls.iter_mut().enumerate() {
            call["index"] = serde_json::json!(index);
        }
        "tool_calls"
    } else {
        "stop"
    };
    let chunk = serde_json::json!({"choices":[{"index":0,"delta":message}]});
    let done = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]});
    let body = format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n");
    ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream")
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
            pending_replay: Default::default(),
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

/// Grounds scripted round accounting in real HTTP requests: each accepted
/// answer is displayed once, and a later identical turn still advances.
#[tokio::test]
async fn scripted_openai_identical_next_turn_consumes_the_next_answer() {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedOpenAi {
            round: round.clone(),
            script: vec![
                serde_json::json!({"content":"First answer."}),
                serde_json::json!({"content":"Second answer."}),
            ],
            pending_replay: Default::default(),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    // Reuse one immutable prompt receipt so this fixture sends byte-identical
    // turns; the convenience entry point would mint a fresh address each time.
    let turn = crate::TurnPromptContext::ephemeral_operator(
        "scripted-replay",
        "do the thing",
        "do the thing",
    );
    for expected in ["First answer.", "Second answer."] {
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.prompt_disposition = PromptDisposition::Explain;
        let (reply, streamed, _, _) = chat_complete_with_prompt(c, Some(&turn), None, &mut NoMcp)
            .await
            .expect("scripted streamed primary must produce a complete answer");
        assert_eq!(reply, expected);
        assert!(streamed, "the display reissue must carry real SSE text");
    }
    assert_eq!(round.load(Ordering::SeqCst), 2, "two logical rounds");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4, "primary and display for each turn");
    assert!(requests.iter().all(is_stream), "all generations stream");
    assert_eq!(requests[0].body, requests[1].body, "first display replay");
    assert_eq!(requests[2].body, requests[3].body, "second display replay");
    assert_eq!(requests[0].body, requests[2].body, "identical next turn");
}

#[cfg(test)]
#[path = "http_stream_empty.rs"]
mod stream_empty;

#[cfg(test)]
#[path = "http_tool_recovery.rs"]
mod tool_recovery;

#[cfg(test)]
#[path = "http_wire_contract.rs"]
mod wire_contract;

#[cfg(test)]
#[path = "http_compaction.rs"]
mod compaction;

#[cfg(test)]
#[path = "http_finalization.rs"]
mod finalization;

#[cfg(test)]
#[path = "http_reasoning_replay.rs"]
mod reasoning_replay;

#[cfg(test)]
#[path = "http_reasoning_overflow.rs"]
mod reasoning_overflow;

#[path = "http_context_exceeded.rs"]
mod context_exceeded;

#[path = "http_token_count.rs"]
mod token_count;

#[path = "http_context_exceeded_optional.rs"]
mod optional_context_exceeded;

#[cfg(test)]
#[path = "http_mcp_routing.rs"]
mod mcp_routing;

#[cfg(test)]
#[path = "http_narration.rs"]
mod narration;

#[cfg(test)]
#[path = "http_plan_handoff.rs"]
mod plan_handoff;

#[cfg(test)]
#[path = "http_verification.rs"]
mod verification;

#[cfg(test)]
#[path = "http_spill_retrieval.rs"]
mod spill_retrieval;

#[cfg(test)]
#[path = "http_smart_harness.rs"]
mod smart_harness;
