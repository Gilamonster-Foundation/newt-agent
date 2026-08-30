use super::note_sink::tests::MockSink;
use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ]
}

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
        max_tool_rounds: 6,
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

fn body_json(req: &Request) -> serde_json::Value {
    serde_json::from_slice(&req.body).unwrap_or_default()
}

fn advertised_tool_names(body: &serde_json::Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d["function"]["name"].as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn messages_contain(body: &serde_json::Value, needle: &str) -> bool {
    body["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter().any(|m| {
                m["content"]
                    .as_str()
                    .map(|c| c.contains(needle))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Ollama-shaped responder: issues one save_note tool call, then a final
/// text answer once the "note saved:" tool result is visible in history.
/// Also records whether save_note was advertised and whether the memory
/// nudge line reached the model.
struct SaveNoteResponder {
    save_note_advertised: Arc<AtomicBool>,
    nudge_seen: Arc<AtomicBool>,
    final_answer: String,
}

impl Respond for SaveNoteResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        if advertised_tool_names(&body).contains(&"save_note".to_string()) {
            self.save_note_advertised.store(true, Ordering::SeqCst);
        }
        if messages_contain(&body, "[system reminder:")
            && messages_contain(&body, "without a saved note")
        {
            self.nudge_seen.store(true, Ordering::SeqCst);
        }
        if messages_contain(&body, "note saved:") {
            // The tool result round-tripped — answer for real now.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": self.final_answer }
            }))
        } else if body.get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{ "function": {
                        "name": "save_note",
                        "arguments": {
                            "action": "add",
                            "text": "user prefers vi keybindings"
                        }
                    }}]
                }
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "final summary" }
            }))
        }
    }
}

#[tokio::test]
async fn ollama_save_note_routes_to_sink_and_result_feeds_back() {
    let server = MockServer::start().await;
    let advertised = Arc::new(AtomicBool::new(false));
    let nudge_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SaveNoteResponder {
            save_note_advertised: advertised.clone(),
            nudge_seen: nudge_seen.clone(),
            final_answer: "noted, moving on".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut sink = MockSink::default();
    let mut c = ctx(&uri, &messages, &caveats);
    c.note_sink = Some(&mut sink);
    let (reply, _streamed, _usage, hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(
        advertised.load(Ordering::SeqCst),
        "save_note must be advertised when a sink is present"
    );
    assert_eq!(
        sink.calls,
        vec!["add:user prefers vi keybindings"],
        "the tool call must route through the sink"
    );
    assert_eq!(reply, "noted, moving on");
    assert_eq!(hallu, 0, "save_note is a real tool, not a hallucination");
    assert!(
        !nudge_seen.load(Ordering::SeqCst),
        "no nudge configured — none may be injected"
    );
}

/// Without a sink the tool must be absent from the advertised set, and a
/// configured nudge must NOT be appended (absent-without-sink).
struct NoSinkObserver {
    save_note_advertised: Arc<AtomicBool>,
    nudge_seen: Arc<AtomicBool>,
}

impl Respond for NoSinkObserver {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        if advertised_tool_names(&body).contains(&"save_note".to_string()) {
            self.save_note_advertised.store(true, Ordering::SeqCst);
        }
        if messages_contain(&body, "[system reminder:") {
            self.nudge_seen.store(true, Ordering::SeqCst);
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "plain answer" }
        }))
    }
}

