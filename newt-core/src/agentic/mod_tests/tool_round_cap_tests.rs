use super::*;
use crate::caveats::Caveats;
use crate::{BackendKind, MemMessage};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Was the `"tools"` key present on this request body?
fn request_has_tools(req: &Request) -> bool {
    serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .map(|v| v.get("tools").is_some())
        .unwrap_or(false)
}

/// Ollama-shaped responder: returns a tool call whenever `tools` are
/// offered, and a plain text answer once they are withheld. Counts the
/// number of tool-offering requests it served.
struct OllamaResponder {
    tool_rounds_served: Arc<AtomicUsize>,
    final_answer: String,
}

impl Respond for OllamaResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if request_has_tools(req) {
            self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                    }]
                }
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": self.final_answer }
            }))
        }
    }
}

/// OpenAI-shaped responder: same logic, OpenAI `choices[0].message` shape.
struct OpenAiResponder {
    tool_rounds_served: Arc<AtomicUsize>,
    final_answer: String,
}

impl Respond for OpenAiResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if request_has_tools(req) {
            self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
                    }]
                }}]
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": self.final_answer } }]
            }))
        }
    }
}

struct ProtectedCapResponder {
    openai: bool,
    exact_task: String,
    pair_seen_on_final: Arc<std::sync::atomic::AtomicBool>,
    omission_seen_on_final: Arc<std::sync::atomic::AtomicBool>,
}

impl Respond for ProtectedCapResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if request_has_tools(req) {
            if self.openai {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_cap",
                            "type": "function",
                            "function": {
                                "name": "definitely_not_a_real_tool",
                                "arguments": "{}"
                            }
                        }]
                    }}]
                }));
            }
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{"function": {
                        "name": "definitely_not_a_real_tool",
                        "arguments": {}
                    }}]
                }
            }));
        }

        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let system_count = messages
            .iter()
            .filter(|message| message["role"].as_str() == Some("system"))
            .count();
        let pair_seen = messages.windows(2).any(|pair| {
            let card = pair[0]["role"].as_str() == Some("system")
                && pair[0]["content"].as_str().is_some_and(|content| {
                    (if self.openai {
                        content.contains(prompt_read::ACTIVE_PROMPT_PREFIX)
                    } else {
                        content.starts_with(prompt_read::ACTIVE_PROMPT_PREFIX)
                    }) && content.contains("address: prompt:")
                        && !content.contains("<ephemeral-unrecorded>")
                });
            card && pair[1]["role"].as_str() == Some("user")
                && pair[1]["content"].as_str() == Some(self.exact_task.as_str())
        });
        self.pair_seen_on_final.store(
            pair_seen && (!self.openai || system_count == 1),
            Ordering::SeqCst,
        );
        self.omission_seen_on_final.store(
            messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("messages omitted"))
            }),
            Ordering::SeqCst,
        );

        if self.openai {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "cap summary"}}]
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "cap summary"}
            }))
        }
    }
}

struct OpenAiReasoningCapResponder {
    round: AtomicUsize,
    first_plan_seen_on_final: Arc<std::sync::atomic::AtomicBool>,
    policy_seen_on_final: Arc<std::sync::atomic::AtomicBool>,
}

impl Respond for OpenAiReasoningCapResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if request_has_tools(req) {
            let round = self.round.fetch_add(1, Ordering::SeqCst);
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "reasoning_content": format!("persistent plan round {round}"),
                    "tool_calls": [{
                        "id": "call_cap",
                        "type": "function",
                        "function": {
                            "name": "definitely_not_a_real_tool",
                            "arguments": "{}"
                        }
                    }]
                }}]
            }));
        }

        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let first_plan_seen = body["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["reasoning_content"].as_str() == Some("persistent plan round 0")
            })
        });
        self.first_plan_seen_on_final
            .store(first_plan_seen, Ordering::SeqCst);
        self.policy_seen_on_final.store(
            body["max_tokens"] == 10_000
                && body["temperature"] == 0.6
                && body["top_p"] == 0.95
                && body["chat_template_kwargs"]["enable_thinking"] == true
                && body.get("parallel_tool_calls").is_none(),
            Ordering::SeqCst,
        );
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "cap summary"}}]
        }))
    }
}

fn msgs() -> Vec<MemMessage> {
    vec![
        MemMessage::system("you are a test"),
        MemMessage::user("do the thing"),
    ]
}

fn hard_budget_ctx<'a>(
    url: &'a str,
    messages: &'a [MemMessage],
    caveats: &'a Caveats,
    task: &'a str,
    kind: BackendKind,
) -> ChatCtx<'a> {
    ChatCtx {
        url,
        model: "tiny-context-model",
        kind,
        api_key: (kind == BackendKind::Openai).then_some("sk-test"),
        messages,
        task,
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
        max_tool_rounds: 1,
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
        inference_timeout_secs: 5,
        mid_loop_trim_threshold: 40,
        compaction_trigger_policy: crate::CompactionTriggerPolicy::HeadroomAware,
        mid_loop_trim_tokens: None,
        max_ok_input: None,
        build_check_cmd: None,
        safe_context: Some(256),
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

async fn assert_no_requests(server: &MockServer) {
    assert!(
        server
            .received_requests()
            .await
            .expect("wiremock request journal")
            .is_empty(),
        "irreducible-prompt refusal must happen before HTTP dispatch"
    );
}

#[tokio::test]
async fn ollama_cap_trim_keeps_headless_active_pair_after_more_than_six_trailing_messages() {
    let server = MockServer::start().await;
    let task = "CURRENT-B: keep this exact prompt through cap trim";
    let pair_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let omission_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ProtectedCapResponder {
            openai: false,
            exact_task: task.to_string(),
            pair_seen_on_final: pair_seen.clone(),
            omission_seen_on_final: omission_seen.clone(),
        })
        .mount(&server)
        .await;
    let messages = vec![
        MemMessage::system("base"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    context.safe_context = None;
    context.max_tool_rounds = 4;
    let (reply, _, _, _) = chat_complete(context, &mut NoMcp)
        .await
        .expect("cap exit succeeds");
    assert!(reply.starts_with("cap summary"), "{reply}");
    assert!(pair_seen.load(Ordering::SeqCst));
    assert!(
        omission_seen.load(Ordering::SeqCst),
        "four tool rounds create >6 trailing messages and force a real trim"
    );
}

#[tokio::test]
async fn openai_cap_trim_keeps_headless_active_pair_after_more_than_six_trailing_messages() {
    let server = MockServer::start().await;
    let task = "CURRENT-B: keep this exact OpenAI prompt through cap trim";
    let pair_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let omission_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ProtectedCapResponder {
            openai: true,
            exact_task: task.to_string(),
            pair_seen_on_final: pair_seen.clone(),
            omission_seen_on_final: omission_seen.clone(),
        })
        .mount(&server)
        .await;
    let messages = vec![
        MemMessage::system("base"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    context.safe_context = None;
    context.max_tool_rounds = 4;
    let (reply, _, _, _) = openai_chat_complete(context, &mut NoMcp)
        .await
        .expect("cap exit succeeds");
    assert!(reply.starts_with("cap summary"), "{reply}");
    assert!(pair_seen.load(Ordering::SeqCst));
    assert!(
        omission_seen.load(Ordering::SeqCst),
        "four tool rounds create >6 trailing messages and force a real trim"
    );
}

#[tokio::test]
async fn openai_cap_exit_preserves_the_full_current_turn_reasoning_tail() {
    let server = MockServer::start().await;
    let first_plan_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let policy_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningCapResponder {
            round: AtomicUsize::new(0),
            first_plan_seen_on_final: first_plan_seen.clone(),
            policy_seen_on_final: policy_seen.clone(),
        })
        .mount(&server)
        .await;
    let task = "keep the active plan through cap exit";
    let messages = vec![MemMessage::system("base"), MemMessage::user(task)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    context.safe_context = None;
    context.max_tool_rounds = 4;
    context.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;
    context.cognition = Some(crate::role_profile::Cognition::Deliberating);
    context.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        chat_template_kwargs: Some(true),
        parallel_tool_calls: Some(false),
        bounded_reasoning_continuation: Some(true),
    };

    let (reply, _, _, _) = openai_chat_complete(context, &mut NoMcp)
        .await
        .expect("cap exit succeeds");
    assert!(reply.starts_with("cap summary"), "{reply}");
    assert!(
        first_plan_seen.load(Ordering::SeqCst),
        "the tools-disabled cap-exit request must retain the first current-turn plan"
    );
    assert!(
        policy_seen.load(Ordering::SeqCst),
        "the cap-exit request must retain cognition policy and omit tool-only fields"
    );
}

#[test]
fn ollama_auth_headers_builds_sensitive_bearer_or_nothing() {
    let h = ollama_auth_headers(Some("ol-cloud-key"));
    let v = h.get(reqwest::header::AUTHORIZATION).expect("header set");
    assert_eq!(v.to_str().unwrap(), "Bearer ol-cloud-key");
    assert!(v.is_sensitive(), "token must never reach debug logs");
    assert!(ollama_auth_headers(None).is_empty());
    assert!(ollama_auth_headers(Some("   ")).is_empty());
}

#[tokio::test]
async fn ollama_loop_sends_bearer_auth_on_every_request_when_key_configured() {
    // Field regression (Ollama Cloud 401): the wire spoke plain HTTP with
    // the key dropped on the floor. Every request the loop makes — tool
    // rounds AND the final summary — must now carry the bearer.
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaResponder {
            tool_rounds_served: served.clone(),
            final_answer: "authed answer".into(),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "do the thing",
        BackendKind::Ollama,
    );
    ctx.api_key = Some("ol-cloud-key");
    ctx.safe_context = None;
    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("turn completes");
    assert_eq!(reply, "authed answer");
    let reqs = server.received_requests().await.expect("journal");
    assert!(!reqs.is_empty());
    for r in &reqs {
        assert_eq!(
            r.headers.get("authorization").map(|v| v.to_str().unwrap()),
            Some("Bearer ol-cloud-key"),
            "unauthenticated request slipped through to {}",
            r.url
        );
    }
}

#[tokio::test]
async fn ollama_loop_honors_configured_cap_and_returns_real_final_answer() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaResponder {
            tool_rounds_served: served.clone(),
            final_answer: "here is my partial summary".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let cap = 3;
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let (reply, streamed, _usage, _hallu) = chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: cap,
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
            inference_timeout_secs: 120,
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
            end_reason: Some(&mut end_reason),
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
        },
        &mut NoMcp,
    )
    .await
    .expect("chat_complete should succeed");

    // The cap was honoured: exactly `cap` tool-offering rounds were served.
    assert_eq!(served.load(Ordering::SeqCst), cap);
    // The cap-exit issued a final tools-disabled completion and returned
    // its text — NOT the dead placeholder.
    assert!(reply.starts_with("here is my partial summary"), "{reply}");
    assert_ne!(reply, "(reached tool-round limit)");
    assert!(!streamed);
    // The cap exit reports itself (acceptance forensics, commit 4).
    assert_eq!(end_reason, Some(crate::TurnEndReason::RoundCap));
}

/// A completed-spill viewport painted by a tool result is DISMISSED by the
/// loop's hooks — round-top and cap-exit — and cannot outlive the turn
/// (the fn-exit guard discards on every path). Pins the load-bearing
/// erase-before-canonical-write discipline the call sites implement.
#[derive(Default)]
struct DismissTrackingRenderer {
    rendered: AtomicUsize,
    erased: AtomicUsize,
    active: std::sync::atomic::AtomicBool,
}

impl CompletedSpillRenderer for DismissTrackingRenderer {
    fn render_completed(&self, _output: &str, _width: usize, _max_height: usize) -> usize {
        self.rendered.fetch_add(1, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);
        3
    }
    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
    fn erase(&self) {
        self.erased.fetch_add(1, Ordering::SeqCst);
        self.active.store(false, Ordering::SeqCst);
    }
    fn discard(&self) {
        self.active.store(false, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn ollama_loop_dismisses_the_completed_viewport_it_painted() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaResponder {
            tool_rounds_served: served.clone(),
            final_answer: "done".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "do the thing",
        BackendKind::Ollama,
    );
    let renderer = std::sync::Arc::new(DismissTrackingRenderer::default());
    ctx.completed_spill_renderer = Some(renderer.clone());
    ctx.safe_context = None;

    let _ = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("turn completes");

    assert!(
        renderer.rendered.load(Ordering::SeqCst) > 0,
        "the wired renderer painted for tool results"
    );
    assert!(
        renderer.erased.load(Ordering::SeqCst) > 0,
        "the loop's dismiss hooks erased the viewport"
    );
    assert!(
        !renderer.is_active(),
        "no viewport survives the turn (round-top / cap-exit / guard)"
    );
}

#[tokio::test]
async fn ollama_cap_exit_preserves_action_intent_as_a_paused_handoff() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "I have two issues: duplicate topic_has_rollups and a stray brace. Let me fix both — read around 490 to see what needs removing, then verify with a build check."
                }
            })))
            .mount(&server)
            .await;

    let client = reqwest::Client::new();
    let chat_url = format!("{}/api/chat", server.uri());
    let (reply, streamed, _usage) = final_summary_ollama(
        &client,
        &chat_url,
        "test-model",
        Vec::new(),
        CapExit {
            max_tool_rounds: 25,
            accumulated: None,
            wasted_calls: 0,
            progress: Some("<plan>1. [ ] fix duplicate helper definitions</plan>".to_string()),
            observed: Vec::new(),
            request_budget: None,
            calibration: 1.0,
            estimation: crate::tokens::TokenEstimation::default(),
            ollama_num_ctx: Some(4_096),
        },
    )
    .await
    .expect("final summary helper should return a fallback");

    assert!(!streamed);
    assert!(reply.contains("tool-round limit (25"), "{reply}");
    assert!(
        reply.contains("Let me fix both"),
        "the model's progress prose should survive the pause: {reply}"
    );
    assert!(
        reply.contains("have not run"),
        "future actions must be clearly marked as pending: {reply}"
    );
    assert!(reply.contains("progress handoff"), "{reply}");
    assert!(
        !reply.contains("final summarization request also failed"),
        "{reply}"
    );
    assert!(reply.contains("Captured working state"), "{reply}");
    assert!(
        reply.contains("fix duplicate helper definitions"),
        "{reply}"
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["options"]["num_ctx"], 4_096);
}

