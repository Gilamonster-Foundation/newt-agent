use super::*;

#[tokio::test]
async fn openai_strips_inline_think_and_never_returns_reasoning_content() {
    // #857: a reasoning model served with the parser OFF puts its CoT inline as
    // <think>…</think> in content; served with the parser ON it lands in a
    // separate reasoning_content field. Either way the returned answer must be
    // ONLY the clean content — no <think> markers, no CoT, no reasoning_content.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "content": "<think>secret chain of thought</think>The final answer.",
                "reasoning_content": "separate-channel reasoning"
            }}]
        })))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let (reply, _streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai dispatch should succeed");

    assert_eq!(reply, "The final answer.", "answer is the stripped content");
    assert!(!reply.contains("<think>"), "no think markers: {reply}");
    assert!(
        !reply.contains("secret chain of thought"),
        "inline CoT must not leak: {reply}"
    );
    assert!(
        !reply.contains("separate-channel reasoning"),
        "reasoning_content must not leak into the reply: {reply}"
    );
}

struct OpenAiReasoningReplayResponder {
    round: AtomicUsize,
    second_request: Arc<Mutex<Option<serde_json::Value>>>,
}

impl Respond for OpenAiReasoningReplayResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if self.round.fetch_add(1, Ordering::SeqCst) == 0 {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "content": "<think>inspect the first result before continuing</think>",
                    "reasoning_content": "read the first result, then choose the next action",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "definitely_not_a_real_tool",
                            "arguments": "{}"
                        }
                    }]
                }}]
            }));
        }

        *self.second_request.lock().expect("capture lock") = Some(body_json(req));
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {
                "role": "assistant",
                "content": "finished after the tool result"
            }}]
        }))
    }
}

#[tokio::test]
async fn openai_replays_reasoning_content_within_the_current_user_turn() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningReplayResponder {
            round: AtomicUsize::new(0),
            second_request: second_request.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("two-round OpenAI dispatch succeeds");

    assert_eq!(reply, "finished after the tool result");
    let request = second_request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("second request captured");
    let replayed_messages = request["messages"].as_array().expect("messages array");
    let replayed_index = replayed_messages
        .iter()
        .position(|message| {
            message["role"] == "assistant"
                && message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
        })
        .expect("assistant tool-call message replayed");
    let replayed_assistant = &replayed_messages[replayed_index];
    assert_eq!(
        replayed_assistant["reasoning_content"],
        "read the first result, then choose the next action"
    );
    assert_eq!(
        replayed_assistant["content"],
        "<think>inspect the first result before continuing</think>"
    );

    assert_eq!(replayed_messages[replayed_index + 1]["role"], "tool");
}

#[tokio::test]
async fn openai_default_scope_redacts_reasoning_from_tool_replay() {
    let server = MockServer::start().await;
    let second_request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiReasoningReplayResponder {
            round: AtomicUsize::new(0),
            second_request: second_request.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;

    chat_complete(c, &mut NoMcp)
        .await
        .expect("default OpenAI dispatch succeeds");

    let request = second_request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("second request captured");
    let replayed_assistant = request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| {
            message["role"] == "assistant"
                && message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
        })
        .expect("assistant tool-call message replayed");
    assert_eq!(replayed_assistant["content"], "");
    assert!(replayed_assistant.get("reasoning_content").is_none());
}

#[tokio::test]
async fn openai_current_turn_scope_redacts_inline_reasoning_from_restored_history() {
    let server = MockServer::start().await;
    let request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureOpenAiRequestResponder {
            request: request.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("an earlier task"),
        MemMessage::assistant("<think>private old plan</think>visible old answer"),
        MemMessage::user("do the thing"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.reasoning_replay_scope = crate::model_card::ReasoningReplayScope::CurrentUserTurn;

    chat_complete(c, &mut NoMcp)
        .await
        .expect("restored-history dispatch succeeds");

    let request = request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("request captured");
    let old_assistant = request["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("restored assistant message present");
    assert_eq!(old_assistant["content"], "visible old answer");
    assert!(!request.to_string().contains("private old plan"));
}

#[test]
fn openai_current_turn_scope_strips_reasoning_from_an_older_turn() {
    let message = serde_json::json!({
        "role": "assistant",
        "content": "<think>old inline plan</think>visible answer",
        "reasoning_content": "old split plan",
        "tool_calls": [{
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        }]
    });

    let replay = prepare_openai_assistant_replay(
        &message,
        "visible answer",
        crate::model_card::ReasoningReplayScope::CurrentUserTurn,
        false,
    );

    assert_eq!(replay["content"], "visible answer");
    assert!(replay.get("reasoning_content").is_none());
    assert_eq!(replay["tool_calls"], message["tool_calls"]);
}