#[tokio::test]
async fn without_sink_no_tool_and_no_nudge_even_when_due() {
    let server = MockServer::start().await;
    let advertised = Arc::new(AtomicBool::new(false));
    let nudge_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(NoSinkObserver {
            save_note_advertised: advertised.clone(),
            nudge_seen: nudge_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    // A nudge that is overdue (interval 1, one quiet turn already counted)…
    let mut nudge = NoteNudge::new(1);
    let _ = nudge.begin_turn();
    let mut c = ctx(&uri, &messages, &caveats);
    // …but NO sink: the loop must neither advertise nor nudge.
    c.note_nudge = Some(&mut nudge);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert_eq!(reply, "plain answer");
    assert!(
        !advertised.load(Ordering::SeqCst),
        "save_note advertised without a sink"
    );
    assert!(
        !nudge_seen.load(Ordering::SeqCst),
        "nudge injected without a sink"
    );
}

#[tokio::test]
async fn nudge_appended_to_user_message_when_due() {
    let server = MockServer::start().await;
    let advertised = Arc::new(AtomicBool::new(false));
    let nudge_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(NoSinkObserver {
            save_note_advertised: advertised.clone(),
            nudge_seen: nudge_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut sink = MockSink::default();
    // One quiet turn already elapsed → due on this (the next) turn.
    let mut nudge = NoteNudge::new(1);
    let _ = nudge.begin_turn();
    let mut c = ctx(&uri, &messages, &caveats);
    c.note_sink = Some(&mut sink);
    c.note_nudge = Some(&mut nudge);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert_eq!(reply, "plain answer");
    assert!(
        nudge_seen.load(Ordering::SeqCst),
        "the reminder line must reach the model on the due turn"
    );
}

#[tokio::test]
async fn organic_save_resets_the_nudge_counter() {
    let server = MockServer::start().await;
    let advertised = Arc::new(AtomicBool::new(false));
    let nudge_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SaveNoteResponder {
            save_note_advertised: advertised.clone(),
            nudge_seen: nudge_seen.clone(),
            final_answer: "done".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut sink = MockSink::default();
    let mut nudge = NoteNudge::new(1);
    let mut c = ctx(&uri, &messages, &caveats);
    c.note_sink = Some(&mut sink);
    c.note_nudge = Some(&mut nudge);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");
    assert_eq!(reply, "done");
    assert_eq!(sink.calls.len(), 1, "the model saved organically");

    // The turn included an organic save → the counter restarted, so the
    // next turn must NOT be nudged (without the save, interval=1 would
    // have made it due).
    assert!(
        nudge.begin_turn().is_none(),
        "organic save_note use must reset the nudge counter"
    );
}

/// OpenAI-shaped mirror: save_note advertised + routed, nudge appended.
struct OpenAiSaveNoteResponder {
    save_note_advertised: Arc<AtomicBool>,
    nudge_seen: Arc<AtomicBool>,
}

impl Respond for OpenAiSaveNoteResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        if advertised_tool_names(&body).contains(&"save_note".to_string()) {
            self.save_note_advertised.store(true, Ordering::SeqCst);
        }
        if messages_contain(&body, "[system reminder:") {
            self.nudge_seen.store(true, Ordering::SeqCst);
        }
        if messages_contain(&body, "note saved:") {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "openai noted" } }]
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "save_note",
                            "arguments": "{\"action\":\"add\",\"text\":\"CI gate is just check\"}"
                        }
                    }]
                }}]
            }))
        }
    }
}

#[tokio::test]
async fn openai_save_note_routes_and_nudge_appends() {
    let server = MockServer::start().await;
    let advertised = Arc::new(AtomicBool::new(false));
    let nudge_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiSaveNoteResponder {
            save_note_advertised: advertised.clone(),
            nudge_seen: nudge_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut sink = MockSink::default();
    let mut nudge = NoteNudge::new(1);
    let _ = nudge.begin_turn(); // due on this turn
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.note_sink = Some(&mut sink);
    c.note_nudge = Some(&mut nudge);
    let (reply, _, _, hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai loop should succeed");

    assert_eq!(reply, "openai noted");
    assert_eq!(sink.calls, vec!["add:CI gate is just check"]);
    assert!(advertised.load(Ordering::SeqCst));
    assert!(nudge_seen.load(Ordering::SeqCst));
    assert_eq!(hallu, 0);
}

/// A sink error (here: the 19.1 over-budget curator error) must round-trip
/// to the model verbatim as the tool result so it can replace/remove and
/// retry — pinned end-to-end through the loop.
struct ErrorEchoResponder {
    error_seen_by_model: Arc<AtomicBool>,
}

impl Respond for ErrorEchoResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        if messages_contain(&body, "Replace or remove existing entries first")
            && messages_contain(&body, "1. an existing entry")
        {
            self.error_seen_by_model.store(true, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "I will curate first" }
            }))
        } else if body.get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{ "function": {
                        "name": "save_note",
                        "arguments": { "action": "add", "text": "too big" }
                    }}]
                }
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "final summary" }
            }))
        }
    }
}

#[tokio::test]
async fn over_budget_error_round_trips_to_the_model() {
    let server = MockServer::start().await;
    let error_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ErrorEchoResponder {
            error_seen_by_model: error_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut sink = MockSink {
            fail_with: Some(
                "NOTES.md is full: this write needs 99/50 chars. \
                 Replace or remove existing entries first.\nCurrent entries:\n  1. an existing entry"
                    .into(),
            ),
            ..Default::default()
        };
    let mut c = ctx(&uri, &messages, &caveats);
    c.note_sink = Some(&mut sink);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert_eq!(reply, "I will curate first");
    assert!(
        error_seen.load(Ordering::SeqCst),
        "the curator error (full entry list + instruction) must reach the model verbatim"
    );
}