#[tokio::test]
async fn openai_cap_exit_preserves_progress_as_a_paused_handoff() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "content": "Summary of Findings\n\nThe configuration path is wired.\n\nNext Steps Required\n\nRun cargo check and open the pull request."
                    }
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 8}
            })))
            .mount(&server)
            .await;

    let client = reqwest::Client::new();
    let (reply, streamed, usage) = final_summary_openai(
        &client,
        &format!("{}/v1/chat/completions", server.uri()),
        "test-model",
        None,
        Vec::new(),
        generation_policy::GenerationPolicy::default(),
        CapExit {
            max_tool_rounds: 40,
            accumulated: Some(crate::TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
            }),
            wasted_calls: 0,
            progress: None,
            observed: Vec::new(),
            request_budget: None,
            calibration: 1.0,
            estimation: crate::tokens::TokenEstimation::default(),
            ollama_num_ctx: None,
        },
    )
    .await
    .expect("OpenAI cap summary should become a paused handoff");

    assert!(!streamed);
    assert!(reply.contains("Summary of Findings"), "{reply}");
    assert!(reply.contains("have not run"), "{reply}");
    assert!(reply.contains("progress handoff"), "{reply}");
    assert_eq!(
        usage,
        Some(crate::TokenUsage {
            input_tokens: 100,
            output_tokens: 58,
        })
    );

    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        body.get("tools").is_none(),
        "cap request stays tools-disabled"
    );
    assert!(
        body["messages"].to_string().contains("progress update"),
        "the model is explicitly asked for a resumable progress update"
    );
}

#[tokio::test]
async fn ollama_cap_exit_refuses_giant_fresh_result_before_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": "must not be dispatched" }
        })))
        .mount(&server)
        .await;

    let exact_task = "CURRENT-B: preserve this exact active operator prompt";
    let mut messages = vec![
        serde_json::json!({
            "role": "system",
            "content": format!("{}\naddress: prompt:test", prompt_read::ACTIVE_PROMPT_PREFIX),
        }),
        serde_json::json!({"role": "user", "content": exact_task}),
        serde_json::json!({
            "role": "assistant",
            "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "huge.txt"}}}],
        }),
        serde_json::json!({"role": "tool", "content": "x".repeat(32_000)}),
    ];
    let head = protected_prompt_head_len(&messages, prompt_read::ACTIVE_PROMPT_PREFIX);
    messages = trim_for_summary(&messages, head, 6);
    let client = reqwest::Client::new();
    let (reply, streamed, usage) = final_summary_ollama(
        &client,
        &format!("{}/api/chat", server.uri()),
        "tiny-model",
        messages,
        CapExit {
            max_tool_rounds: 1,
            accumulated: None,
            wasted_calls: 0,
            progress: None,
            observed: Vec::new(),
            request_budget: Some(2_000),
            calibration: 1.0,
            estimation: crate::tokens::TokenEstimation::default(),
            ollama_num_ctx: Some(2_500),
        },
    )
    .await
    .expect("oversized cap exit returns deterministic fallback");
    assert!(!streamed);
    assert!(usage.is_none());
    assert!(reply.contains("tool-round limit (1"), "{reply}");
    assert!(
        reply.contains("final summarization request also failed"),
        "{reply}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("wiremock request journal")
            .is_empty(),
        "the oversized cap-exit request must never reach the backend"
    );
}

#[tokio::test]
async fn openai_cap_exit_refuses_giant_fresh_result_before_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "must not be dispatched"}}]
        })))
        .mount(&server)
        .await;

    let messages = vec![
        serde_json::json!({
            "role": "system",
            "content": format!("{}\naddress: prompt:test", prompt_read::ACTIVE_PROMPT_PREFIX),
        }),
        serde_json::json!({"role": "user", "content": "CURRENT-B exact task"}),
        serde_json::json!({"role": "tool", "content": "x".repeat(32_000)}),
    ];
    let client = reqwest::Client::new();
    let (reply, streamed, usage) = final_summary_openai(
        &client,
        &format!("{}/v1/chat/completions", server.uri()),
        "tiny-model",
        None,
        messages,
        generation_policy::GenerationPolicy::default(),
        CapExit {
            max_tool_rounds: 1,
            accumulated: None,
            wasted_calls: 0,
            progress: None,
            observed: Vec::new(),
            request_budget: Some(2_000),
            calibration: 1.0,
            estimation: crate::tokens::TokenEstimation::default(),
            ollama_num_ctx: None,
        },
    )
    .await
    .expect("oversized cap exit returns deterministic fallback");
    assert!(!streamed);
    assert!(usage.is_none());
    assert!(reply.contains("tool-round limit (1"), "{reply}");
    assert!(
        server
            .received_requests()
            .await
            .expect("wiremock request journal")
            .is_empty(),
        "the oversized cap-exit request must never reach the backend"
    );
}

/// UAT (Step 27.3 + 27.5, simulated integration): a thrash run — a DISTINCT
/// failing tool call every round (so the failed-call count climbs to the
/// cap) AND a final summary that also errors. The cap-exit must be HONEST:
/// name the tooling problem, never advise "raise max_tool_rounds".
struct ThrashResponder {
    round: AtomicUsize,
}

impl Respond for ThrashResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if request_has_tools(req) {
            let n = self.round.fetch_add(1, Ordering::SeqCst);
            // A distinct unknown tool each round → each fails and is NOT a
            // repeat, so the guard records every one (wasted_calls climbs to
            // the cap, which is what flips the cap-exit to honest advice).
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "function": { "name": format!("bogus_tool_{n}"), "arguments": {} }
                    }]
                }
            }))
        } else {
            // The final tools-disabled summary request ALSO fails (500),
            // forcing the cap_exit_fallback path.
            ResponseTemplate::new(500).set_body_string("model exploded")
        }
    }
}

#[tokio::test]
async fn uat_thrash_run_gets_honest_cap_exit_not_raise_the_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ThrashResponder {
            round: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let cap = 3;
    let (reply, _streamed, _usage, hallu) = chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: cap,
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
            inference_timeout_secs: 120,
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
        },
        &mut NoMcp,
    )
    .await
    .expect("chat_complete should succeed even when the summary fails");

    // Every round emitted a (distinct) bogus call → counted as a hallucination.
    assert_eq!(hallu, cap as u32, "each round hallucinated a tool");
    // Step 27.5: the cap-exit is HONEST — a tooling problem, NOT "raise the cap".
    assert!(
        reply.contains("tool calls that failed"),
        "honest advice expected, got: {reply}"
    );
    assert!(
        !reply.contains("raise [tui].max_tool_rounds"),
        "must not blame the round cap on a thrash run: {reply}"
    );
}

#[tokio::test]
async fn a_set_cancel_flag_abandons_the_turn_before_any_network_call() {
    // The interrupt checkpoint at the round-loop top runs before the first
    // request, so a pre-tripped flag returns instantly — the bogus URL
    // (a closed port) is never contacted. If the checkpoint regressed,
    // the dispatch would try to connect and this would not return empty.
    let messages = msgs();
    let caveats = Caveats::top();
    let flag = std::sync::atomic::AtomicBool::new(true);
    let (reply, streamed, usage, hallu) = chat_complete(
        ChatCtx {
            url: "http://127.0.0.1:1",
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: 5,
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
            inference_timeout_secs: 120,
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
            cancel: Some(&flag),
            live_tool_output: None,
            git_tool: None,
            crew_runner: None,
            operating_mode_control: None,
            plan_mode_control: None,
            steering: None,
            completed_spill_renderer: None,
        },
        &mut NoMcp,
    )
    .await
    .expect("an interrupted turn still returns Ok, just empty");
    assert!(reply.is_empty(), "interrupted before any model output");
    assert!(!streamed);
    assert!(usage.is_none());
    assert_eq!(hallu, 0);
}

/// Serves one tool call, and — simulating the operator typing WHILE that
/// request is on the wire — submits a steering message from inside the
/// responder. The next round's request body must carry it.
///
/// Submitting from the responder rather than pre-loading the queue is the
/// point: a pre-loaded queue would also pass if steering were only ever
/// drained once, before the turn started. This can only pass if the drain
/// really happens at each round boundary.
struct SteersMidTurn {
    inbox: std::sync::Arc<SessionSteeringInbox>,
    requests_seen: Arc<AtomicUsize>,
    steer: String,
    final_answer: String,
}

impl Respond for SteersMidTurn {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.requests_seen.fetch_add(1, Ordering::SeqCst) == 0 {
            self.inbox.submit(self.steer.clone());
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                    }]
                }
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": { "content": self.final_answer }
        }))
    }
}

/// Every user-role content string in an Ollama request body.
fn user_contents(req: &wiremock::Request) -> Vec<String> {
    let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
    body["messages"]
        .as_array()
        .map(|msgs| {
            msgs.iter()
                .filter(|m| m["role"] == "user")
                .filter_map(|m| m["content"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// #952/#1669: the operator's mid-turn correction reaches the model at the
/// NEXT round boundary — not at the round cap, and not never.
#[tokio::test]
async fn steering_submitted_mid_turn_appears_in_the_next_rounds_request() {
    const STEER: &str = "don't change the public API";
    let server = MockServer::start().await;
    let inbox = std::sync::Arc::new(SessionSteeringInbox::new());
    let seen = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SteersMidTurn {
            inbox: inbox.clone(),
            requests_seen: seen.clone(),
            steer: STEER.into(),
            final_answer: "acknowledged".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "do the thing",
        BackendKind::Ollama,
    );
    // `hard_budget_ctx` exists to prove the 256-token refusal; this test
    // is about round boundaries, so give it an ordinary budget.
    ctx.model = "test-model";
    ctx.safe_context = None;
    ctx.max_tool_rounds = 3;
    ctx.steering = Some(inbox.as_ref() as &dyn SteeringInbox);
    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("turn completes");
    assert_eq!(reply, "acknowledged");

    let reqs = server.received_requests().await.expect("journal");
    assert!(
        reqs.len() >= 2,
        "need a second round to observe the delivery, got {}",
        reqs.len()
    );
    assert!(
        !user_contents(&reqs[0]).iter().any(|c| c.contains(STEER)),
        "the steer did not exist yet when round 0 was sent"
    );
    assert!(
        user_contents(&reqs[1]).iter().any(|c| c.contains(STEER)),
        "round 1 must carry the operator's mid-turn correction; got {:?}",
        user_contents(&reqs[1])
    );
    assert_eq!(inbox.pending(), 0, "delivery consumes the queue");
}

/// The regression floor: with no inbox lent, nothing about the request
/// bodies changes. Every headless / eval caller passes `None`.
#[tokio::test]
async fn no_inbox_means_no_extra_user_messages() {
    let server = MockServer::start().await;
    let seen = Arc::new(AtomicUsize::new(0));
    let idle = std::sync::Arc::new(SessionSteeringInbox::new());
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SteersMidTurn {
            // Submits into an inbox the loop was never given.
            inbox: idle.clone(),
            requests_seen: seen.clone(),
            steer: "never delivered".into(),
            final_answer: "done".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "do the thing",
        BackendKind::Ollama,
    );
    ctx.model = "test-model";
    ctx.safe_context = None;
    ctx.max_tool_rounds = 3;
    ctx.steering = None;
    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("journal");
    for r in &reqs {
        assert!(
            !user_contents(r)
                .iter()
                .any(|c| c.contains("never delivered")),
            "a steer reached the model through an inbox that was never lent"
        );
    }
    assert_eq!(idle.pending(), 1, "it stayed queued, undelivered");
}

#[test]
fn responses_keeps_exact_active_prompt_at_user_priority() {
    let exact = "operator text must remain user data";
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "base policy"}),
        serde_json::json!({"role": "user", "content": "historical ask"}),
    ];
    prompt_read::ensure_active_prompt_card(
        &mut messages,
        prompt_read::PromptReadContext::new(None, exact, None),
        None,
    );

    let (instructions, input) = crate::responses_wire::build_responses_input(&messages);
    let instructions = instructions.expect("base and metadata instructions");
    assert!(instructions.contains(prompt_read::ACTIVE_PROMPT_PREFIX));
    assert!(
        !instructions.contains(exact),
        "operator content must not be promoted to Responses instructions"
    );
    assert!(input
        .iter()
        .any(|item| { item["role"] == "user" && item["content"].as_str() == Some(exact) }));
}

#[test]
fn tools_flatten_to_responses_shape() {
    let chat = serde_json::json!([{
        "type": "function",
        "function": {
            "name": "git",
            "description": "run git",
            "parameters": {"type": "object"}
        }
    }]);
    let out = tools_to_responses(&chat);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "function");
    assert_eq!(
        out[0]["name"], "git",
        "name hoisted out of the function wrapper"
    );
    assert_eq!(out[0]["description"], "run git");
    assert!(out[0]["function"].is_null(), "no nested function wrapper");
    // A non-strict tool stays non-strict — no strictness is invented.
    assert!(
        out[0].get("strict").is_none(),
        "absent strict must not become present"
    );
}

#[test]
fn tools_to_responses_preserves_strictness_semantics() {
    // #1526 (invariant #6): a strict Chat Completions schema must stay strict
    // after conversion. `strict` moves from the `function` object to the
    // Responses tool's TOP level, and the parameters' `additionalProperties` /
    // `required` are carried through wholesale (not silently relaxed).
    let chat = serde_json::json!([{
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "write a file",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }
        }
    }]);
    let out = tools_to_responses(&chat);
    assert_eq!(out.len(), 1);
    assert_eq!(
        out[0]["strict"], true,
        "strict must survive at the Responses tool's top level"
    );
    // Validation-semantic fields inside `parameters` are unchanged.
    assert_eq!(
        out[0]["parameters"]["required"],
        serde_json::json!(["path"])
    );
    assert_eq!(out[0]["parameters"]["additionalProperties"], false);
}

