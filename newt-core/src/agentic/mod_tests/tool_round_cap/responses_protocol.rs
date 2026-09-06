use super::*;

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
