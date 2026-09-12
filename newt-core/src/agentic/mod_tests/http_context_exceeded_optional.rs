//! Optional cap summaries still owe accounting for observed context failures.
//! Each provider first completes a real built-in tool through the normal loop.
use super::*;
use serde_json::{json, Value};

const CALL: &str = "completed_cap_call";
const TOOL: &str = "get_context_remaining";

fn completed_tool(wire: &str, request: &Request) -> ResponseTemplate {
    let body = match wire {
        "ollama" => json!({"message":{"role":"assistant","content":"","tool_calls":[
            {"function":{"name":TOOL,"arguments":{}}}
        ]},"done":true}),
        "responses" => json!({"id":"response_completed","status":"completed","output":[
            {"type":"function_call","id":"function_completed","call_id":CALL,"name":TOOL,"arguments":"{}"}
        ]}),
        "anthropic" if is_stream(request) => {
            let frames = [
                json!({"type":"message_start","message":{"id":"message_completed","type":"message","role":"assistant","model":"test-model","content":[]}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":CALL,"name":TOOL,"input":{}}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
                json!({"type":"message_stop"}),
            ];
            let body = frames
                .iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect::<String>();
            return ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream");
        }
        "anthropic" => {
            json!({"id":"message_completed","type":"message","role":"assistant","model":"test-model","stop_reason":"tool_use","content":[
                {"type":"tool_use","id":CALL,"name":TOOL,"input":{}}
            ]})
        }
        _ => unreachable!("the fixture names one supported adapter"),
    };
    ResponseTemplate::new(200).set_body_json(body)
}

fn retained_result<'a>(wire: &str, summary: &'a Value) -> Option<&'a str> {
    let items = summary[if wire == "responses" {
        "input"
    } else {
        "messages"
    }]
    .as_array()?;
    match wire {
        "ollama" => items.iter().find(|item| item["role"] == "tool")?["content"].as_str(),
        "responses" => items
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == CALL)?
            ["output"]
            .as_str(),
        "anthropic" => items
            .iter()
            .filter(|item| item["role"] == "user")
            .filter_map(|item| item["content"].as_array())
            .flatten()
            .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == CALL)?
            ["content"]
            .as_str(),
        _ => None,
    }
}

async fn cap_rejection(wire: &'static str, context_exceeded: bool) {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &Request| {
            let mut requests = captured.lock().unwrap();
            requests.push(body_json(request));
            if requests.len() == 1 {
                completed_tool(wire, request)
            } else {
                ResponseTemplate::new(if context_exceeded { 500 } else { 400 }).set_body_json(
                    json!({"error":{"message":if context_exceeded {
                        "Context size has been exceeded."
                    } else {
                        "invalid optional summary request"
                    }}}),
                )
            }
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut observation = observability::SolveObservation::default();
    let mut state = CompressState::new();
    let mut reason = None;
    let mut context = ctx(&uri, &messages, &caveats);
    context.kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    context.max_tool_rounds = 1;
    context.action_nudges = false;
    context.solve_obs = Some(&mut observation);
    context.compress_state = Some(&mut state);
    context.end_reason = Some(&mut reason);
    let result = if wire == "responses" {
        openai_responses_complete(context, &mut NoMcp).await
    } else {
        chat_complete(context, &mut NoMcp).await
    };
    let (reply, _, usage, _) =
        result.expect("the optional failure must preserve the completed tool round");
    assert!(reply.contains("tool-round limit (1"), "{wire}: {reply}");
    assert_eq!(reason, Some(crate::TurnEndReason::RoundCap), "{wire}");
    assert!(
        usage.is_none(),
        "the fixture supplies no usage evidence: {wire}"
    );
    let requests = requests.lock().unwrap().clone();
    let summaries = &requests[1..];
    assert!(
        !summaries.is_empty(),
        "the cap must exercise its optional dispatch: {wire}"
    );
    assert!(
        summaries.len() <= 3,
        "at most two smaller summary attempts: {wire}"
    );
    let completed = retained_result(wire, &summaries[0])
        .expect("the completed tool result reaches the cap request");
    assert!(
        completed.starts_with("Context budget:"),
        "the actual built-in tool completed: {wire}: {completed}"
    );
    for summary in summaries {
        assert!(
            summary["tools"].is_null(),
            "the tool cap cannot be bypassed: {wire}"
        );
        assert_eq!(
            retained_result(wire, summary),
            Some(completed),
            "retain the exact observed result: {wire}"
        );
    }
    assert!(
        summaries
            .windows(2)
            .all(|pair| pair[1].to_string().len() < pair[0].to_string().len()),
        "a rejected summary must never be re-sent unchanged: {wire}"
    );
    let rejections = observation
        .behavior_signals
        .iter()
        .filter(|signal| {
            matches!(
                signal,
                observability::BehaviorSignal::ContextExceeded { .. }
            )
        })
        .collect::<Vec<_>>();
    if context_exceeded {
        assert_eq!(
            rejections.len(),
            summaries.len(),
            "a rejected cap summary must be recorded for {wire}"
        );
        assert!(matches!(
            rejections.last().unwrap(),
            observability::BehaviorSignal::ContextExceeded {
                projected_tokens: None,
                ..
            }
        ));
        assert!(
            state.calibration.ratio(None) >= 1.5,
            "missing usage must learn the conservative correction: {wire}"
        );
    } else {
        assert!(
            rejections.is_empty(),
            "ordinary provider errors are not context rejections: {wire}"
        );
        assert_eq!(
            state.calibration.ratio(None),
            1.0,
            "ordinary provider errors cannot recalibrate the session: {wire}"
        );
    }
}

#[tokio::test]
async fn ollama_cap_context_failure_retains_completed_tool_and_records_learning() {
    cap_rejection("ollama", true).await;
}

#[tokio::test]
async fn anthropic_cap_context_failure_retains_completed_tool_and_records_learning() {
    cap_rejection("anthropic", true).await;
}

#[tokio::test]
async fn responses_cap_context_failure_retains_completed_tool_and_records_learning() {
    cap_rejection("responses", true).await;
}

#[tokio::test]
async fn ordinary_optional_cap_errors_leave_context_classification_and_calibration_unchanged() {
    for wire in ["ollama", "anthropic", "responses"] {
        cap_rejection(wire, false).await;
    }
}