#[test]
fn cognition_maps_to_the_responses_reasoning_field_or_is_omitted() {
    use crate::role_profile::Cognition;
    // Opt-in: each level projects to the Responses `reasoning.effort` value.
    assert_eq!(
        responses_reasoning_field(Some(Cognition::Contemplating)),
        Some(serde_json::json!({ "effort": "high" }))
    );
    assert_eq!(
        responses_reasoning_field(Some(Cognition::Glancing)),
        Some(serde_json::json!({ "effort": "minimal" }))
    );
    assert_eq!(
        responses_reasoning_field(Some(Cognition::Deliberating)),
        Some(serde_json::json!({ "effort": "medium" }))
    );
    // Not opted in → the field is omitted entirely (request unchanged).
    assert_eq!(responses_reasoning_field(None), None);
}

#[test]
fn responses_loop_consumes_the_shared_decoder_for_text_calls_and_usage() {
    // The agentic loop now shares ONE decoder with the inference transport
    // (`crate::responses_wire`). This grounds that the loop's consumption
    // path gets text, calls, echo (reasoning + function_call in order), and
    // usage from that single decoder — no second hand-rolled parser.
    let json = serde_json::json!({
        "status": "completed",
        "output": [
            {"type": "reasoning", "summary": "…"},
            {"type": "message", "role": "assistant",
             "content": [{"type": "output_text", "text": "the answer"}]},
            {"type": "function_call", "call_id": "call_1", "name": "git",
             "arguments": "{\"op\":\"status\"}"}
        ],
        "usage": {"input_tokens": 100, "output_tokens": 20}
    });
    let d = crate::responses_wire::decode_response(&json).expect("a completed tool-call turn");
    assert_eq!(d.text, "the answer");
    assert_eq!(d.tool_calls.len(), 1);
    assert_eq!(d.tool_calls[0]["call_id"], "call_1");
    // The echo re-sends the reasoning item AND the function_call in output
    // order, so a reasoning model (gpt-5.6-sol) does not 400 on the follow-up
    // turn for a function_call missing its required reasoning item.
    assert_eq!(d.echo.len(), 2, "reasoning + function_call are echoed");
    assert_eq!(d.echo[0]["type"], "reasoning");
    assert_eq!(d.echo[1]["type"], "function_call");
    assert_eq!(d.echo[1]["call_id"], "call_1");
    let usage = d.usage.unwrap();
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 20);
}

fn giant_prompt_messages(task: &str) -> Vec<MemMessage> {
    vec![MemMessage::system("base policy"), MemMessage::user(task)]
}

fn mid_sized_pair_task(label: &str) -> String {
    format!("{label} {}", "x".repeat(6_000))
}

#[test]
fn accepted_prompt_cannot_raise_budget_past_declared_ceiling() {
    assert_eq!(capped_accepted_prompt_tokens(61_221, Some(54_394)), 54_394);
    assert_eq!(capped_accepted_prompt_tokens(8_734, None), 8_734);
}

#[test]
fn authoritative_zero_input_budget_is_not_erased() {
    assert_eq!(authoritative_request_budget(Some(0), true, None), Some(0));
    assert_eq!(authoritative_request_budget(Some(0), false, None), None);
}

#[test]
fn accepted_prompt_proof_suppresses_count_fallback_only_while_it_covers_current() {
    assert!(count_guard_has_headroom(16_261, None, Some(23_799)));
    assert!(!count_guard_has_headroom(24_000, None, Some(23_799)));
    assert!(count_guard_has_headroom(24_000, Some(800_000), None));
}

/// Prove the regression fixture isolates the live-tail duplicate — the
/// protected recovery copy and schemas fit, but the irreducible complete
/// request (recovery copy + newest user presentation) does not — and
/// RETURN the `safe_context` budget the run should use.
///
/// The budget is DERIVED from the live catalog, not pinned: both `one_copy`
/// (protected head + advertised schemas) and `complete` (+ the duplicated
/// live-tail presentation) already track the catalog, and the gap between
/// them is one catalog-independent ~1.5k-token task copy. Sizing the budget
/// at their midpoint keeps `one_copy <= budget < complete` under any catalog
/// growth, so the fixture always exercises "one copy fits, the irreducible
/// pair does not → refuse". Returning it makes the guard below and the
/// actual `chat_complete` run agree on the same number.
fn mid_sized_pair_budget(task: &str, responses_wire: bool) -> usize {
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": "base policy"}),
        serde_json::json!({"role": "user", "content": task}),
    ];
    prompt_read::ensure_active_prompt_card(
        &mut messages,
        prompt_read::PromptReadContext::new(None, task, None),
        None,
    );
    let head = protected_prompt_head_len(&messages, prompt_read::ACTIVE_PROMPT_PREFIX);
    let chat_tools = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    let tools = if responses_wire {
        serde_json::Value::Array(tools_to_responses(&chat_tools))
    } else {
        chat_tools
    };
    let estimation = crate::tokens::TokenEstimation::default();
    let one_copy = estimate_request_tokens(&messages[..head], Some(&tools), estimation);
    let complete = estimate_request_tokens(&messages, Some(&tools), estimation);
    // Strictly between one_copy and complete (their gap is the ~1.5k-token
    // live-tail task copy), so one protected copy fits but the pair cannot.
    let budget = (one_copy + complete) / 2;
    assert!(
        one_copy <= budget,
        "fixture invalid: one protected copy needs {one_copy} tokens, budget {budget}"
    );
    assert!(
        complete > budget,
        "fixture invalid: the irreducible pair needs {complete} tokens, budget {budget}"
    );
    budget
}

fn assert_irreducible_refusal(error: &anyhow::Error) {
    let message = error.to_string();
    assert!(
        message.contains("refusing before inference dispatch"),
        "{message}"
    );
    assert!(
        message.contains("operator prompt was not truncated"),
        "{message}"
    );
}

#[tokio::test]
async fn ollama_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
    let server = MockServer::start().await;
    let task = format!("OLLAMA-GIANT {}", "x".repeat(20_000));
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let error = chat_complete(
        hard_budget_ctx(
            &server.uri(),
            &messages,
            &caveats,
            &task,
            BackendKind::Ollama,
        ),
        &mut NoMcp,
    )
    .await
    .expect_err("giant exact prompt is irreducible");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn openai_chat_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
    let server = MockServer::start().await;
    let task = format!("OPENAI-CHAT-GIANT {}", "x".repeat(20_000));
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let error = openai_chat_complete(
        hard_budget_ctx(
            &server.uri(),
            &messages,
            &caveats,
            &task,
            BackendKind::Openai,
        ),
        &mut NoMcp,
    )
    .await
    .expect_err("giant exact prompt is irreducible");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn responses_giant_exact_prompt_refuses_before_zero_wire_dispatches() {
    let server = MockServer::start().await;
    let task = format!("RESPONSES-GIANT {}", "x".repeat(20_000));
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let error = openai_responses_complete(
        hard_budget_ctx(
            &server.uri(),
            &messages,
            &caveats,
            &task,
            BackendKind::Openai,
        ),
        &mut NoMcp,
    )
    .await
    .expect_err("giant exact prompt is irreducible");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn ollama_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("OLLAMA-MID-PAIR");
    let budget = mid_sized_pair_budget(&task, false);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Ollama);
    ctx.safe_context = Some(budget as u32);
    let error = chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the two irreducible prompt presentations exceed the window");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn openai_chat_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("OPENAI-CHAT-MID-PAIR");
    let budget = mid_sized_pair_budget(&task, false);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = Some(budget as u32);
    let error = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the two irreducible prompt presentations exceed the window");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn openai_chat_declared_num_ctx_is_a_local_refusal_budget() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("OPENAI-CHAT-NUM-CTX");
    let budget = mid_sized_pair_budget(&task, false);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.num_ctx = Some(((budget * 100).div_ceil(80)) as u32);

    let error = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the declared local window must refuse the irreducible request");

    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn openai_chat_output_reserve_tightens_declared_window_before_dispatch() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("OPENAI-CHAT-OUTPUT-RESERVE");
    let budget = mid_sized_pair_budget(&task, false);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.cognition = Some(crate::role_profile::Cognition::Contemplating);
    ctx.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        ..Default::default()
    };
    let context_window = budget + 16_000;
    assert!(
        context_window * ctx.input_ceiling_pct as usize / 100 > budget,
        "fixture must be tightened by output reserve, not percentage"
    );
    ctx.num_ctx = Some(context_window as u32);

    let error = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the 16K output reserve must refuse the irreducible input");

    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn responses_mid_sized_irreducible_prompt_pair_refuses_before_dispatch() {
    let server = MockServer::start().await;
    let task = mid_sized_pair_task("RESPONSES-MID-PAIR");
    let budget = mid_sized_pair_budget(&task, true);
    let messages = giant_prompt_messages(&task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = Some(budget as u32);
    let error = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the two irreducible prompt presentations exceed the window");
    assert_irreducible_refusal(&error);
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn responses_never_sends_num_ctx_on_the_wire() {
    // A configured window is a LOCAL limit (see the refusal test below), but
    // it must NEVER be sent on the Responses wire (limits are provider-side).
    // Here the window is large enough to fit the small request, so it
    // succeeds AND the body carries no `num_ctx`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "provider accepted"}]
            }],
            "usage": {"input_tokens": 20, "output_tokens": 3}
        })))
        .mount(&server)
        .await;

    let task = "a normal Responses request";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    // A generous configured window: a local ceiling, but the small request
    // fits well under it, so nothing is refused.
    ctx.num_ctx = Some(1_000_000);

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("the request fits the configured window");
    assert_eq!(reply, "provider accepted");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        body.get("num_ctx").is_none(),
        "Responses must not send the ChatCtx num_ctx display hint"
    );
    assert!(
        body.get("reasoning").is_none(),
        "no cognition set → no reasoning.effort on the wire (request unchanged)"
    );
}

#[tokio::test]
async fn responses_request_sets_store_false() {
    // BHV-STORAGE-001: the AGENTIC-loop Responses request explicitly opts out
    // of server-side retention (`store: false`), not by inheriting the API's
    // `store: true` default. A dedicated, correctly-scoped assertion (the
    // storage contract must not lean on an unrelated num_ctx test).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "ok"}]}]
        })))
        .mount(&server)
        .await;

    let task = "store policy";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = Some(1_000_000); // fits → the request dispatches
    openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("request succeeds");

    let requests = server.received_requests().await.expect("journal");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body.get("store"),
        Some(&serde_json::Value::Bool(false)),
        "the agentic Responses request must set store:false explicitly"
    );
}

#[tokio::test]
async fn responses_refuses_locally_when_a_configured_window_cannot_fit() {
    // #1526 (invariant #4): a CONFIGURED context window is a local safety
    // limit even though it is never sent on the Responses wire. A window too
    // small to hold the irreducible request must be refused PRE-DISPATCH —
    // no request reaches the provider — rather than relying on a reactive
    // 400 or a silent truncation. (The previous contract wrongly let this
    // sail through; that assertion is now reversed.)
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0) // nothing may be dispatched
        .mount(&server)
        .await;

    let task = "a normal Responses request";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    // A 1-token window leaves zero input capacity → local refusal.
    ctx.num_ctx = Some(1);

    openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a 1-token configured window cannot fit the request");
    assert_no_requests(&server).await;
}

#[tokio::test]
async fn dispatch_responses_json_retries_transient_transport_failures() {
    // R5: the ONE shared Responses dispatch (used by BOTH the per-round loop
    // and the final tools-disabled summary) retries a transient status. A
    // persistent 503 exhausts the retries — the mock's `.expect(3)` (initial
    // + 2 retries) proves the summary path is no longer a bare, un-retried
    // `send()` that a transient blip could discard after all rounds were spent.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(503)) // retryable transport failure
        .expect(3)
        .mount(&server)
        .await;

    let retry = crate::retry::RetryPolicy {
        max_retries: 2,
        base: std::time::Duration::from_millis(0),
        max: std::time::Duration::from_millis(0),
        jitter: false,
    };
    let client = reqwest::Client::new();
    let url = format!("{}/v1/responses", server.uri());
    // B5: the dispatcher accepts only a ValidatedResponsesRequest. This test
    // exercises transport backoff, not the wire invariants, so it uses the
    // test-only constructor to stand up a validated body directly.
    let validated = responses_wire_validation::ValidatedResponsesRequest::from_body_for_test(
        serde_json::json!({"model": "m", "input": []}),
    );
    let err = super::dispatch_responses_json(&client, &url, None, &validated, &retry, false)
        .await
        .expect_err("a persistent 503 exhausts retries");
    assert_eq!(
        err.to_string(),
        "inference endpoint 503 Service Unavailable: "
    );
    assert_eq!(
        err.downcast_ref::<observability::DispatchError>()
            .unwrap()
            .class,
        observability::ErrorClass::Model
    );
    // `.expect(3)` is verified on server drop — it retried, not sent once.
}

