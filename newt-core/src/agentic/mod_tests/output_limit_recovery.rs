//! A truncated tool batch must be discarded and retried with bounded host guidance.
use super::*;

pub(super) fn reply(wire: &str, attempt: usize, limited: bool, tool: bool) -> ResponseTemplate {
    reply_with(wire, attempt, limited, tool, "Finished.")
}

fn reply_with(
    wire: &str,
    attempt: usize,
    limited: bool,
    tool: bool,
    prose: &str,
) -> ResponseTemplate {
    let args = if limited {
        "{\"path\":\"partial\",\"content\":\"cut".into()
    } else {
        serde_json::json!({"path":format!("done{attempt}"),"content":"written"}).to_string()
    };
    let id = format!("call_{attempt}");
    let text = if tool { "" } else { prose };
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

type Reply = (String, bool, Option<crate::TokenUsage>, u32);
type Run = (
    Reply,
    Vec<serde_json::Value>,
    Option<crate::TurnEndReason>,
    tempfile::TempDir,
);

/// One turn on `wire` with an optional explicit allowance; the responder sees
/// the 1-based attempt and the request body.
async fn run(
    wire: &'static str,
    allowance: Option<u32>,
    obs: Option<&mut crate::agentic::SolveObservation>,
    respond: impl Fn(usize, &serde_json::Value) -> ResponseTemplate + Send + Sync + 'static,
) -> Run {
    let _settings = default_loop_settings();
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let requests = captured.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let mut requests = requests.lock().unwrap();
            requests.push(body_json(request));
            respond(requests.len(), requests.last().unwrap())
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
    context.output_allowance = allowance;
    context.solve_obs = obs;
    let result = chat_complete(context, &mut NoMcp).await.unwrap();
    let requests = captured.lock().unwrap().clone();
    (result, requests, end, dir)
}

/// The output cap each wire sends: `num_predict`, `max_output_tokens`, or a
/// Chat/Messages `max_tokens` (`max_completion_tokens` on first-party OpenAI).
fn sent_cap(wire: &str, body: &serde_json::Value) -> Option<u64> {
    match wire {
        "ollama" => body["options"]["num_predict"].as_u64(),
        "responses" => body["max_output_tokens"].as_u64(),
        _ => body["max_tokens"]
            .as_u64()
            .or_else(|| body["max_completion_tokens"].as_u64()),
    }
}

fn history(wire: &str, body: &serde_json::Value) -> String {
    body[if wire == "responses" {
        "input"
    } else {
        "messages"
    }]
    .to_string()
}

/// The turn finished normally. The Responses loop records no end reason on an
/// ordinary answer (pre-existing; it writes the slot only on failure and cap
/// paths), so there "normal" means "not Failed".
fn assert_completed(wire: &str, end: Option<crate::TurnEndReason>) {
    if wire == "responses" {
        assert_ne!(end, Some(crate::TurnEndReason::Failed), "{wire}");
    } else {
        assert_eq!(end, Some(crate::TurnEndReason::Completed), "{wire}");
    }
}

async fn scenario(wire: &'static str, bounded: bool) {
    let (result, requests, end, dir) = run(wire, None, None, move |attempt, _| {
        // Three independent recoveries exceed a lifetime allowance of two.
        let limited = bounded || matches!(attempt, 1 | 3 | 5);
        reply(wire, attempt, limited, bounded || attempt < 7)
    })
    .await;
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
        assert!(
            !request[field].to_string().contains("partial"),
            "discarded tool not replayed"
        );
        // A retry either raises the cap (G1) or, with no headroom, carries the
        // one bounded note; never more than one.
        let raised = sent_cap(wire, request) > sent_cap(wire, &requests[0]);
        assert!(notes.len() <= 1, "{wire} request {index}: one bounded note");
        if notes.is_empty() {
            assert!(raised, "{wire} request {index}: no note needs a raised cap");
            continue;
        }
        assert_eq!(
            notes[0]["role"], "user",
            "host guidance is never assistant speech"
        );
        assert!(notes[0].to_string().contains("[loop-guidance] [harness]"));
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
        #[serial_test::serial(anthropic_loop_env)]
        async fn $name() {
            // Hold process_env across both scenarios and env restoration.
            let _settings = default_loop_settings();
            let _env = crate::agentic::anthropic_loop_tests::test_env_with_process_lock(true);
            scenario($wire, false).await;
            scenario($wire, true).await;
        }
    };
}
wire_test!(output_limit_recovery_chat, "openai");
wire_test!(output_limit_recovery_responses, "responses");
wire_test!(output_limit_recovery_ollama, "ollama");
wire_test!(output_limit_recovery_anthropic, "anthropic");

/// G1: a capped tool batch retries with a doubled cap (bounded by the
/// ceiling), so the same call fits instead of failing twice and halting.
async fn raise_scenario(wire: &'static str) {
    let (result, requests, end, dir) = run(wire, Some(1000), None, move |attempt, body| {
        let limited = sent_cap(wire, body).unwrap_or(0) < 2000;
        reply(wire, attempt, limited, attempt < 3)
    })
    .await;
    let caps: Vec<_> = requests.iter().map(|r| sent_cap(wire, r)).collect();
    assert_eq!(requests.len(), 3, "{wire}: caps {caps:?} {result:?}");
    assert_eq!(caps[0], Some(1000), "{wire}");
    assert!(
        caps[1] > caps[0] && caps[1] <= Some(16_384),
        "{wire}: {caps:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("done2")).unwrap(),
        "written"
    );
    assert!(!dir.path().join("partial").exists());
    assert_completed(wire, end);
}

