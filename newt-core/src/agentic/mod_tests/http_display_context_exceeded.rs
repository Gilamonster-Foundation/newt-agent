//! The optional display call must preserve both the accepted answer and an
//! observed SSE rejection. The same words inside ordinary text are not one.
use super::*;
use serde_json::{json, Value};

const ACCEPTED: &str = "the accepted original answer";

async fn display_case(body: String, rejected: bool, expected: &str, streamed: bool) {
    let server = MockServer::start().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |request: &Request| {
            let mut requests = captured.lock().unwrap();
            requests.push(body_json(request));
            if requests.len() == 1 {
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices":[{"finish_reason":"stop","message":{
                        "role":"assistant","content":ACCEPTED
                    }}]
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_raw(body.clone().into_bytes(), "text/event-stream")
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
    context.kind = BackendKind::Openai;
    context.action_nudges = false;
    context.solve_obs = Some(&mut observation);
    context.compress_state = Some(&mut state);
    context.end_reason = Some(&mut reason);
    let (reply, was_streamed, _, _) = chat_complete(context, &mut NoMcp).await.unwrap();
    assert_eq!(
        reply, expected,
        "an observed display rejection must preserve the accepted answer"
    );
    assert_eq!(was_streamed, streamed);
    assert_eq!(reason, Some(crate::TurnEndReason::Completed));
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "never repeat a failed optional display request"
    );
    assert_eq!(requests[1]["stream"], true);
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
    if rejected {
        assert_eq!(
            rejections.len(),
            1,
            "the HTTP200 SSE capacity error must be recorded"
        );
        assert!(matches!(
            rejections[0],
            observability::BehaviorSignal::ContextExceeded {
                projected_tokens: None,
                ..
            }
        ));
        assert!(
            state.calibration.ratio(None) >= 1.5,
            "a rejected display with no usage must learn the conservative correction"
        );
    } else {
        assert!(
            rejections.is_empty(),
            "quoted diagnostic text is not a provider error"
        );
        assert_eq!(state.calibration.ratio(None), 1.0);
    }
}

#[tokio::test]
async fn display_sse_context_error_preserves_accepted_answer_and_records_learning() {
    for body in [
        "data: {\"error\":{\"message\":\"Context size has been exceeded\"}}\n\n",
        "event: error\ndata: {\"message\":\"Context size has been exceeded\"}\n\n",
    ] {
        // A complete error envelope is meaningful even without a DONE frame.
        display_case(body.into(), true, ACCEPTED, false).await;
    }
}

#[tokio::test]
async fn display_sse_context_error_after_visible_text_cannot_replace_the_accepted_answer() {
    let partial = json!({"choices":[{"delta":{"content":"visible incomplete answer"}}]});
    let error = json!({"error":{"message":"Context size has been exceeded"}});
    display_case(
        format!("data: {partial}\n\ndata: {error}\n\ndata: [DONE]\n\n"),
        true,
        ACCEPTED,
        false,
    )
    .await;
}

#[tokio::test]
async fn display_sse_quoted_context_error_remains_ordinary_answer_text() {
    let quote =
        "The documented response is {\"error\":{\"message\":\"Context size has been exceeded\"}}.";
    let frame: Value = json!({"choices":[{"delta":{"content":quote},"finish_reason":"stop"}]});
    display_case(
        format!("data: {frame}\n\ndata: [DONE]\n\n"),
        false,
        quote,
        true,
    )
    .await;
}