/// Build a history (~36k estimated tokens) far larger than the recovered
/// cw-400 budget, so the recovery's compaction is forced to FIRE (not merely
/// fit) even after the always-advertised tool schemas (~5k tokens) claim their
/// share of the recovered 40k-token window.
fn overflowing_responses_history(task: &str) -> Vec<MemMessage> {
    let mut messages = vec![MemMessage::system("base policy")];
    for i in 0..60 {
        messages.push(MemMessage::user(format!(
            "historical step {i} {}",
            "x".repeat(1_200)
        )));
        messages.push(MemMessage::assistant(format!(
            "did step {i} {}",
            "y".repeat(1_200)
        )));
    }
    messages.push(MemMessage::user(task));
    messages
}

#[tokio::test]
async fn responses_recovers_from_a_context_window_400_by_compacting_and_redispatching() {
    // #1528: a hard context-window 400 on the Responses wire must be
    // RECOVERED — learn the true window, tighten the input ceiling, compact
    // the running history to fit, and re-dispatch — never surfaced as a raw
    // 400. Regression: before this slice the Responses loop returned the 400
    // directly (no compaction, no redispatch).
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct OverflowThenOk {
        served: Arc<AtomicUsize>,
    }
    impl Respond for OverflowThenOk {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.served.fetch_add(1, Ordering::SeqCst) == 0 {
                // A numbered LiteLLM/vLLM overflow cw_overflow recognizes.
                ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "recovered and done"}]}]
                }))
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(OverflowThenOk {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "RECOVER: keep going after the window shrinks";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    // Cloud default: no local window → the first (over-window) request
    // dispatches and the provider's 400 drives recovery.
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    ctx.max_tool_rounds = 4;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("cw-400 recovery compacts and redispatches to success");
    assert_eq!(reply, "recovered and done");
    assert_eq!(
        served.load(Ordering::SeqCst),
        2,
        "exactly one 400 then one recovered 200 — the raw 400 never surfaced"
    );
}

#[tokio::test]
async fn responses_context_window_400_recovery_is_bounded() {
    // #1528: recovery is capped at 2 retries. A server that 400s every time
    // must ultimately surface the error after at most 1 + 2 dispatches, never
    // looping forever.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
        })))
        .expect(3) // initial + exactly 2 bounded recoveries
        .mount(&server)
        .await;

    let task = "BOUNDED: never loop forever on a persistent 400";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    // max_tool_rounds == 1 so the bound proven is the INNER `cw_retries` cap
    // (recovery retries in place), not the outer round cap: a single logical
    // round still dispatches at most initial + 2 recoveries before surfacing.
    ctx.max_tool_rounds = 1;

    openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a persistent cw-400 surfaces after the bounded retries");
    // `.expect(3)` verified on drop: initial + exactly 2 recoveries.
}

#[test]
fn responses_cw_recovery_ceiling_is_monotone_non_increasing() {
    // #1528: each recovery composes the freshly-learned window with the
    // previously-tightened ceiling via `min` (the same `recovered_input_budget`
    // declared-ceiling composition the recovery branch uses), so the effective
    // input ceiling can only shrink — never rebound — across successive 400s.
    let pct = 80;
    // First 400 learns a 6000-token window → 4800 input ceiling.
    let first = recovered_input_budget(6000, pct, None, None);
    assert_eq!(first, 4800);
    // A later 400 reporting a LARGER window must NOT raise the ceiling: the
    // retained tighter ceiling wins.
    let second = recovered_input_budget(100_000, pct, None, Some(first));
    assert!(second <= first, "ceiling rose: {second} > {first}");
    assert_eq!(second, first);
    // A later 400 reporting a SMALLER window tightens further.
    let third = recovered_input_budget(2_000, pct, None, Some(second));
    assert!(
        third < second,
        "ceiling failed to tighten: {third} !< {second}"
    );
}

#[tokio::test]
async fn responses_cw_400_recovery_retries_the_same_logical_round_with_tools() {
    // #1528 (review P1): a cw-400 must retry the SAME logical tool round in
    // place, not advance the round counter. With max_tool_rounds == 1 the buggy
    // loop consumed the only round on recovery and demoted the recovered request
    // to the tools-disabled summary — 2 requests: [400, summary]. The fix
    // dispatches a real recovered TOOL round (still carrying tools); only a
    // COMPLETED round then advances to the summary — 3 requests:
    // [400, recovered tool round, summary].
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct OverflowThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for OverflowThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
                })),
                // The recovered request: a real tool round (get_context_remaining
                // is executed synthetically in-loop — no external side effect).
                1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "function_call", "name": "get_context_remaining",
                        "arguments": "{}", "call_id": "c1"}]
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "done"}]}]
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(OverflowThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "SAME ROUND: recovery must not burn the only tool round";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("recovery retries the round in place and the turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [400, recovered tool round, summary]; a 2-request run means \
             recovery burned the only round and demoted to the tools-disabled summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(1)["tools"].is_array(),
        "the RECOVERED request must still carry tools — a real tool round, not the summary"
    );
    assert!(
        body(2)["tools"].is_null(),
        "only the final summary (after the completed round) is tools-disabled"
    );
}

#[tokio::test]
async fn responses_cw_400_on_the_final_round_recovers_in_place() {
    // #1528 (review P1): a cw-400 on the LAST logical round retries THAT round
    // rather than jumping to the summary. max_tool_rounds == 2: round 0 completes
    // a tool call; round 1 400s, recovers IN PLACE and does another tool call,
    // then the completed round advances to the summary — 4 requests. The buggy
    // loop would `continue` past round 1 straight into the summary — 3 requests.
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct ToolThen400ThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for ToolThen400ThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let tool = serde_json::json!({
                "model": "mock",
                "output": [{"type": "function_call", "name": "get_context_remaining",
                    "arguments": "{}", "call_id": "c1"}]
            });
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(200).set_body_json(tool),
                1 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
                })),
                2 => ResponseTemplate::new(200).set_body_json(tool),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "finished"}]}]
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ToolThen400ThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "FINAL ROUND: a 400 on the last round retries it, not the summary";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    ctx.max_tool_rounds = 2;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("the final round recovers in place and completes");
    assert_eq!(reply, "finished");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        4,
        "expected round0 tool, round1 400, round1 recovered tool, summary; a \
             3-request run means the 400 burned round 1 and jumped to the summary"
    );
}

#[tokio::test]
async fn responses_proactively_compacts_before_the_first_dispatch() {
    // #1528 B3 (req 1/2): when the FIRST request is LOCALLY known to exceed the
    // input budget, the loop compacts BEFORE dispatching — no round-trip to learn
    // it from a cw-400. The single request the server sees is already compacted
    // (carries the reference-summary envelope) and STILL carries tools (a real
    // tool round — the round counter is not consumed). Regression: before B3 the
    // Responses loop only compacted REACTIVELY, after a provider 400 (so this
    // request would have been dispatched over budget, or 400'd first).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "proactively compacted"}]}]
        })))
        .mount(&server)
        .await;

    let task = "PROACTIVE: compact before the first dispatch";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    // A LOCAL budget (no cw-400 needed): the raw history dwarfs it, the compacted
    // form fits. recover_cw_400 stays None — proactive never learns from a 400.
    ctx.safe_context = Some(12_000);
    ctx.num_ctx = Some(12_000);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("proactive compaction fits the request and it dispatches once");
    assert_eq!(reply, "proactively compacted");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        1,
        "a single dispatch — proactively compacted, no cw-400 round-trip"
    );
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap_or_default();
    assert!(
        body["input"]
            .to_string()
            .contains("newt-compaction-summary"),
        "the first request was compacted BEFORE dispatch (reference-summary envelope present)"
    );
    assert!(
        body["tools"].is_array(),
        "the compacted request is still a REAL tool round — the round is not consumed"
    );
    let n_items = body["input"].as_array().map_or(0, Vec::len);
    assert!(n_items < 30, "compacted to {n_items} items (raw was ~121)");
}

#[tokio::test]
async fn responses_irreducible_request_refuses_before_the_proactive_guard() {
    // The B3 proactive guard sits BEHIND the pre-loop irreducible preflight: a
    // request whose protected head + newest live user alone dwarf the budget can
    // never be helped by compaction (the newest user is protected), so it is
    // refused BEFORE the round loop — the guard never runs. This pins that
    // fail-closed path to its DISTINGUISHING pre-loop message ("the operator
    // prompt was not truncated"), distinct from the in-loop preflight's "function
    // outputs were not truncated". (Honesty note per adversarial review: this
    // exercises the pre-existing irreducible gate, NOT B3's proactive path — B3's
    // guard is best-effort and DELEGATES the refusal to the authoritative
    // preflight, so there is no B3-specific fail-closed behavior to assert here.
    // B3's own new behavior — proactive compaction that lets an over-budget
    // request SUCCEED — is covered by responses_proactively_compacts_* /
    // responses_final_summary_is_proactively_compacted.)
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "UNREACHED"}]}]
        })))
        .expect(0)
        .mount(&server)
        .await;

    let task = format!("IRREDUCIBLE {}", "z".repeat(40_000));
    let messages = vec![MemMessage::system("base policy"), MemMessage::user(&task)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = Some(512);
    ctx.num_ctx = Some(512);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 1;

    let err = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("an irreducible over-budget request fails closed, never dispatched");
    let msg = err.to_string();
    assert!(
        msg.contains("operator prompt was not truncated"),
        "expected the PRE-LOOP irreducible refusal (distinct from the in-loop \
             preflight message), got: {msg}"
    );
    // `.expect(0)` verified on MockServer drop: ZERO inference dispatches.
}

#[tokio::test]
async fn responses_final_summary_is_proactively_compacted() {
    // #1528 B3 (req 6): the tools-DISABLED final summary is proactively compacted
    // when it is locally over budget, before its dispatch. `max_tool_rounds == 0`
    // skips the tool rounds so this exercises ONLY the final-summary guard.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "summarized"}]}]
        })))
        .mount(&server)
        .await;

    let task = "FINAL SUMMARY: compact the tools-disabled summary too";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = Some(12_000);
    ctx.num_ctx = Some(12_000);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 0;
    let mut end_reason = None;
    ctx.end_reason = Some(&mut end_reason);

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("the tools-disabled summary is proactively compacted and dispatches");
    assert_eq!(reply, "summarized");
    assert_eq!(end_reason, Some(crate::TurnEndReason::RoundCap));

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(reqs.len(), 1, "only the final summary dispatched");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap_or_default();
    assert!(
        body["tools"].is_null(),
        "the final summary is tools-disabled"
    );
    assert!(
        body["input"]
            .to_string()
            .contains("newt-compaction-summary"),
        "the tools-disabled summary was proactively compacted before dispatch"
    );
    assert!(
        body["input"].to_string().contains("progress update"),
        "Responses receives the same resumable cap handoff as chat backends"
    );
}

#[tokio::test]
async fn responses_unusable_cap_summary_returns_a_round_cap_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "failed"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let task = "report progress at the cap";
    let messages = vec![MemMessage::system("base policy"), MemMessage::user(task)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.num_ctx = None;
    ctx.max_tool_rounds = 0;
    let mut end_reason = None;
    ctx.end_reason = Some(&mut end_reason);

    let (reply, streamed, usage, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("an unusable final summary becomes an honest paused fallback");
    assert!(!streamed);
    assert_eq!(usage, None);
    assert!(reply.contains("Paused at the tool-round limit"), "{reply}");
    assert!(
        reply.contains("final summarization request also failed"),
        "{reply}"
    );
    assert_eq!(end_reason, Some(crate::TurnEndReason::RoundCap));
}

// --- #1528 B3 transactional helper unit tests (B3-CG-004/005) ---

fn assistant(i: usize, chars: usize) -> serde_json::Value {
    serde_json::json!({"role": "assistant", "content": format!("step {i}: {}", "w ".repeat(chars))})
}

/// B3-CG-004: a candidate compaction REJECTED by the post-bridge budget guard
/// commits NOTHING — the live `input` and `CompressState` are untouched and no
/// committed notice is emitted (the helper returns before the commit block). The
/// typed `OverBudgetAfterFence` carries the reason for the caller's error chain.
/// Fails on `711c247` (non-transactional: notice + state mutated before the check).
#[tokio::test]
async fn compact_responses_input_post_fence_overflow_is_transactional() {
    use crate::agentic::compress::{CompressState, Summarizer};
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let summ: Summarizer = Box::new(move |_r: String| {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok("a short summary".to_string()) })
    });
    let mut input = vec![serde_json::json!({"role": "user", "content": "the task"})];
    for i in 0..6 {
        input.push(assistant(i, 300));
    }
    input.push(serde_json::json!({"role": "user", "content": "recent turn"}));
    let original = input.clone();
    let mut state = CompressState::new();

    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(10), // actionable_budget: tiny → the fenced rebuild overflows it
        400,      // compaction_budget: generous enough that compress FIRES
        1.0,
        crate::tokens::TokenEstimation::default(),
        "the task",
        8_192,
        true,
        None,
        Some(&*summ),
        &mut state,
        false,
    )
    .await;

    assert!(
        matches!(outcome, ResponsesCompaction::OverBudgetAfterFence(_)),
        "expected a post-fence overflow rejection"
    );
    assert_eq!(
        input, original,
        "TRANSACTIONAL: a rejected candidate leaves input UNCHANGED"
    );
    assert_eq!(
        state.counters().compressions,
        0,
        "TRANSACTIONAL: live CompressState attempts UNCHANGED (compaction ran on a clone)"
    );
    assert!(
        !state.is_disabled(),
        "TRANSACTIONAL: disabled latch UNCHANGED"
    );
    assert!(
        calls.load(std::sync::atomic::Ordering::SeqCst) >= 1,
        "the candidate compaction actually ran (summarizer invoked)"
    );
}

