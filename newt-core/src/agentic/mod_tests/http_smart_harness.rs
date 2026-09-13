//! Provider-loop contracts: one recorded observation precedes each auxiliary verdict.
use super::*;
use crate::agentic::smart_harness::{AdjudicationSettings, SmartHarness};

#[cfg(target_os = "linux")]
#[path = "http_smart_completion.rs"]
mod completion;

async fn run(
    wire: &str,
    texts: &[&str],
    verdicts: &[&str],
    cap: usize,
) -> (String, crate::TurnEndReason, Vec<Request>) {
    let server = MockServer::start().await;
    let texts = texts.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let calls = Arc::new(AtomicUsize::new(0));
    let wire_owned = wire.to_string();
    Mock::given(method("POST")).respond_with(move |request: &Request| {
        let body = body_json(request);
        assert_eq!(body["stream"], wire_owned == "openai", "OpenAI streams the primary response; smart mode still avoids an extra display generation");
        if wire_owned == "openai" {
            assert_eq!(body["stream_options"]["include_usage"], true);
        }
        let i = calls.fetch_add(1, Ordering::SeqCst);
        let text = &texts[i.min(texts.len()-1)];
        let value = if text == "<tool>" {
            let args = serde_json::json!({"path":"missing.txt"});
            match wire_owned.as_str() {
                "ollama" => serde_json::json!({"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":args}}]},"done":true}),
                "anthropic" => serde_json::json!({"id":"msg_1","type":"message","role":"assistant","model":"test-model","stop_reason":"tool_use","content":[{"type":"tool_use","id":"call_1","name":"read_file","input":args}]}),
                "responses" => serde_json::json!({"id":"resp_1","status":"completed","output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":args.to_string()}]}),
                _ => serde_json::json!({"choices":[{"message":{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read_file","arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]}),
            }
        } else { match wire_owned.as_str() {
            "ollama" => serde_json::json!({"message":{"role":"assistant","content":text},"done":true}),
            "anthropic" => serde_json::json!({"id":"msg_1","type":"message","role":"assistant","model":"test-model","stop_reason":"end_turn","content":[{"type":"text","text":text}],"usage":{"input_tokens":10,"output_tokens":5}}),
            "responses" => serde_json::json!({"id":"resp_1","status":"completed","model":"test-model","output":[{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}]}),
            _ => serde_json::json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]}),
        }};
        ResponseTemplate::new(200).set_body_json(value)
    }).mount(&server).await;
    let verdicts = Arc::new(Mutex::new(
        verdicts
            .iter()
            .map(|s| s.to_string())
            .collect::<std::collections::VecDeque<_>>(),
    ));
    let harness = SmartHarness::new(
        agent_harness::Session::new(Default::default()).unwrap(),
        Arc::new(move |prompt| {
            assert!(
                prompt.contains("reply_cid"),
                "the auxiliary sees an already-recorded reply CID"
            );
            let verdict = verdicts
                .lock()
                .unwrap()
                .pop_front()
                .expect("bounded auxiliary call");
            Box::pin(async move { Ok(verdict) })
        }),
        AdjudicationSettings::default(),
    )
    .unwrap();
    let uri = server.uri();
    let messages = msgs();
    let caveats = Caveats::top();
    let mut context = ctx(&uri, &messages, &caveats);
    context.smart_harness = Some(&harness);
    context.max_tool_rounds = cap;
    context.kind = match wire {
        "ollama" => BackendKind::Ollama,
        "anthropic" => BackendKind::Anthropic,
        _ => BackendKind::Openai,
    };
    let mut reason = None;
    context.end_reason = Some(&mut reason);
    let result = if wire == "responses" {
        openai_responses_complete(context, &mut NoMcp).await
    } else {
        chat_complete(context, &mut NoMcp).await
    }
    .expect("smart provider round");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        harness.replay_last_request().unwrap(),
        requests.last().unwrap().body,
        "cold replay matches actual HTTP request bytes"
    );
    (
        result.0,
        reason.expect("every smart terminal reports its control outcome"),
        requests,
    )
}

#[tokio::test]
async fn all_four_wires_deliver_a_real_answer_after_a_nudge_without_regeneration() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let (text, reason, requests) = run(
            wire,
            &["Let me calculate that.", "The answer is three."],
            &["\"narration\"", "\"answer\""],
            4,
        )
        .await;
        assert_eq!(text, "The answer is three.", "{wire}");
        assert_eq!(reason, crate::TurnEndReason::Completed, "{wire}");
        assert_eq!(requests.len(), 2, "{wire}");
        assert!(
            String::from_utf8_lossy(&requests[1].body).contains("[loop-guidance]"),
            "{wire}"
        );
    }
}

#[tokio::test]
async fn all_four_wires_preserve_questions_and_honestly_stop_exhausted_narration() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let (text, reason, requests) =
            run(wire, &["Which repository?"], &["\"question\""], 4).await;
        assert_eq!(text, "Which repository?", "{wire}");
        assert_eq!(reason, crate::TurnEndReason::AwaitingOperator, "{wire}");
        assert_eq!(requests.len(), 1, "{wire}");
        let (text, reason, requests) = run(
            wire,
            &["Let me edit it.", "I am finished."],
            &["\"narration\"", "\"narration\""],
            4,
        )
        .await;
        assert!(text.contains("Incomplete"), "{wire}");
        assert_eq!(
            reason,
            crate::TurnEndReason::NarrationCapExhausted,
            "{wire}"
        );
        assert_eq!(requests.len(), 2, "{wire}");
    }
}

#[tokio::test]
async fn all_four_wires_fail_loudly_on_malformed_adjudication_and_final_round_narration() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let (text, reason, requests) =
            run(wire, &["Done."], &["prose surrounding \"answer\""], 4).await;
        assert!(text.contains("AdjudicationFailure"), "{wire}");
        assert_eq!(reason, crate::TurnEndReason::Failed, "{wire}");
        assert_eq!(requests.len(), 1, "{wire}");
        let (_, reason, requests) = run(wire, &["I will inspect it."], &["\"narration\""], 1).await;
        assert_eq!(reason, crate::TurnEndReason::NarrationFinalRound, "{wire}");
        assert_eq!(requests.len(), 1, "{wire}");
    }
}

#[tokio::test]
async fn all_four_wires_record_validated_tools_and_stop_tool_only_caps_incomplete() {
    for wire in ["ollama", "openai", "anthropic", "responses"] {
        let (text, reason, requests) =
            run(wire, &["<tool>", "The file is absent."], &["\"answer\""], 3).await;
        assert_eq!(text, "The file is absent.", "{wire}");
        assert_eq!(reason, crate::TurnEndReason::Completed, "{wire}");
        assert_eq!(requests.len(), 2, "{wire}");
        let (_, reason, requests) = run(wire, &["<tool>"], &[], 1).await;
        assert_eq!(reason, crate::TurnEndReason::RoundCap, "{wire}");
        assert_eq!(requests.len(), 1, "{wire}");
    }
}
