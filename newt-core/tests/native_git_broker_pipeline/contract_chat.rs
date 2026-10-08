//! Scripted inference-loop fixture: one run_command call, then a final answer.
//! Only the host-side mock model uses loopback HTTP; child tools retain net:none.
//! Adapted from agentic/mod_tests/openai_stream_loop.rs and http_loop.rs.
use newt_core::{
    agentic::{chat_complete, GitTool, PromptDisposition},
    worktree_adoption::WorktreeSession,
    BackendKind, Caveats, ChatCtx, MemMessage, NoMcp,
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

pub async fn dispatch(
    session: &WorktreeSession,
    workspace: &Path,
    caveats: &Caveats,
    command: &str,
    git_tool: Option<&dyn GitTool>,
) -> String {
    let server = MockServer::start().await;
    let round = Arc::new(AtomicUsize::new(0));
    let seen = round.clone();
    let args = serde_json::json!({"command": command}).to_string();
    Mock::given(method("POST")).and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            let mut message = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                serde_json::json!({"role":"assistant", "tool_calls":[{"id":"contract-call", "type":"function", "function":{"name":"run_command", "arguments":args}}]})
            } else { serde_json::json!({"role":"assistant", "content":"Fixture complete."}) };
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            if body["stream"] != true {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices":[{"message":message}]}));
            }
            let finish = if let Some(calls) = message["tool_calls"].as_array_mut() {
                calls[0]["index"] = serde_json::json!(0);
                "tool_calls"
            } else { "stop" };
            let chunk = serde_json::json!({"choices":[{"index":0,"delta":message}]});
            let done = serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]});
            ResponseTemplate::new(200).set_body_raw(format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n"), "text/event-stream")
        }).mount(&server).await;
    let messages = vec![
        MemMessage::system("Execute the requested fixture command once."),
        MemMessage::user(command),
    ];
    let uri = server.uri();
    let mut context = ctx(&uri, &messages, caveats);
    context.workspace = workspace.to_str().unwrap();
    context.worktree_session = Some(session);
    context.git_tool = git_tool;
    chat_complete(context, &mut NoMcp)
        .await
        .expect("scripted ChatCtx turn");
    let requests = server.received_requests().await.unwrap();
    requests
        .iter()
        .rev()
        .find_map(|req| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            body["messages"]
                .as_array()?
                .iter()
                .rev()
                .find(|message| message["role"] == "tool")?["content"]
                .as_str()
                .map(str::to_owned)
        })
        .expect("the model must receive the actual tool result")
}

fn ctx<'a>(server_uri: &'a str, messages: &'a [MemMessage], caveats: &'a Caveats) -> ChatCtx<'a> {
    ChatCtx {
        shell_dialect: newt_core::agentic::shell_dialect_sentence(false, false),
        command_budget: Default::default(),
        overflow_retry: Default::default(),
        run_allowance: None,
        verify_outcomes: false,
        round_cap_hit: None,
        smart_harness: None,
        url: server_uri,
        model: "test-model",
        kind: BackendKind::Openai,
        api_key: None,
        messages,
        task: "do the thing",
        workspace: "",
        default_command_cwd: None,
        worktree_session: None,
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
        responses_capability: Default::default(),
        openai_api: Default::default(),
        output_allowance: None,
        attempt_ledger: None,
        reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope::Never,
        emits_leading_reasoning: false,
        max_tool_rounds: 8,
        narration_nudge_cap: 1,
        action_nudges: false,
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
        compaction_trigger_policy: newt_core::CompactionTriggerPolicy::HeadroomAware,
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
        estimation: newt_core::tokens::TokenEstimation::default(),
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
        plan_draft_sink: None,
        steering: None,
        completed_spill_renderer: None,
    }
}