/// B3-CG-004: a forbidden `system` item fails classification (BridgeError) with no
/// compaction and no side effects.
#[tokio::test]
async fn compact_responses_input_bridge_error_is_transactional() {
    use crate::agentic::compress::CompressState;
    let mut input = vec![
        serde_json::json!({"role": "user", "content": "task"}),
        serde_json::json!({"role": "system", "content": "smuggled"}),
    ];
    let original = input.clone();
    let mut state = CompressState::new();
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(5),
        5,
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        None,
        None,
        &mut state,
        false,
    )
    .await;
    assert!(matches!(outcome, ResponsesCompaction::BridgeError));
    assert_eq!(
        input, original,
        "transactional: input unchanged on bridge error"
    );
    assert_eq!(state.counters().compressions, 0, "no compaction ran");
}

/// B3-CG-004: a compressor refusal (protected head alone exceeds the target)
/// commits nothing.
#[tokio::test]
async fn compact_responses_input_refusal_is_transactional() {
    use crate::agentic::compress::CompressState;
    let mut input = vec![serde_json::json!({"role": "user", "content": "x".repeat(4_000)})];
    for i in 0..3 {
        input.push(assistant(i, 10));
    }
    let original = input.clone();
    let mut state = CompressState::new();
    // compaction_budget = 1 → the protected head alone exceeds it → refuse.
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt with a large protected head that cannot shrink"),
        None,
        Some(1),
        1,
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        None,
        None,
        &mut state,
        false,
    )
    .await;
    assert!(
        matches!(
            outcome,
            ResponsesCompaction::Refused | ResponsesCompaction::NotFired
        ),
        "an irreducible tiny budget refuses / makes no progress"
    );
    assert_eq!(input, original, "transactional: input unchanged on refusal");
    assert_eq!(
        state.counters().compressions,
        0,
        "transactional: state unchanged"
    );
}

/// B3-CG-004: a candidate that fits COMMITS all three effects — input rewritten
/// to the compacted form, the anti-thrash attempt recorded, and (structurally,
/// after this point) the notice emitted.
#[tokio::test]
async fn compact_responses_input_commits_only_on_success() {
    use crate::agentic::compress::{CompressState, Summarizer};
    let summ: Summarizer =
        Box::new(|_r: String| Box::pin(async { Ok("brief summary".to_string()) }));
    let mut input = vec![serde_json::json!({"role": "user", "content": "task"})];
    for i in 0..6 {
        input.push(assistant(i, 300));
    }
    input.push(serde_json::json!({"role": "user", "content": "recent turn"}));
    let before_len = input.len();
    let mut state = CompressState::new();
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(100_000), // generous actionable budget → the compacted form fits
        400,           // tight compaction target → compress fires
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        None,
        Some(&*summ),
        &mut state,
        false,
    )
    .await;
    assert!(
        matches!(outcome, ResponsesCompaction::Compacted),
        "a fitting compaction commits"
    );
    assert!(
        input.len() < before_len,
        "committed: input rewritten to fewer items ({} < {before_len})",
        input.len()
    );
    assert!(
        input.iter().any(|m| m["content"]
            .as_str()
            .unwrap_or("")
            .contains("newt-compaction-summary")),
        "committed: the reference-summary envelope is present"
    );
    assert_eq!(
        state.counters().compressions,
        1,
        "committed: the anti-thrash attempt IS recorded on the live state"
    );
}

/// B3-CG-004 / §2.6: the FOURTH effect — the session compaction/spill store — is
/// also transactional. A rejected candidate writes NOTHING to the live store
/// (rejected-candidate-publishes-nothing); a committed one flushes exactly its
/// staged span. Fails on `711c247`, where the shared `store.store(...)` inside
/// `compress` was not rolled back on reject (leaking an orphaned redacted span per
/// rejected proactive attempt).
#[tokio::test]
async fn compact_responses_input_spill_store_is_transactional() {
    use crate::agentic::compress::{CompressState, Summarizer};
    use crate::agentic::content_spill::{SessionSpillStore, SpillStore};
    let make_input = || {
        let mut v = vec![serde_json::json!({"role": "user", "content": "task"})];
        for i in 0..6 {
            v.push(assistant(i, 300));
        }
        v.push(serde_json::json!({"role": "user", "content": "recent turn"}));
        v
    };
    let summarizer =
        || -> Summarizer { Box::new(|_r: String| Box::pin(async { Ok("brief".to_string()) })) };

    // REJECT (tiny actionable budget → post-fence overflow): store stays EMPTY.
    {
        let store = SessionSpillStore::new([7u8; 16]);
        let s = summarizer();
        let mut input = make_input();
        let mut state = CompressState::new();
        let outcome = compact_responses_input(
            &mut input,
            Some("you are newt"),
            None,
            Some(10),
            400,
            1.0,
            crate::tokens::TokenEstimation::default(),
            "task",
            8_192,
            true,
            Some(&store),
            Some(&*s),
            &mut state,
            false,
        )
        .await;
        assert!(matches!(
            outcome,
            ResponsesCompaction::OverBudgetAfterFence(_)
        ));
        assert_eq!(
            store.unique_objects(),
            0,
            "TRANSACTIONAL: a rejected candidate writes NO committed spill"
        );
        assert_eq!(
            store.logical_spill_refs(),
            0,
            "a rejected candidate installs no logical reference either"
        );
        assert_eq!(input, make_input(), "live input is UNCHANGED on reject");
        assert!(
            !serde_json::to_string(&input)
                .unwrap()
                .contains("compaction:"),
            "no retrieval marker leaked into live input"
        );
    }
    // COMMIT (generous budget): the staged span is flushed exactly once.
    {
        let store = SessionSpillStore::new([7u8; 16]);
        let s = summarizer();
        let mut input = make_input();
        let mut state = CompressState::new();
        let outcome = compact_responses_input(
            &mut input,
            Some("you are newt"),
            None,
            Some(100_000),
            400,
            1.0,
            crate::tokens::TokenEstimation::default(),
            "task",
            8_192,
            true,
            Some(&store),
            Some(&*s),
            &mut state,
            false,
        )
        .await;
        assert!(matches!(outcome, ResponsesCompaction::Compacted));
        assert_eq!(
            store.unique_objects(),
            1,
            "committed: the compacted span is flushed to the store exactly once"
        );
        // The live input names the committed span's `compaction:<cid>` handle.
        assert!(
            serde_json::to_string(&input)
                .unwrap()
                .contains("compaction:"),
            "the committed candidate names its retrieval handle"
        );
    }
}

fn spill_middle_input() -> Vec<serde_json::Value> {
    let mut v = vec![serde_json::json!({"role": "user", "content": "task"})];
    for i in 0..6 {
        v.push(assistant(i, 300));
    }
    v.push(serde_json::json!({"role": "user", "content": "recent turn"}));
    v
}

/// Correction 1: with NO real compaction store, a successful compaction still
/// summarizes but promises NO retrieval — no `memory_fetch("compaction:...")`
/// handle. Fails on `8b3a1c8`, which wrapped a `None` store and invented
/// `compaction:s0` (a phantom, unresolvable handle).
#[tokio::test]
async fn compact_responses_input_no_store_emits_no_retrieval_handle() {
    use crate::agentic::compress::Summarizer;
    let summ: Summarizer = Box::new(|_r: String| Box::pin(async { Ok("brief".to_string()) }));
    let mut input = spill_middle_input();
    let mut state = crate::agentic::compress::CompressState::new();
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(100_000),
        400,
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        None, // NO real compaction store
        Some(&*summ),
        &mut state,
        false,
    )
    .await;
    assert!(matches!(outcome, ResponsesCompaction::Compacted));
    let text = serde_json::to_string(&input).unwrap();
    assert!(
        text.contains("newt-compaction-summary"),
        "it still compacted"
    );
    assert!(
        !text.contains("compaction:"),
        "no retrieval handle promised without a store: {text:.200}"
    );
}

/// §2.6 (replaces the obsolete "store-issued id" correction): a committed
/// compaction names a `compaction:<cid>` CONTENT handle — not a predicted or
/// allocated id — and that handle parses as a canonical CID AND resolves in the
/// live store to the committed verbatim span. Content addressing dissolved the
/// allocator, so there is no id to predict or steal.
#[tokio::test]
async fn compact_responses_input_names_a_resolvable_content_handle() {
    use crate::agentic::compress::Summarizer;
    use crate::agentic::content_spill::{SessionSpillStore, SpillCid, SpillStore};
    let store = SessionSpillStore::new([7u8; 16]);
    let summ: Summarizer = Box::new(|_r: String| Box::pin(async { Ok("brief".to_string()) }));
    let mut input = spill_middle_input();
    let mut state = crate::agentic::compress::CompressState::new();
    let outcome = compact_responses_input(
        &mut input,
        Some("you are newt"),
        None,
        Some(100_000),
        400,
        1.0,
        crate::tokens::TokenEstimation::default(),
        "task",
        8_192,
        true,
        Some(&store),
        Some(&*summ),
        &mut state,
        false,
    )
    .await;
    assert!(matches!(outcome, ResponsesCompaction::Compacted));
    let text = serde_json::to_string(&input).unwrap();
    // Extract the `compaction:<cid>` handle (a base32-lower CID — ascii
    // alphanumeric, so read up to the first non-alphanumeric terminator).
    let handle: String = text
        .split("compaction:")
        .nth(1)
        .expect("the marker names a compaction handle")
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let cid = SpillCid::parse(&handle).expect("the handle is a canonical content CID");
    assert!(!text.contains("compaction:s0"), "no predicted s0 marker");
    assert!(
        store
            .fetch(&cid)
            .is_some_and(|r| r.redacted_text.contains("step 0")),
        "the emitted handle resolves to the committed verbatim span"
    );
    assert_eq!(store.unique_objects(), 1);
}

// --- #1528 B3 (item 6): pure lifecycle-invariant property tests ---

/// A pure, side-effect-free MODEL of one proactive pre-dispatch decision, mirroring
/// the guard + transactional `compact_responses_input` + the authoritative preflight
/// at the estimate level. `committed_post_estimate` is `Some(fenced estimate)` IFF
/// the transactional helper committed a candidate (which it does only when the
/// candidate passed `check_post_bridge_budget`, i.e. fits the budget). Corresponds to
/// `formal/CompactionLifecycle` (estimate → compact → validate → readyToDispatch |
/// abort).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProactiveDecision {
    DispatchAsIs,
    DispatchCompacted,
    FailClosed,
}

fn proactive_decision(
    actionable_budget: Option<usize>,
    pre_estimate: usize,
    committed_post_estimate: Option<usize>,
) -> ProactiveDecision {
    let Some(budget) = actionable_budget else {
        return ProactiveDecision::DispatchAsIs; // no budget → no gate
    };
    if pre_estimate <= budget {
        return ProactiveDecision::DispatchAsIs; // already fits → no compaction
    }
    match committed_post_estimate {
        Some(post) if post <= budget => ProactiveDecision::DispatchCompacted,
        _ => ProactiveDecision::FailClosed,
    }
}

/// B3-CG-001/005: over the whole finite decision space, DISPATCH implies the
/// governing estimate is within budget, and a compaction FAILURE implies zero
/// dispatch. Also: no budget → dispatch as-is; a fitting request is never compacted.
#[test]
fn proactive_decision_never_dispatches_over_budget() {
    for budget in [None, Some(0usize), Some(100), Some(1000)] {
        for pre in [0usize, 100, 1000, 5000] {
            for committed in [None, Some(0usize), Some(100), Some(1000), Some(5000)] {
                let d = proactive_decision(budget, pre, committed);
                match d {
                    ProactiveDecision::DispatchAsIs => {
                        // Only when there is no budget, or the request already fits.
                        assert!(
                            budget.is_none() || pre <= budget.unwrap(),
                            "DispatchAsIs must mean no-budget or already-fits: {budget:?} {pre}"
                        );
                    }
                    ProactiveDecision::DispatchCompacted => {
                        let b = budget.expect("compacted dispatch requires a budget");
                        assert!(pre > b, "compaction only runs when over budget");
                        assert!(
                            committed.expect("committed implies Some") <= b,
                            "DISPATCH ⟹ post-fence estimate ≤ actionable budget"
                        );
                    }
                    ProactiveDecision::FailClosed => {
                        // Failure ⟹ zero dispatch: either no committed candidate, or
                        // the committed candidate did not fit (never emitted here).
                        let b = budget.expect("fail-closed only under a budget");
                        assert!(pre > b);
                        assert!(committed.is_none_or(|p| p > b));
                    }
                }
            }
        }
    }
}

/// B3-CG-003: the tools-disabled compaction target NEVER subtracts schema overhead,
/// the tools-enabled one always does, and dropping schemas can only RAISE the target
/// — across a sweep of ceilings / schema sizes / calibrations.
#[test]
fn compaction_target_schema_subtraction_follows_tools() {
    use super::send_budget::ResponsesBudgetState;
    for window in [4_096u32, 32_768, 131_072] {
        for schema in [0usize, 500, 4_000] {
            for cal in [0.5f32, 1.0, 2.0] {
                let mut s = ResponsesBudgetState::new(Some(window), 80, None, None, None, None);
                let recovered = s.recovered_budget_for_window(window);
                s.recover_from_cw400(recovered);
                s.set_tool_schema_tokens(schema);
                let with = s.compaction_budget(cal, true);
                let without = s.compaction_budget(cal, false);
                assert!(
                    without >= with,
                    "dropping schemas never lowers the target: {without} >= {with} \
                         (window={window} schema={schema} cal={cal})"
                );
                if schema == 0 {
                    assert_eq!(with, without, "no schemas → the flag is a no-op");
                }
            }
        }
    }
}

