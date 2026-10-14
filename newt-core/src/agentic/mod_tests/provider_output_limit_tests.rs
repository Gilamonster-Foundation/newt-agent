//! Output exhaustion discards every tool; only bounded primary repair is allowed.
use super::*;

async fn limited_reply(responses: bool, tools: bool, streamed: bool, summary: bool) {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    let complete = json!({"path":"marker", "content":"must not execute"}).to_string();
    let cut = format!("{{\"path\":\"large\",\"content\":\"{}", "x".repeat(65_536));
    let usage = json!({"prompt_tokens":40,"completion_tokens":16_384});
    let template = if responses {
        let output = if tools {
            json!([
                {"type":"function_call","call_id":"one","name":"write_file","arguments":complete},
                {"type":"function_call","call_id":"two","name":"write_file","arguments":cut}
            ])
        } else {
            json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"This answer is partial"}]}])
        };
        ResponseTemplate::new(200).set_body_json(json!({"status":"incomplete",
            "incomplete_details":{"reason":"max_output_tokens"}, "output":output,
            "usage":{"input_tokens":40,"output_tokens":16_384}}))
    } else if streamed {
        let delta = if tools {
            json!({"tool_calls":[
                {"index":0,"id":"one","type":"function","function":{"name":"write_file","arguments":complete}},
                {"index":1,"id":"two","type":"function","function":{"name":"write_file","arguments":cut}}
            ]})
        } else {
            json!({"content":"This answer is partial"})
        };
        wire(
            &[
                json!({"choices":[{"delta":delta,"finish_reason":"length"}]}),
                json!({"choices":[],"usage":usage}),
            ],
            true,
        )
    } else {
        let message = if tools {
            json!({"tool_calls":[
                {"id":"one","type":"function","function":{"name":"write_file","arguments":complete}},
                {"id":"two","type":"function","function":{"name":"write_file","arguments":cut}}
            ]})
        } else {
            json!({"content":"This answer is partial"})
        };
        ResponseTemplate::new(200).set_body_json(
            json!({"choices":[{"message":message,"finish_reason":"length"}],"usage":usage}),
        )
    };
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(&server)
        .await;
    let uri = server.uri();
    let messages = msgs();
    let caveats = Caveats::top();
    let attempts = Mutex::new(crate::attempts::AttemptLedger::default());
    let mut end_reason = None;
    let mut context = ctx(&uri, &messages, &caveats);
    context.workspace = root;
    context.action_nudges = false;
    context.attempt_ledger = Some(&attempts);
    context.end_reason = Some(&mut end_reason);
    context.max_tool_rounds = if summary { 0 } else { 3 };
    context.openai_api = if responses {
        crate::OpenAiApi::Responses
    } else {
        crate::OpenAiApi::ChatCompletions
    };
    let result = chat_complete(context, &mut NoMcp).await;
    let requests = server.received_requests().await.unwrap();
    // #2874: capped prose continues too, so both shapes use the bounded
    // two-retry allowance; only the tool-less summary call has none.
    let attempts_expected = if summary { 1 } else { 3 };
    assert_eq!(
        requests.len(),
        attempts_expected,
        "bounded recovery: {result:?}"
    );
    assert!(
        !dir.path().join("marker").exists(),
        "no member of a capped batch may execute"
    );
    assert!(
        !dir.path().join("large").exists(),
        "cut write arguments must never execute"
    );
    let (text, _, usage, _) = result.expect("terminal truncation notice and usage retained");
    assert!(
        text.contains("output limit") && text.contains("truncated"),
        "{text}"
    );
    assert!(
        text.contains("16384") && text.contains("operator"),
        "{text}"
    );
    if !tools {
        assert!(
            text.contains("This answer is partial"),
            "partial prose retained: {text}"
        );
    }
    if summary {
        assert_ne!(end_reason, Some(crate::TurnEndReason::Completed));
    } else {
        assert_eq!(end_reason, Some(crate::TurnEndReason::Failed));
    }
    assert_eq!(usage.unwrap().input_tokens, 40);
    assert_eq!(
        usage.unwrap().output_tokens,
        16_384 * attempts_expected as u32
    );
    assert_eq!(
        attempts.lock().unwrap().records().count(),
        attempts_expected
    );
}

#[tokio::test]
async fn output_limit_chat_prose_is_explicit_and_terminal() {
    for streamed in [false, true] {
        for summary in [false, true] {
            limited_reply(false, false, streamed, summary).await;
        }
    }
}
#[tokio::test]
async fn output_limit_chat_cut_write_retries_are_bounded_and_never_executed() {
    for streamed in [false, true] {
        limited_reply(false, true, streamed, false).await;
    }
}
#[tokio::test]
async fn output_limit_responses_prose_is_explicit_and_terminal() {
    for summary in [false, true] {
        limited_reply(true, false, false, summary).await;
    }
}
#[tokio::test]
async fn output_limit_responses_cut_write_retries_are_bounded_and_never_executed() {
    limited_reply(true, true, false, false).await;
}
