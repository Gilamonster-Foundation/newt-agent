//! Independent overflows on long turns get fresh bounded recovery attempts.
use super::*;
use serde_json::{json, Value};

const ANSWER: &str = "recovered third independent overflow";

fn reply(wire: &str, request: &Request, attempt: usize) -> ResponseTemplate {
    let tool = attempt < 6;
    let id = format!("call_{attempt}");
    let args = json!({"round": attempt});
    let body = match wire {
        "ollama" => {
            let mut message = json!({"role":"assistant","content":if tool { "" } else { ANSWER }});
            if tool {
                message["tool_calls"] =
                    json!([{"function":{"name":"get_context_remaining","arguments":args}}]);
            }
            json!({"message":message,"done":true})
        }
        "responses" => {
            json!({"id":format!("response_{attempt}"),"status":"completed","output":if tool {
                json!([{"type":"function_call","id":format!("function_{attempt}"),"call_id":id,"name":"get_context_remaining","arguments":args.to_string()}])
            } else {
                json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":ANSWER}]}])
            }})
        }
        "anthropic" => {
            let content = if tool {
                json!([{"type":"tool_use","id":id,"name":"get_context_remaining","input":args}])
            } else {
                json!([{"type":"text","text":ANSWER}])
            };
            let stop = if tool { "tool_use" } else { "end_turn" };
            if is_stream(request) {
                let mut frames = vec![
                    json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","model":"test-model","content":[]}}),
                ];
                for (index, block) in content.as_array().unwrap().iter().enumerate() {
                    let mut start = block.clone();
                    if block["type"] == "text" {
                        start["text"] = json!("");
                    }
                    frames.push(
                        json!({"type":"content_block_start","index":index,"content_block":start}),
                    );
                    if block["type"] == "text" {
                        frames.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}}));
                    }
                    frames.push(json!({"type":"content_block_stop","index":index}));
                }
                frames.extend([
                    json!({"type":"message_delta","delta":{"stop_reason":stop}}),
                    json!({"type":"message_stop"}),
                ]);
                let body = frames
                    .iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .collect::<String>();
                return ResponseTemplate::new(200)
                    .set_body_raw(body.into_bytes(), "text/event-stream");
            }
            json!({"id":id,"type":"message","role":"assistant","model":"test-model","stop_reason":stop,"content":content})
        }
        "openai" => {
            let mut message = json!({"role":"assistant","content":if tool { "" } else { ANSWER }});
            if tool {
                message["tool_calls"] = json!([{"id":id,"type":"function","function":{"name":"get_context_remaining","arguments":args.to_string()}}]);
            }
            json!({"choices":[{"finish_reason":if tool {"tool_calls"} else {"stop"},"message":message}]})
        }
        _ => unreachable!(),
    };
    ResponseTemplate::new(200).set_body_json(body)
}

async fn overflow_scenario(wire: &'static str, independent: bool) {
    let _settings = default_loop_settings();
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let replay = DisplayReplay::default();
    let endpoint = match wire {
        "ollama" => "/api/chat",
        "responses" => "/v1/responses",
        "anthropic" => "/v1/messages",
        _ => "/v1/chat/completions",
    };
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(move |request: &Request| {
            if wire == "openai" && replay.take(request) {
                return sse_replay(ANSWER);
            }
            let mut requests = captured.lock().unwrap();
            requests.push(body_json(request));
            let attempt = requests.len();
            if !independent || matches!(attempt, 1 | 3 | 5) {
                return ResponseTemplate::new(500).set_body_json(json!({
                    "error":{"code":500,"message":"Context size has been exceeded."}
                }));
            }
            if wire == "openai" && attempt >= 6 {
                replay.arm(request);
            }
            reply(wire, request, attempt)
        })
        .mount(&server)
        .await;
    let seed = history();
    let mut messages = vec![seed[0].clone()];
    for _ in 0..10 {
        messages.extend_from_slice(&seed[1..seed.len() - 1]);
    }
    messages.push(seed.last().unwrap().clone());
    // Enough disposable history for three shrinks above the tool-schema floor.
    let tail_start = messages.len().saturating_sub(20);
    for (index, message) in messages.iter_mut().enumerate() {
        if index >= tail_start && message.content.len() > 1_000 {
            message.content.truncate(200);
        } else if message.content.len() > 1_000 {
            message.content = message.content.repeat(4);
        }
    }
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut context = ctx(&uri, &messages, &caveats);
    context.kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    context.task = "keep the exact operator prompt";
    context.action_nudges = false;
    context.num_ctx = Some(2_097_152);
    context.mid_loop_trim_threshold = 2_000;
    context.max_tool_rounds = 4;
    let result = if wire == "responses" {
        openai_responses_complete(context, &mut NoMcp).await
    } else {
        chat_complete(context, &mut NoMcp).await
    };
    let requests = requests.lock().unwrap();
    if !independent {
        assert_eq!(requests.len(), 3, "{wire}: exactly two consecutive retries");
        assert_eq!(
            crate::retry::classify(&result.unwrap_err()),
            crate::retry::Retryability::ContextExceeded
        );
        let field = if wire == "responses" {
            "input"
        } else {
            "messages"
        };
        assert!(
            requests
                .windows(2)
                .all(|pair| pair[1][field].to_string().len() < pair[0][field].to_string().len()),
            "{wire}: retries must shrink"
        );
        return;
    }
    assert_eq!(requests.len(), 6, "{wire}: {result:?}");
    assert_eq!(
        crate::agentic::model_explanation(&(result.unwrap().0)),
        ANSWER,
        "{wire}"
    );
    for pair in requests.as_chunks::<2>().0 {
        let field = if wire == "responses" {
            "input"
        } else {
            "messages"
        };
        assert!(
            pair[1][field].to_string().len() < pair[0][field].to_string().len(),
            "{wire}: each rejected request must shrink"
        );
    }
}

/// A later overflow gets recovery even after two earlier successful recoveries.
#[tokio::test]
async fn openai_context_exceeded_recovers_across_successful_rounds() {
    overflow_scenario("openai", true).await;
}
/// A later overflow gets recovery even after two earlier successful recoveries.
#[tokio::test]
async fn ollama_context_exceeded_recovers_across_successful_rounds() {
    overflow_scenario("ollama", true).await;
}
/// A later overflow gets recovery even after two earlier successful recoveries.
#[tokio::test]
async fn anthropic_context_exceeded_recovers_across_successful_rounds() {
    overflow_scenario("anthropic", true).await;
}
/// A later overflow gets recovery even after two earlier successful recoveries.
#[tokio::test]
async fn responses_context_exceeded_recovers_across_successful_rounds() {
    overflow_scenario("responses", true).await;
}

/// A failed retry cannot renew its own allowance.
#[tokio::test]
async fn openai_context_exceeded_bounds_consecutive_retries() {
    overflow_scenario("openai", false).await;
}

/// A failed retry cannot renew its own allowance.
#[tokio::test]
async fn ollama_context_exceeded_bounds_consecutive_retries() {
    overflow_scenario("ollama", false).await;
}

/// A failed retry cannot renew its own allowance.
#[tokio::test]
async fn anthropic_context_exceeded_bounds_consecutive_retries() {
    overflow_scenario("anthropic", false).await;
}

/// A failed retry cannot renew its own allowance.
#[tokio::test]
async fn responses_context_exceeded_bounds_consecutive_retries() {
    overflow_scenario("responses", false).await;
}