/// #1528 B3 (item 3, B3-CG-005): the IN-LOOP no-progress path — a tool round
/// yields an OVERSIZED fresh result, the next request exceeds the local budget,
/// and the proactive guard invokes the compactor EXACTLY ONCE (a counting
/// summarizer proves it). The protected newest tool output cannot be reduced, so
/// the authoritative preflight refuses: ONE dispatch, ZERO second dispatch, the
/// logical round intact. This FAILS if the proactive helper call is deleted — the
/// summarizer is never invoked on round 1 (`calls == 0`), which the pre-existing
/// preflight-refusal path does not do.
#[tokio::test]
async fn responses_proactive_no_progress_invokes_the_compactor_exactly_once() {
    use crate::agentic::compress::Summarizer;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "model": "mock",
            "output": [{"type": "function_call", "name": "read_file",
                "arguments": "{\"path\":\"huge.txt\"}", "call_id": "c1"}]
        })))
        .mount(&server)
        .await;
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("huge.txt"), "x".repeat(64_000)).unwrap();

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = calls.clone();
    let summ: Summarizer = Box::new(move |_r: String| {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok("brief".to_string()) })
    });

    let task = "read the huge fixture then summarize";
    // A SMALL reducible history — round 0 fits, and it survives as a summarizable
    // MIDDLE on round 1 (after the giant tool output becomes the protected tail).
    let mut messages = vec![MemMessage::system("base policy")];
    for i in 0..4 {
        messages.push(MemMessage::user(format!("earlier ask {i}")));
        messages.push(MemMessage::assistant(format!("earlier reply {i}")));
    }
    messages.push(MemMessage::user(task));
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.workspace = workspace.path().to_str().unwrap();
    ctx.summarizer = Some(&*summ);
    ctx.safe_context = Some(8_000);
    ctx.num_ctx = Some(8_000);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 2;

    let error = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("the fresh giant tool output cannot be compacted → fail closed");
    let chain = format!("{error:#}");
    assert!(
        chain.contains("function outputs were not truncated"),
        "in-loop preflight refusal expected: {chain}"
    );
    assert!(
        chain.contains("proactive compaction"),
        "the B3 compaction reason is attached to the chain: {chain}"
    );
    let requests = server.received_requests().await.expect("request journal");
    assert_eq!(
        requests.len(),
        1,
        "round 0 dispatched; the oversized round 1 never did (zero second dispatch)"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the proactive guard invoked the compactor EXACTLY once on round 1"
    );
}

/// #1528 B3 (B3-CG-003, item 1): after the provider rejects tools, the SAME round
/// retries TOOLS-DISABLED; when that retry is locally over budget the PROACTIVE
/// guard compacts it with a target that does NOT subtract tool-schema overhead
/// (the request sends none). The HTTP journal shows req1 WITH tools, req2 WITHOUT
/// tools and proactively compacted, and the turn completes in the same round.
/// (The exact "no schema subtraction" arithmetic is pinned deterministically by
/// `compaction_target_schema_subtraction_follows_tools` and the budget unit test.)
#[tokio::test]
async fn responses_unsupported_tools_retry_is_proactively_compacted_without_schema_overhead() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct UnsupportedThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UnsupportedThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if self.served.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "this model does not support tools"}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "done tools-disabled"}]}]
                }))
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(UnsupportedThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "summarize the history";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = Some(12_000);
    ctx.num_ctx = Some(12_000);
    ctx.max_ok_input = None;
    ctx.recover_cw_400 = None;
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("tools-disabled retry is proactively compacted and completes in the same round");
    assert_eq!(reply, "done tools-disabled");
    let reqs = server.received_requests().await.expect("request journal");
    assert_eq!(
        reqs.len(),
        2,
        "req1 (tools) rejected, req2 (tools-disabled) retried IN THE SAME round"
    );
    let b1: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap_or_default();
    assert!(b1["tools"].is_array(), "req1 advertised tools");
    let b2: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap_or_default();
    assert!(
        b2["tools"].is_null(),
        "req2 is tools-disabled (the schema overhead it must not reserve)"
    );
    assert!(
        b2["input"].to_string().contains("newt-compaction-summary"),
        "req2 was proactively compacted before dispatch"
    );
}

/// #1528 B3 (B3-CG-003, item 1, reactive): tools rejected → tools-disabled retry
/// → a context-window 400 → REACTIVE recovery compacts the tools-disabled request
/// (no schema overhead reserved) and redispatches IN THE SAME round. req3 carries
/// no tools, is compacted, and completes.
#[tokio::test]
async fn responses_unsupported_tools_then_cw400_reactive_recovery_no_schema_overhead() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct UnsupportedThen400ThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UnsupportedThen400ThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "this model does not support tools"}
                })),
                1 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "mock",
                    "output": [{"type": "message",
                        "content": [{"type": "output_text", "text": "recovered tools-disabled"}]}]
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(UnsupportedThen400ThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "summarize the history";
    let messages = overflowing_responses_history(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_context_window_400);
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("tools-disabled cw-400 recovers in the same round and completes");
    assert_eq!(reply, "recovered tools-disabled");
    let reqs = server.received_requests().await.expect("request journal");
    assert_eq!(
        reqs.len(),
        3,
        "req1 tools rejected, req2 tools-disabled 400, req3 tools-disabled recovered"
    );
    let b3: serde_json::Value = serde_json::from_slice(&reqs[2].body).unwrap_or_default();
    assert!(
        b3["tools"].is_null(),
        "the recovered request is tools-disabled"
    );
    assert!(
        b3["input"].to_string().contains("newt-compaction-summary"),
        "the recovered tools-disabled request was compacted"
    );
}

#[tokio::test]
async fn responses_missing_call_id_aborts_without_a_followup_request() {
    // RR2: a `function_call` with no `call_id` cannot be correlated to its
    // output. The turn ABORTS — no fabricated id, no follow-up request. The
    // mock's `.expect(1)` proves only the initial dispatch reached the server.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "completed",
            "output": [{"type": "function_call", "name": "run_command", "arguments": "{}"}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let task = "do a thing";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = Some(1_000_000); // fits → dispatches, then aborts on validation

    let err = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a call with no id cannot be correlated");
    assert!(
        err.to_string().contains("malformed provider output"),
        "got: {err}"
    );
}

#[tokio::test]
async fn responses_duplicate_call_ids_abort_without_a_followup_request() {
    // RR2: duplicate `call_id`s mis-route results — abort, no follow-up.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "status": "completed",
            "output": [
                {"type": "function_call", "call_id": "dup", "name": "a", "arguments": "{}"},
                {"type": "function_call", "call_id": "dup", "name": "b", "arguments": "{}"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let task = "do a thing";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = Some(1_000_000);

    let err = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("duplicate ids cannot be correlated");
    assert!(
        err.to_string().contains("malformed provider output"),
        "got: {err}"
    );
}

#[tokio::test]
async fn responses_emits_cognition_as_reasoning_effort_on_the_wire() {
    // The psyche cognition dial must reach the real /v1/responses request as
    // `reasoning.effort` — grounds the pure mapping test against the full loop.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "considered"}]
            }],
            "usage": {"input_tokens": 20, "output_tokens": 3}
        })))
        .mount(&server)
        .await;

    let task = "think hard about this";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.cognition = Some(crate::role_profile::Cognition::Contemplating);

    let (reply, _, _, _) = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect("the request should dispatch");
    assert_eq!(reply, "considered");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body["reasoning"]["effort"], "high",
        "cognition=contemplating must ride the wire as reasoning.effort=high"
    );
}

struct GiantPromptReadResponder {
    openai: bool,
}

impl Respond for GiantPromptReadResponder {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        if self.openai {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_prompt_read",
                        "type": "function",
                        "function": {
                            "name": "prompt_read",
                            "arguments": "{\"address\":\"previous\"}"
                        }
                    }]
                }}]
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {
                        "name": "prompt_read",
                        "arguments": {"address": "previous"}
                    }
                }]}
            }))
        }
    }
}

fn giant_previous_prompt_context(
    store: &SessionPromptStore,
    conversation_id: &str,
    task: &str,
) -> crate::TurnPromptContext {
    let giant = format!("GIANT PREVIOUS OPERATOR PROMPT\n{}", "z\n".repeat(25_000));
    store
        .begin_prompt(
            conversation_id,
            crate::NewPrompt::operator(giant.as_bytes(), giant.as_bytes()),
        )
        .unwrap();
    store
        .begin_prompt(
            conversation_id,
            crate::NewPrompt::operator(task.as_bytes(), task.as_bytes()),
        )
        .unwrap()
}

#[tokio::test]
async fn ollama_giant_prompt_read_result_refuses_before_second_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(GiantPromptReadResponder { openai: false })
        .mount(&server)
        .await;
    let task = "re-read the prior prompt, then explain it";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let prompt_store = SessionPromptStore::default();
    let turn = giant_previous_prompt_context(&prompt_store, "ollama-prompt-read", task);
    let source = prompt_store.source("ollama-prompt-read");
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = Some(8_000);
    ctx.max_tool_rounds = 2;

    let error = chat_complete_with_prompt(ctx, Some(&turn), Some(&source), &mut NoMcp)
        .await
        .expect_err("the giant exact prompt_read result must block the next request");
    let message = error.to_string();
    assert!(
        message.contains("complete inference request needs"),
        "{message}"
    );
    assert!(
        message.contains("tool results were not truncated"),
        "{message}"
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    assert_eq!(requests.len(), 1, "no over-budget second dispatch");
}

#[tokio::test]
async fn openai_chat_giant_prompt_read_result_refuses_before_second_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(GiantPromptReadResponder { openai: true })
        .mount(&server)
        .await;
    let task = "re-read the prior prompt, then explain it";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let prompt_store = SessionPromptStore::default();
    let turn = giant_previous_prompt_context(&prompt_store, "openai-prompt-read", task);
    let source = prompt_store.source("openai-prompt-read");
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = Some(8_000);
    ctx.max_tool_rounds = 2;

    let error = openai_chat_complete_with_prompt(ctx, Some(&turn), Some(&source), &mut NoMcp)
        .await
        .expect_err("the giant exact prompt_read result must block the next request");
    let message = error.to_string();
    assert!(
        message.contains("complete inference request needs"),
        "{message}"
    );
    assert!(
        message.contains("tool results were not truncated"),
        "{message}"
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    assert_eq!(requests.len(), 1, "no over-budget second dispatch");
}

#[tokio::test]
async fn responses_refuses_giant_function_output_before_second_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{
                "type": "function_call",
                "call_id": "call_huge",
                "name": "read_file",
                "arguments": "{\"path\":\"huge.txt\"}"
            }],
            "usage": {"input_tokens": 100, "output_tokens": 10}
        })))
        .mount(&server)
        .await;

    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("huge.txt"), "x".repeat(64_000)).unwrap();
    let task = "read the large fixture and report what it contains";
    let messages = giant_prompt_messages(task);
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.workspace = workspace.path().to_str().unwrap();
    ctx.safe_context = Some(8_000);
    ctx.max_tool_rounds = 2;

    let error = openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("giant function output must block the next request");
    // #1528 B3: the fresh giant tool output is the newest protected item, so the
    // proactive guard's best-effort compaction cannot reduce it; the authoritative
    // preflight then refuses. The headless error CHAIN (`{:#}`) carries BOTH the
    // preflight refusal (root) AND the attached proactive-compaction reason
    // (item 4) — so structured callers see the refusal was preceded by a real,
    // failed compaction attempt, not a naive over-budget send.
    let chain = format!("{error:#}");
    assert!(chain.contains("Responses request needs"), "{chain}");
    assert!(
        chain.contains("function outputs were not truncated"),
        "{chain}"
    );
    assert!(
        chain.contains("proactive compaction"),
        "the fail-closed error chain must attach the B3 compaction reason: {chain}"
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    assert_eq!(
        requests.len(),
        1,
        "the first tool call may dispatch, but its giant output must never be resent"
    );
}

#[tokio::test]
async fn responses_durable_prompt_context_reaches_v1_responses_wire() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": [{
                    "type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "hello from responses"}]
                }],
                "usage": {"input_tokens": 12, "output_tokens": 4}
            })),
        )
        .mount(&server)
        .await;

    let store_root = tempfile::TempDir::new().unwrap();
    let store_workspace = tempfile::TempDir::new().unwrap();
    let store =
        crate::ConversationStore::new(store_root.path(), store_workspace.path(), 0).unwrap();
    let conversation_id = "responses-durable-wire";
    let exact_task = "do the thing through the durable Responses seam";
    let turn_prompt = store
        .begin_prompt(
            conversation_id,
            "Responses durable wire",
            None,
            crate::NewPrompt::operator(exact_task.as_bytes(), exact_task.as_bytes()),
        )
        .unwrap();
    let prompt_source = StorePromptSource::new(&store, conversation_id);
    let expected_address = turn_prompt.active().id().to_string();
    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user(exact_task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let (reply, streamed, usage, _hallu) = openai_responses_complete_with_prompt(
        ChatCtx {
            url: &uri,
            model: "gpt-5-codex",
            kind: BackendKind::Openai,
            api_key: Some("sk-test"),
            messages: &messages,
            task: exact_task,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: 5,
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
            inference_timeout_secs: 120,
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
        },
        Some(&turn_prompt),
        Some(&prompt_source),
        &mut NoMcp,
    )
    .await
    .expect("responses loop returns the message text");
    assert_eq!(reply, "hello from responses");
    assert!(!streamed);
    assert_eq!(usage.map(|u| u.input_tokens), Some(12));
    let requests = server
        .received_requests()
        .await
        .expect("wiremock request journal");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let instructions = body["instructions"].as_str().unwrap_or_default();
    assert!(instructions.contains(prompt_read::ACTIVE_PROMPT_PREFIX));
    assert!(instructions.contains(&format!("address: {expected_address}")));
    assert!(!instructions.contains("<ephemeral-unrecorded>"));
    assert!(body["input"].as_array().is_some_and(|input| input
        .iter()
        .any(|item| item["role"] == "user" && item["content"].as_str() == Some(exact_task))));
}

