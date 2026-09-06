use super::*;

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
    let mut reaches = Vec::new();
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
            phantom_reaches: Some(&mut reaches),
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
    assert_eq!(
        reaches,
        vec![crate::PhantomReach {
            name_as_called: "definitely_not_a_real_tool".into(),
            resolution: crate::PhantomResolution::Unknown,
            active_context_features: Vec::new(),
        }],
        "record the unknown reach once, without treating the real listing as phantom"
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
