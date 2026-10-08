//! #2824 supersedes automatic thinking-off recovery: output caps hand off to the operator.
use super::*;
use crate::role_profile::Cognition;

#[tokio::test]
async fn cognition_preferences_cannot_purchase_a_capped_generation_retry() {
    for level in [None, Some(Cognition::Zen), Some(Cognition::Thoughtful)] {
        for preference in [
            crate::config::OverflowRetry::Off,
            crate::config::OverflowRetry::ThinkingOff,
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"choices":[{"finish_reason":"length","message":{
                    "content":null,"reasoning_content":"private unfinished reasoning"}}],
                    "usage":{"prompt_tokens":20,"completion_tokens":8}}),
                ))
                .mount(&server)
                .await;
            let messages = msgs();
            let caveats = Caveats::top();
            let uri = server.uri();
            let mut c = ctx(&uri, &messages, &caveats);
            c.kind = BackendKind::Openai;
            c.cognition = level;
            c.overflow_retry = preference;
            c.chat_completions_capability.cognition = Some(true);
            c.chat_completions_capability.chat_template_kwargs = Some(true);
            let (reply, _, usage, _) = chat_complete(c, &mut NoMcp).await.unwrap();
            assert!(
                reply.contains("output limit") && reply.contains("operator"),
                "{reply}"
            );
            assert!(!reply.contains("private unfinished reasoning"));
            assert_eq!(usage.unwrap().output_tokens, 8);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
}

/// A new operator turn may continue, retaining the original cognition policy.
#[tokio::test]
async fn only_a_new_operator_turn_continues_after_output_exhaustion() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            let first = seen.fetch_add(1, Ordering::SeqCst) == 0;
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"finish_reason":if first {"length"} else {"stop"},
                    "message":{"content":if first {"partial"} else {"finished"}}}]
            }))
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    for turn in 0..2 {
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.action_nudges = false;
        c.cognition = Some(Cognition::Thoughtful);
        c.chat_completions_capability.cognition = Some(true);
        c.chat_completions_capability.chat_template_kwargs = Some(true);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), turn + 1);
        if turn == 0 {
            assert!(reply.contains("truncated"));
        } else {
            assert_eq!(reply, "finished");
        }
    }
    let requests = server.received_requests().await.unwrap();
    for req in requests {
        assert_eq!(
            body_json(&req)["chat_template_kwargs"]["enable_thinking"],
            true
        );
    }
}