#[tokio::test]
async fn openai_loop_honors_configured_cap_and_returns_real_final_answer() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiResponder {
            tool_rounds_served: served.clone(),
            final_answer: "openai partial answer".into(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let cap = 2;
    let mut end_reason: Option<crate::TurnEndReason> = None;
    let (reply, streamed, _usage, _hallu) = openai_chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Openai,
            api_key: Some("sk-test"),
            messages: &messages,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: cap,
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
            inference_timeout_secs: 120,
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
            end_reason: Some(&mut end_reason),
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
        },
        &mut NoMcp,
    )
    .await
    .expect("openai_chat_complete should succeed");

    assert_eq!(served.load(Ordering::SeqCst), cap);
    assert!(reply.starts_with("openai partial answer"), "{reply}");
    assert_ne!(reply, "(reached tool-round limit)");
    assert!(!streamed);
    assert_eq!(end_reason, Some(crate::TurnEndReason::RoundCap));
}

/// 17.6: with a recorder lent in `ChatCtx.tool_events`, the Ollama loop
/// records one event per executed tool call — name as invoked, digested
/// args (keys + hash, never raw values), best-effort outcome, duration
/// claim. Without a recorder (every other test here) nothing changes.
#[tokio::test]
async fn ollama_loop_records_tool_events_with_digested_args() {
    let server = MockServer::start().await;
    struct TwoToolResponder;
    impl Respond for TwoToolResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "", "tool_calls": [
                        { "function": { "name": "list_dir",
                                        "arguments": {"path": "."} } },
                        { "function": { "name": "definitely_not_a_real_tool",
                                        "arguments": {"token": "tippy-top-secret"} } }
                    ]}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "done" }
                }))
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(TwoToolResponder)
        .mount(&server)
        .await;

    let ws = tempfile::TempDir::new().unwrap();
    let workspace = ws.path().to_string_lossy().into_owned();
    let messages = msgs();
    let caveats = Caveats::top();
    let mut events: Vec<crate::ToolEvent> = Vec::new();
    chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
            task: "do the thing",
            workspace: &workspace,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: 1,
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
            inference_timeout_secs: 120,
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
            tool_events: Some(&mut events),
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
        },
        &mut NoMcp,
    )
    .await
    .expect("chat_complete should succeed");

    assert_eq!(events.len(), 2, "one event per tool call: {events:?}");
    assert_eq!(events[0].tool, "list_dir");
    assert!(events[0].ok, "a real listing reads as success");
    assert!(events[0].args_digest.contains("path"));
    assert!(events[0].duration_ms.is_some());
    assert_eq!(events[1].tool, "definitely_not_a_real_tool");
    assert!(!events[1].ok, "an unknown tool reads as failure");
    // Args are digested, never recorded raw.
    assert!(events[1].args_digest.contains("token"));
    assert!(
        !events[1].args_digest.contains("tippy-top-secret"),
        "raw arg value leaked: {}",
        events[1].args_digest
    );
}

/// 17.6: the OpenAI loop records the same per-call events (its tool
/// arguments arrive as a JSON *string* — the digest must match the
/// parsed-args digest the Ollama path produces for identical args).
#[tokio::test]
async fn openai_loop_records_tool_events_with_digested_args() {
    let server = MockServer::start().await;
    struct OneToolResponder;
    impl Respond for OneToolResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "list_dir",
                                          "arguments": "{\"path\": \".\"}" }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "done" } }]
                }))
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OneToolResponder)
        .mount(&server)
        .await;

    let ws = tempfile::TempDir::new().unwrap();
    let workspace = ws.path().to_string_lossy().into_owned();
    let messages = msgs();
    let caveats = Caveats::top();
    let mut events: Vec<crate::ToolEvent> = Vec::new();
    openai_chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Openai,
            api_key: Some("sk-test"),
            messages: &messages,
            task: "do the thing",
            workspace: &workspace,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: 1,
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
            inference_timeout_secs: 120,
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
            tool_events: Some(&mut events),
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
        },
        &mut NoMcp,
    )
    .await
    .expect("openai_chat_complete should succeed");

    assert_eq!(events.len(), 1, "one event per tool call: {events:?}");
    assert_eq!(events[0].tool, "list_dir");
    assert!(events[0].ok);
    assert_eq!(
        events[0].args_digest,
        crate::ToolEvent::from_call("x", &serde_json::json!({"path": "."}), true, None).args_digest,
        "string-encoded args must digest like parsed args"
    );
}

#[tokio::test]
async fn cap_exit_fallback_when_final_summary_errors() {
    // No mock for the tools-disabled request would still 404 via the
    // tool-offering mock only matching when... actually both match the same
    // path, so instead we mount a server that always 500s the *second*
    // shape. Simpler: a server that returns tool calls for tools-present
    // and a 500 for tools-absent, forcing the fallback branch.
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct ErrOnFinal {
        served: Arc<AtomicUsize>,
    }
    impl Respond for ErrOnFinal {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "", "tool_calls": [{
                        "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                    }]}
                }))
            } else {
                ResponseTemplate::new(500).set_body_string("boom")
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ErrOnFinal {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, _streamed, _usage, _hallu) = chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: 2,
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
            inference_timeout_secs: 120,
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
        },
        &mut NoMcp,
    )
    .await
    .expect("chat_complete should succeed even when final summary errors");

    // Fallback names the limit + recovery direction — strictly better than the bare
    // placeholder.
    assert!(reply.contains("tool-round limit"));
    assert!(reply.contains("increase the tool-round limit"));
}

/// `run_command` called with a tool name as the first word must return a
/// corrective error message, not shell it through agent-bridle.
#[tokio::test]
async fn run_command_refuses_tool_name_as_shell_command() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    for tool in [
        "list_dir",
        "read_file",
        "write_file",
        "use_skill",
        "web_fetch",
    ] {
        let args = serde_json::json!({ "command": format!("{tool} some/path") });
        let out = execute_tool(
            "run_command",
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None, // memory_source
            None,
            None,
            None, // git_tool
            None, // crew_runner
            None, // scratchpad_store
            None, // code_search
            None, // where_is
            None, // experience_store
            None, // step_ledger
        )
        .await;
        assert!(
            out.contains("is a tool, not a shell command"),
            "expected corrective message for '{tool}', got: {out}"
        );
    }
}

/// When the final summary 500s, the accumulated usage from the tool rounds
/// must still be returned (not None), so usage.jsonl is not blank.
#[tokio::test]
async fn accumulated_usage_survives_summary_failure() {
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));

    struct UsageRoundsErrFinal {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UsageRoundsErrFinal {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "", "tool_calls": [{
                        "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                    }]},
                    // Ollama reports per-round usage even in non-streaming mode.
                    "prompt_eval_count": 100,
                    "eval_count": 20,
                }))
            } else {
                ResponseTemplate::new(500).set_body_string("boom")
            }
        }
    }

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(UsageRoundsErrFinal {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let cap = 2;
    let (reply, _streamed, usage, hallu) = chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: cap,
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
            inference_timeout_secs: 120,
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
        },
        &mut NoMcp,
    )
    .await
    .expect("chat_complete must succeed even when final summary errors");

    // The fallback reply must contain accumulated token counts.
    assert!(reply.contains("tool-round limit"), "got: {reply}");
    assert!(
        reply.contains("in / ") && reply.contains("out tokens"),
        "fallback must include accumulated token counts, got: {reply}"
    );

    // The usage returned must be non-None and reflect the rounds.
    let u = usage.expect("usage must be Some even when final summary fails");
    // SEMANTICS CHANGED in Step 18.1: each round's 100-token prompt
    // contained the same history, so the turn input is the largest single
    // prompt (100), not the 200 sum that double-counted it.
    assert_eq!(
        u.input_tokens, 100,
        "largest single prompt across 2 rounds, not the sum"
    );
    assert_eq!(
        u.output_tokens, 40,
        "2 rounds × 20 output tokens each = 40 total"
    );

    // Unknown tool calls during cap rounds counted as hallucinations.
    assert_eq!(
        hallu, cap as u32,
        "each round had one hallucinated tool call"
    );
}

// -----------------------------------------------------------------------
// Read-only nudge injection test
//
// Scenario: model keeps calling list_dir (read-only) for 3 rounds.
// On round 4 the harness injects the nudge.  The responder detects the
// nudge text in the message list and returns a final text answer instead
// of another tool call, proving the nudge reached the model.
// -----------------------------------------------------------------------

struct ReadOnlyNudgeResponder {
    /// Flipped to true the first time the responder sees the nudge text.
    nudge_seen: Arc<std::sync::atomic::AtomicBool>,
}

impl Respond for ReadOnlyNudgeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default();
        let has_nudge = body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter().any(|m| {
                    m["content"]
                        .as_str()
                        .map(|c| c.contains("read-only rounds so far"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        if has_nudge {
            self.nudge_seen
                .store(true, std::sync::atomic::Ordering::SeqCst);
            // Return a plain text answer — no more tool calls.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "nudge received, writing file now" }
            }))
        } else if request_has_tools(req) {
            // Keep returning list_dir calls until the nudge arrives.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{ "function": {
                        "name": "list_dir",
                        "arguments": { "path": "." }
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
async fn read_only_nudge_injected_after_three_rounds() {
    let server = MockServer::start().await;
    let nudge_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ReadOnlyNudgeResponder {
            nudge_seen: nudge_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, _streamed, _usage, _hallu) = chat_complete(
        ChatCtx {
            url: &server.uri(),
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages: &messages,
            task: "list all files",
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
            caveats: &caveats,
            persona_tools: None,
            cognition: None,
            chat_completions_capability: Default::default(),
            reasoning_replay_scope: crate::model_card::ReasoningReplayScope::Never,
            emits_leading_reasoning: false,
            max_tool_rounds: 10,
            narration_nudge_cap: 1,
            action_nudges: true,
            prompt_disposition: PromptDisposition::Act,
            prompt_intake: None,
            workflow_grace_rounds: 0,
            tool_output_lines: 5,
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
        },
        &mut NoMcp,
    )
    .await
    .expect("chat_complete should succeed");

    assert!(
        nudge_seen.load(std::sync::atomic::Ordering::SeqCst),
        "nudge was never injected after 3 consecutive read-only rounds"
    );
    assert_eq!(
        reply, "nudge received, writing file now",
        "model should have responded to the nudge with a final answer"
    );
}

// -----------------------------------------------------------------------
// #1528 B5 — strict Responses wire validation. Every dispatch goes through
// `validate_responses_request`; a violation dispatches NOTHING. The loop-level
// tests induce a violation through the request the loop actually builds and
// assert `.expect(0)`; the seam-guard tests prove the same for the structural
// violations `build_body` can never emit, by driving the real validate→dispatch
// guard against a wiremock server that must stay untouched.
// -----------------------------------------------------------------------

/// A tool-role message in history must never reach the wire as a raw `tool`
/// input item — the validator refuses it BEFORE dispatch (ZERO requests).
#[tokio::test]
async fn responses_raw_tool_role_in_input_refuses_before_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let task = "B5 raw tool role";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user(task),
        MemMessage {
            role: crate::memory::Role::Tool,
            content: "smuggled privileged content".into(),
        },
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = Some(1_000_000); // budget is not the failure — the role is
    openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a raw tool role in input must fail closed before dispatch");
    assert_no_requests(&server).await;
}

/// A malformed `spill:` content handle in history is refused before dispatch.
#[tokio::test]
async fn responses_malformed_cid_marker_refuses_before_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    // A long alnum run after `spill:` that is not a canonical CID.
    let bogus = "b".to_string() + &"z".repeat(58);
    let task = format!("recall memory_fetch(\"spill:{bogus}\") please");
    let messages = vec![MemMessage::system("base policy"), MemMessage::user(&task)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = Some(1_000_000);
    openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a malformed CID marker must fail closed before dispatch");
    assert_no_requests(&server).await;
}

/// A canonically-spelled but FOREIGN-session `spill:` handle is refused before
/// dispatch — parses, but does not resolve in this session's store.
#[tokio::test]
async fn responses_foreign_session_cid_marker_refuses_before_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let foreign = SpillCid::of(&SpillRecordV1::new(
        SpillScope::Session([2u8; 16]),
        SpillProvenance::ToolOutput { tool_name: None },
        "someone else's secret".to_string(),
    ))
    .unwrap();
    let task = format!("memory_fetch(\"spill:{}\")", foreign.to_handle());
    let messages = vec![MemMessage::system("base policy"), MemMessage::user(&task)];
    let caveats = Caveats::top();
    let uri = server.uri();
    // THIS session's store is a DIFFERENT nonce, so the foreign handle is absent.
    let store = SessionSpillStore::new([1u8; 16]);
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, &task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = Some(1_000_000);
    ctx.spill_store = Some(&store);
    openai_responses_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a foreign-session CID marker must fail closed before dispatch");
    assert_no_requests(&server).await;
}

/// Seam guard: send `body` through the REAL validate→dispatch path against a
/// wiremock server that must stay untouched. A `Some(budget)` drives the
/// over-budget refusal; `tools_permitted=false` models the final summary.
async fn assert_validation_blocks_dispatch(
    body: serde_json::Value,
    tools_permitted: bool,
    authoritative_budget: Option<usize>,
) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let url = format!("{}/v1/responses", server.uri());
    let policy = responses_wire_validation::ResponsesWirePolicy {
        store: crate::responses_wire::STORE_RESPONSE_SERVER_SIDE,
        tools_permitted,
        model: "m",
        authoritative_budget,
        calibration: 1.0,
        estimation: crate::tokens::TokenEstimation::default(),
        spill: None,
        compaction: None,
    };
    let client = reqwest::Client::new();
    // The type system requires a ValidatedResponsesRequest to dispatch; a
    // rejected validation can never construct one, so dispatch is unreachable.
    if let Ok(validated) = responses_wire_validation::validate_responses_request(&body, &policy) {
        let _ = super::dispatch_responses_json(
            &client,
            &url,
            None,
            &validated,
            &tui_retry_policy(&url),
            false,
        )
        .await;
    }
    assert_no_requests(&server).await;
}

