use super::*;

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