/// G2: a text-only truncation continues the turn; the partial text is kept
/// in history and in the stitched answer, and the turn completes.
async fn text_scenario(wire: &'static str) {
    let (result, requests, end, _dir) = run(wire, None, None, move |attempt, _| {
        if attempt == 1 {
            reply_with(wire, attempt, true, false, "First half, ")
        } else {
            reply_with(wire, attempt, false, false, "second half.")
        }
    })
    .await;
    assert_eq!(requests.len(), 2, "{wire}: {result:?}");
    let second = history(wire, &requests[1]);
    assert!(second.contains("First half, "), "{wire}: {second}");
    assert!(
        second.contains("continue exactly where it stopped"),
        "{wire}: {second}"
    );
    assert!(
        result.0.contains("First half, second half."),
        "{wire}: {}",
        result.0
    );
    assert!(!result.0.contains("operator request"), "{wire}");
    assert_completed(wire, end);
}

/// A cap already at the ceiling keeps today's note-only retry and still
/// stops honestly after two consecutive retries.
async fn ceiling_scenario(wire: &'static str) {
    let (result, requests, end, _dir) = run(wire, Some(16_384), None, move |attempt, _| {
        reply(wire, attempt, true, true)
    })
    .await;
    assert_eq!(requests.len(), 3, "{wire}: {result:?}");
    let caps: Vec<_> = requests.iter().map(|r| sent_cap(wire, r)).collect();
    assert!(
        caps.iter().all(|cap| *cap == Some(16_384)),
        "{wire}: {caps:?}"
    );
    assert_eq!(end, Some(crate::TurnEndReason::Failed), "{wire}");
}

macro_rules! continue_test {
    ($raise:ident, $text:ident, $ceiling:ident, $wire:literal) => {
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env)]
        async fn $raise() {
            let _settings = default_loop_settings();
            let _env = crate::agentic::anthropic_loop_tests::test_env_with_process_lock(true);
            raise_scenario($wire).await;
        }
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env)]
        async fn $text() {
            let _settings = default_loop_settings();
            let _env = crate::agentic::anthropic_loop_tests::test_env_with_process_lock(true);
            text_scenario($wire).await;
        }
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env)]
        async fn $ceiling() {
            let _settings = default_loop_settings();
            let _env = crate::agentic::anthropic_loop_tests::test_env_with_process_lock(true);
            ceiling_scenario($wire).await;
        }
    };
}
continue_test!(
    retry_raises_cap_then_succeeds_chat,
    text_only_truncation_continues_chat,
    cap_at_ceiling_falls_back_to_note_only_chat,
    "openai"
);
continue_test!(
    retry_raises_cap_then_succeeds_responses,
    text_only_truncation_continues_responses,
    cap_at_ceiling_falls_back_to_note_only_responses,
    "responses"
);
continue_test!(
    retry_raises_cap_then_succeeds_ollama,
    text_only_truncation_continues_ollama,
    cap_at_ceiling_falls_back_to_note_only_ollama,
    "ollama"
);
continue_test!(
    retry_raises_cap_then_succeeds_anthropic,
    text_only_truncation_continues_anthropic,
    cap_at_ceiling_falls_back_to_note_only_anthropic,
    "anthropic"
);

/// G3: a thinking-only `max_tokens` stop reaches the Anthropic
/// `reasoning_overflow` handler exactly once instead of the early return.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env)]
async fn anthropic_thinking_only_stop_reaches_reasoning_overflow() {
    let _settings = default_loop_settings();
    let _env = crate::agentic::anthropic_loop_tests::test_env_with_process_lock(true);
    let mut obs = crate::agentic::SolveObservation::default();
    let (result, requests, end, _dir) = run("anthropic", None, Some(&mut obs), |_, _| {
        let frames = [
            serde_json::json!({"type":"message_start","message":{"role":"assistant","content":[],"usage":{"input_tokens":40,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private unfinished plan"}}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":7}}),
            serde_json::json!({"type":"message_stop"}),
        ];
        ResponseTemplate::new(200).set_body_raw(
            frames
                .iter()
                .map(|v| format!("data: {v}\n\n"))
                .collect::<String>(),
            "text/event-stream",
        )
    })
    .await;
    let overflows = obs
        .behavior_signals
        .iter()
        .filter(|signal| {
            matches!(
                signal,
                crate::agentic::observability::BehaviorSignal::ReasoningOverflow { .. }
            )
        })
        .count();
    assert_eq!(overflows, 1, "{result:?}");
    assert_eq!(requests.len(), 1, "no continuation is advertised");
    assert!(!result.0.contains("private unfinished plan"));
    assert_eq!(end, Some(crate::TurnEndReason::Failed));
}