fn b5_valid_body() -> serde_json::Value {
    serde_json::json!({
        "model": "m",
        "store": crate::responses_wire::STORE_RESPONSE_SERVER_SIDE,
        "instructions": "be terse",
        "input": [{"role": "user", "content": "hello"}],
    })
}

#[tokio::test]
async fn responses_store_policy_mismatch_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["store"] = serde_json::json!(true);
    assert_validation_blocks_dispatch(body, true, None).await;
}

#[tokio::test]
async fn responses_num_ctx_present_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["num_ctx"] = serde_json::json!(4096);
    assert_validation_blocks_dispatch(body, true, None).await;
}

#[tokio::test]
async fn responses_duplicate_instructions_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["input"] = serde_json::json!([
        {"role": "system", "content": "laundered second instruction source"},
        {"role": "user", "content": "hello"},
    ]);
    assert_validation_blocks_dispatch(body, true, None).await;
}

#[tokio::test]
async fn responses_invalid_input_item_type_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["input"] = serde_json::json!([{"type": "web_search_call", "id": "ws_1"}]);
    assert_validation_blocks_dispatch(body, true, None).await;
}

#[tokio::test]
async fn responses_dangling_function_output_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["input"] = serde_json::json!([
        {"role": "user", "content": "hi"},
        {"type": "function_call_output", "call_id": "c1", "output": "ok"},
    ]);
    assert_validation_blocks_dispatch(body, true, None).await;
}

#[tokio::test]
async fn responses_missing_correlation_id_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["input"] = serde_json::json!([
        {"type": "function_call", "name": "x", "arguments": "{}"},
    ]);
    assert_validation_blocks_dispatch(body, true, None).await;
}

#[tokio::test]
async fn responses_tools_on_final_summary_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["tools"] = serde_json::json!([
        {"type": "function", "name": "x", "parameters": {"type": "object"}},
    ]);
    // tools_permitted=false ⇒ the final tools-disabled summary rejects any tools.
    assert_validation_blocks_dispatch(body, false, None).await;
}

#[tokio::test]
async fn responses_strict_schema_loss_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["tools"] = serde_json::json!([{
        "type": "function",
        "name": "write_file",
        "parameters": {"type": "object", "additionalProperties": false},
    }]);
    assert_validation_blocks_dispatch(body, true, None).await;
}

#[tokio::test]
async fn responses_over_budget_rebuilt_request_blocks_dispatch() {
    let mut body = b5_valid_body();
    body["input"] = serde_json::json!([{"role": "user", "content": "x ".repeat(4_000)}]);
    // A 1-token budget cannot fit the rebuilt request.
    assert_validation_blocks_dispatch(body, true, Some(1)).await;
}

/// Positive control: a valid body passes validation and dispatches EXACTLY once
/// through the real dispatcher — proving the guard blocks only violations.
#[tokio::test]
async fn responses_valid_request_dispatches_exactly_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "ok"}]}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let url = format!("{}/v1/responses", server.uri());
    let policy = responses_wire_validation::ResponsesWirePolicy {
        store: crate::responses_wire::STORE_RESPONSE_SERVER_SIDE,
        tools_permitted: true,
        model: "m",
        authoritative_budget: None,
        calibration: 1.0,
        estimation: crate::tokens::TokenEstimation::default(),
        spill: None,
        compaction: None,
    };
    let validated =
        responses_wire_validation::validate_responses_request(&b5_valid_body(), &policy)
            .expect("a well-formed body validates");
    let client = reqwest::Client::new();
    super::dispatch_responses_json(
        &client,
        &url,
        None,
        &validated,
        &tui_retry_policy(&url),
        false,
    )
    .await
    .expect("the validated request dispatches");
    let reqs = server.received_requests().await.expect("journal");
    assert_eq!(reqs.len(), 1, "a validated request dispatches exactly once");
}

/// A `recover_cw_400` hook for the chat-path cw-400 recovery tests. It
/// parses nothing — it unconditionally reports a roomy recovered input cap
/// so the loop's compress-and-retry path fires: the small test history
/// easily fits the recovered budget, so compaction does not refuse and the
/// SAME logical round is retried in place (#1528, chat-path parity).
fn recover_cw_400_to_40k(_e: &anyhow::Error, _model: &str, _today: &str) -> Option<u32> {
    Some(40_000)
}

/// Serves, in order: a numbered context-window 400, then a real tool round
/// (`get_context_remaining` — executed synthetically in-loop, no side
/// effect), then a plain-text final answer. OpenAI `choices[0].message`
/// shape.
struct OpenAiOverflowThenToolThenDone {
    served: Arc<AtomicUsize>,
}
impl Respond for OpenAiOverflowThenToolThenDone {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        match self.served.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
            })),
            1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "get_context_remaining", "arguments": "{}"}
                    }]
                }}]
            })),
            _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "done"}}]
            })),
        }
    }
}

#[tokio::test]
async fn openai_chat_cw_400_recovery_retries_the_same_logical_round_with_tools() {
    // #1528 (chat-path parity): a cw-400 must retry the SAME logical tool
    // round in place, not advance the round counter. With max_tool_rounds ==
    // 1 the buggy `continue 'round_loop` consumed the only round on recovery
    // and demoted the recovered request to the tools-disabled summary — 2
    // requests: [400, summary]. The fix dispatches a real recovered TOOL
    // round (still carrying tools); only a COMPLETED round then advances to
    // the summary — 3 requests: [400, recovered tool round, summary].
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiOverflowThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "SAME ROUND: recovery must not burn the only tool round";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_cw_400_to_40k);
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect("recovery retries the round in place and the turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [400, recovered tool round, summary]; a 2-request run means \
             recovery burned the only round and demoted to the tools-disabled summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(1)["tools"].is_array(),
        "the RECOVERED request must still carry tools — a real tool round, not the summary"
    );
    assert!(
        body(2)["tools"].is_null(),
        "only the final summary (after the completed round) is tools-disabled"
    );
}

#[tokio::test]
async fn openai_chat_cw_400_recovery_is_bounded() {
    // #1526 review: the chat-transport cw-400 bound (`cw_retries < 2`) has the
    // same exhaustion guarantee the Responses loop proves — a server that 400s
    // every time surfaces the error after at most initial + 2 recoveries, never
    // looping forever. max_tool_rounds == 1 so the bound proven is the INNER
    // `cw_retries` cap (recovery retries in place), not the outer round cap.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
        })))
        .expect(3) // initial + exactly 2 bounded recoveries
        .mount(&server)
        .await;

    let task = "BOUNDED (chat): never loop forever on a persistent 400";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_cw_400_to_40k);
    ctx.max_tool_rounds = 1;

    openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a persistent chat cw-400 surfaces after the bounded retries");
    // `.expect(3)` verified on drop: initial + exactly 2 recoveries.
}

/// Ollama `message` shape of [`OpenAiOverflowThenToolThenDone`]: a numbered
/// context-window 400, then a `get_context_remaining` tool round, then a
/// plain-text final answer.
struct OllamaOverflowThenToolThenDone {
    served: Arc<AtomicUsize>,
}
impl Respond for OllamaOverflowThenToolThenDone {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        match self.served.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
            })),
            1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{"function": {
                        "name": "get_context_remaining", "arguments": {}
                    }}]
                },
                "prompt_eval_count": 10, "eval_count": 1
            })),
            _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "done"}
            })),
        }
    }
}

#[tokio::test]
async fn ollama_chat_cw_400_recovery_retries_the_same_logical_round_with_tools() {
    // #1528 (chat-path parity, Ollama loop): identical intent to the
    // OpenAI-chat case. With max_tool_rounds == 1 the buggy
    // `continue 'round_loop` consumed the only round and demoted the
    // recovered request to the tools-disabled summary — 2 requests. The fix
    // retries the SAME round in place (still WITH tools); only a completed
    // round advances to the summary — 3 requests: [400, recovered tool
    // round, summary].
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaOverflowThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "SAME ROUND: Ollama recovery must not burn the only tool round";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_cw_400_to_40k);
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("recovery retries the round in place and the turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [400, recovered tool round, summary]; a 2-request run means \
             recovery burned the only round and demoted to the tools-disabled summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(1)["tools"].is_array(),
        "the RECOVERED request must still carry tools — a real tool round, not the summary"
    );
    assert!(
        body(2)["tools"].is_null(),
        "only the final summary (after the completed round) is tools-disabled"
    );
}

#[tokio::test]
async fn ollama_chat_malformed_xml_retries_the_same_logical_round_with_tools() {
    // #1533 review: the malformed-XML tool-call recovery appends a corrective
    // nudge and re-dispatches — it must retry the SAME round in place, else at
    // max_tool_rounds == 1 the nudge only ever reaches the tools-disabled
    // summary. Buggy `continue 'round_loop` burned the round → 2 requests +
    // cap-exit. Fixed → 3 requests: [xml error, nudged tool round, summary].
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct XmlThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for XmlThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "ollama xml syntax error in the generated tool call"}
                })),
                1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "",
                        "tool_calls": [{"function": {
                            "name": "get_context_remaining", "arguments": {}}}]},
                    "prompt_eval_count": 10, "eval_count": 1
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "done"}
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(XmlThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "MALFORMED XML: the nudge must reach a tool-capable round";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    // recover_cw_400 stays None: an XML syntax error must NOT be mistaken for
    // a context-window 400 and recovered down the cw-400 path.
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("malformed-XML recovery retries the round and the turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [xml error, nudged tool round, summary]; a 2-request run means \
             the nudge burned the only tool round and reached only the summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(1)["tools"].is_array(),
        "the nudged retry must still carry tools — a real tool round"
    );
    let req1 = serde_json::to_string(&body(1)).unwrap_or_default();
    assert!(
        req1.contains("failed inside Ollama's XML tool-call parser"),
        "the recovered request must carry the corrective XML nudge"
    );
    assert!(
        body(2)["tools"].is_null(),
        "only the final summary is tools-disabled"
    );
}

#[tokio::test]
async fn ollama_chat_malformed_xml_is_bounded_to_two_nudges() {
    // Persistent malformed-XML errors are bounded to the configured 2 nudges
    // (`ollama_xml_retry_nudges < 2`); after that the error surfaces. Exactly
    // 1 + 2 dispatches, then Err — no unbounded in-round loop.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "ollama xml syntax error in the generated tool call"}
        })))
        .expect(3)
        .mount(&server)
        .await;

    let task = "BOUNDED XML: never loop forever on a persistent parser error";
    let messages = vec![MemMessage::system("base policy"), MemMessage::user(task)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    // No recover_cw_400: the XML error must fall through to a terminal error,
    // not be laundered into a cw-400 recovery.
    ctx.max_tool_rounds = 1;

    chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a persistent malformed-XML error surfaces after the bounded nudges");
    // `.expect(3)` verified on drop: initial + exactly 2 nudged retries.
}

#[tokio::test]
async fn ollama_chat_tools_unsupported_recovers_in_the_same_round() {
    // #1533 review: unsupported-tools recovery must retry the SAME round with
    // tools dropped, returning the model's answer DIRECTLY — not burn the
    // round into the tools-disabled cap summary. req2 returns a tool call so
    // the recovered round is provably tool-processing: fixed → 3 requests
    // (tool executes, then summary); buggy `continue 'round_loop` → 2 requests
    // (the burned round's summary can't use the tool call → cap-exit).
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct UnsupportedThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UnsupportedThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "this model does not support tools"}
                })),
                1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "",
                        "tool_calls": [{"function": {
                            "name": "get_context_remaining", "arguments": {}}}]},
                    "prompt_eval_count": 10, "eval_count": 1
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered directly"}
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(UnsupportedThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "TOOLS UNSUPPORTED: retry the same round, don't burn it";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("unsupported-tools recovery retries the same round");
    assert_eq!(reply, "recovered directly");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [tools-unsupported, recovered tool round, summary]; a 2-request \
             run means the round was burned into the tools-disabled cap summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(0)["tools"].is_array(),
        "the first request advertised tools"
    );
    assert!(
        body(1)["tools"].is_null(),
        "the recovered same-round request drops tools"
    );
}

#[tokio::test]
async fn openai_chat_tools_unsupported_recovers_in_the_same_round() {
    // #1533 review: OpenAI-chat unsupported-tools recovery — same as the Ollama
    // case, additionally proving the recovered request drops BOTH `tools` and
    // `tool_choice`.
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct UnsupportedThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UnsupportedThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "this model does not support tools"}
                })),
                1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": null,
                        "tool_calls": [{"id": "c1", "type": "function",
                            "function": {"name": "get_context_remaining", "arguments": "{}"}}]}}]
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "recovered directly"}}]
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(UnsupportedThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "TOOLS UNSUPPORTED (openai): retry the same round, don't burn it";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect("unsupported-tools recovery retries the same round");
    assert_eq!(reply, "recovered directly");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [tools-unsupported, recovered tool round, summary]; a 2-request \
             run means the round was burned into the tools-disabled cap summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(0)["tools"].is_array(),
        "the first request advertised tools"
    );
    assert!(
        body(1)["tools"].is_null() && body(1)["tool_choice"].is_null(),
        "the recovered same-round request drops BOTH tools and tool_choice"
    );
}
