use super::*;

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
