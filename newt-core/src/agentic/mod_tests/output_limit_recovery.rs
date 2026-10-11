//! A truncated tool batch must be discarded and retried with bounded host guidance.
use super::*;

fn reply(wire: &str, attempt: usize, limited: bool, tool: bool) -> ResponseTemplate {
    let args = if limited {
        "{\"path\":\"partial\",\"content\":\"cut".into()
    } else {
        serde_json::json!({"path":format!("done{attempt}"),"content":"written"}).to_string()
    };
    let id = format!("call_{attempt}");
    let text = if tool { "" } else { "Finished." };
    let calls = serde_json::json!([{"id":id,"type":"function","function":{"name":"write_file","arguments":args}}]);
    let body = match wire {
        "ollama" => {
            serde_json::json!({"message":{"role":"assistant","content":text,"tool_calls":if tool { calls } else { serde_json::json!([]) }},"done":true,"done_reason":if limited {"length"} else {"stop"},"prompt_eval_count":40,"eval_count":7})
        }
        "responses" => {
            serde_json::json!({"status":if limited {"incomplete"} else {"completed"},"incomplete_details":if limited {serde_json::json!({"reason":"max_output_tokens"})} else {serde_json::Value::Null},"output":if tool {serde_json::json!([{"type":"function_call","call_id":id,"name":"write_file","arguments":args}])} else {serde_json::json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}])},"usage":{"input_tokens":40,"output_tokens":7}})
        }
        "anthropic" => {
            let mut frames = vec![
                serde_json::json!({"type":"message_start","message":{"role":"assistant","content":[],"usage":{"input_tokens":40,"output_tokens":0}}}),
            ];
            if tool {
                frames.push(serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":"write_file","input":{}}}));
                frames.push(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args}}));
            } else {
                frames.push(serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                frames.push(serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}));
            }
            // A genuinely cut tool block has no content_block_stop.
            if !limited {
                frames.push(serde_json::json!({"type":"content_block_stop","index":0}));
            }
            frames.push(serde_json::json!({"type":"message_delta","delta":{"stop_reason":if limited {"max_tokens"} else if tool {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":7}}));
            frames.push(serde_json::json!({"type":"message_stop"}));
            return ResponseTemplate::new(200).set_body_raw(
                frames
                    .iter()
                    .map(|v| format!("data: {v}\n\n"))
                    .collect::<String>(),
                "text/event-stream",
            );
        }
        _ => {
            serde_json::json!({"choices":[{"finish_reason":if limited {"length"} else if tool {"tool_calls"} else {"stop"},"message":{"role":"assistant","content":text,"tool_calls":if tool {calls} else {serde_json::json!([])}}}],"usage":{"prompt_tokens":40,"completion_tokens":7}})
        }
    };
    ResponseTemplate::new(200).set_body_json(body)
}

async fn scenario(wire: &'static str, bounded: bool) {
    let _settings = default_loop_settings();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let requests = captured.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let mut requests = requests.lock().unwrap();
            requests.push(body_json(request));
            let attempt = requests.len();
            // Three independent recoveries exceed a lifetime allowance of two.
            let limited = bounded || matches!(attempt, 1 | 3 | 5);
            reply(wire, attempt, limited, bounded || attempt < 7)
        })
        .mount(&server)
        .await;
    let uri = server.uri();
    let messages = msgs();
    let caveats = crate::confined_exec::workspace_confined_caveats(dir.path());
    let mut end = None;
    let mut context = ctx(&uri, &messages, &caveats);
    context.kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    if wire == "responses" {
        context.openai_api = crate::OpenAiApi::Responses;
    }
    context.workspace = dir.path().to_str().unwrap();
    context.action_nudges = false;
    context.end_reason = Some(&mut end);
    context.max_tool_rounds = 4;
    let result = chat_complete(context, &mut NoMcp).await.unwrap();
    let requests = captured.lock().unwrap();
    assert_eq!(
        requests.len(),
        if bounded { 3 } else { 7 },
        "{wire}: {result:?}"
    );
    assert!(
        !dir.path().join("partial").exists(),
        "partial tool must never execute"
    );
    for (index, request) in requests.iter().enumerate().skip(1) {
        let field = if wire == "responses" {
            "input"
        } else {
            "messages"
        };
        let entries = request[field].as_array().unwrap();
        let notes: Vec<_> = entries
            .iter()
            .filter(|m| {
                m.to_string()
                    .contains("your last reply was cut off at the output limit")
            })
            .collect();
        assert_eq!(notes.len(), 1, "{wire} request {index}: one bounded note");
        assert_eq!(
            notes[0]["role"], "user",
            "host guidance is never assistant speech"
        );
        assert!(notes[0].to_string().contains("[loop-guidance] [harness]"));
        assert!(
            !request[field].to_string().contains("partial"),
            "discarded tool not replayed"
        );
    }
    assert_eq!(result.2.unwrap().output_tokens, requests.len() as u32 * 7);
    if bounded {
        assert_eq!(end, Some(crate::TurnEndReason::Failed));
        assert!(result
            .0
            .contains("Continuation requires a new operator request"));
    } else {
        for attempt in [2, 4, 6] {
            assert_eq!(
                std::fs::read_to_string(dir.path().join(format!("done{attempt}"))).unwrap(),
                "written"
            );
        }
        assert_eq!(model_explanation(&result.0), "Finished.");
        assert!(!result.0.contains("your last reply was cut off"));
    }
}

macro_rules! wire_test {
    ($name:ident, $wire:literal) => {
        /// Output-limit recovery executes the fresh tool, resets after progress,
        /// and stops honestly after two consecutive retries on this wire.
        #[tokio::test]
        async fn $name() {
            scenario($wire, false).await;
            scenario($wire, true).await;
        }
    };
}
wire_test!(output_limit_recovery_chat, "openai");
wire_test!(output_limit_recovery_responses, "responses");
wire_test!(output_limit_recovery_ollama, "ollama");
wire_test!(output_limit_recovery_anthropic, "anthropic");
