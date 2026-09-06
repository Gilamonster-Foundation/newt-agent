use super::*;

/// A `recover_cw_400` hook for the chat-path cw-400 recovery tests. It
/// parses nothing — it unconditionally reports a roomy recovered input cap
/// so the loop's compress-and-retry path fires: the small test history
/// easily fits the recovered budget, so compaction does not refuse and the
/// SAME logical round is retried in place (#1528, chat-path parity).
fn recover_cw_400_to_40k(_e: &anyhow::Error, _model: &str, _today: &str) -> Option<u32> {
    Some(40_000)
}

/// Serves, in order: a numbered context-window 400, then a real tool round
/// (`get_context_remaining` — executed synthetically in-loop, no side
/// effect), then a plain-text final answer. OpenAI `choices[0].message`
/// shape.
struct OpenAiOverflowThenToolThenDone {
    served: Arc<AtomicUsize>,
}
impl Respond for OpenAiOverflowThenToolThenDone {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        match self.served.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
            })),
            1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "get_context_remaining", "arguments": "{}"}
                    }]
                }}]
            })),
            _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "done"}}]
            })),
        }
    }
}

#[tokio::test]
async fn openai_chat_cw_400_recovery_retries_the_same_logical_round_with_tools() {
    // #1528 (chat-path parity): a cw-400 must retry the SAME logical tool
    // round in place, not advance the round counter. With max_tool_rounds ==
    // 1 the buggy `continue 'round_loop` consumed the only round on recovery
    // and demoted the recovered request to the tools-disabled summary — 2
    // requests: [400, summary]. The fix dispatches a real recovered TOOL
    // round (still carrying tools); only a COMPLETED round then advances to
    // the summary — 3 requests: [400, recovered tool round, summary].
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiOverflowThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "SAME ROUND: recovery must not burn the only tool round";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_cw_400_to_40k);
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect("recovery retries the round in place and the turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [400, recovered tool round, summary]; a 2-request run means \
             recovery burned the only round and demoted to the tools-disabled summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(1)["tools"].is_array(),
        "the RECOVERED request must still carry tools — a real tool round, not the summary"
    );
    assert!(
        body(2)["tools"].is_null(),
        "only the final summary (after the completed round) is tools-disabled"
    );
}

#[tokio::test]
async fn openai_chat_cw_400_recovery_is_bounded() {
    // #1526 review: the chat-transport cw-400 bound (`cw_retries < 2`) has the
    // same exhaustion guarantee the Responses loop proves — a server that 400s
    // every time surfaces the error after at most initial + 2 recoveries, never
    // looping forever. max_tool_rounds == 1 so the bound proven is the INNER
    // `cw_retries` cap (recovery retries in place), not the outer round cap.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
        })))
        .expect(3) // initial + exactly 2 bounded recoveries
        .mount(&server)
        .await;

    let task = "BOUNDED (chat): never loop forever on a persistent 400";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_cw_400_to_40k);
    ctx.max_tool_rounds = 1;

    openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a persistent chat cw-400 surfaces after the bounded retries");
    // `.expect(3)` verified on drop: initial + exactly 2 recoveries.
}

/// Ollama `message` shape of [`OpenAiOverflowThenToolThenDone`]: a numbered
/// context-window 400, then a `get_context_remaining` tool round, then a
/// plain-text final answer.
struct OllamaOverflowThenToolThenDone {
    served: Arc<AtomicUsize>,
}
impl Respond for OllamaOverflowThenToolThenDone {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        match self.served.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"message": "prompt is too long: 999999 tokens > 40000 maximum"}
            })),
            1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [{"function": {
                        "name": "get_context_remaining", "arguments": {}
                    }}]
                },
                "prompt_eval_count": 10, "eval_count": 1
            })),
            _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "done"}
            })),
        }
    }
}

#[tokio::test]
async fn ollama_chat_cw_400_recovery_retries_the_same_logical_round_with_tools() {
    // #1528 (chat-path parity, Ollama loop): identical intent to the
    // OpenAI-chat case. With max_tool_rounds == 1 the buggy
    // `continue 'round_loop` consumed the only round and demoted the
    // recovered request to the tools-disabled summary — 2 requests. The fix
    // retries the SAME round in place (still WITH tools); only a completed
    // round advances to the summary — 3 requests: [400, recovered tool
    // round, summary].
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaOverflowThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "SAME ROUND: Ollama recovery must not burn the only tool round";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.recover_cw_400 = Some(recover_cw_400_to_40k);
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("recovery retries the round in place and the turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [400, recovered tool round, summary]; a 2-request run means \
             recovery burned the only round and demoted to the tools-disabled summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(1)["tools"].is_array(),
        "the RECOVERED request must still carry tools — a real tool round, not the summary"
    );
    assert!(
        body(2)["tools"].is_null(),
        "only the final summary (after the completed round) is tools-disabled"
    );
}

#[tokio::test]
async fn ollama_chat_malformed_xml_retries_the_same_logical_round_with_tools() {
    // #1533 review: the malformed-XML tool-call recovery appends a corrective
    // nudge and re-dispatches — it must retry the SAME round in place, else at
    // max_tool_rounds == 1 the nudge only ever reaches the tools-disabled
    // summary. Buggy `continue 'round_loop` burned the round → 2 requests +
    // cap-exit. Fixed → 3 requests: [xml error, nudged tool round, summary].
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct XmlThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for XmlThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "ollama xml syntax error in the generated tool call"}
                })),
                1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "",
                        "tool_calls": [{"function": {
                            "name": "get_context_remaining", "arguments": {}}}]},
                    "prompt_eval_count": 10, "eval_count": 1
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "done"}
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(XmlThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "MALFORMED XML: the nudge must reach a tool-capable round";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    // recover_cw_400 stays None: an XML syntax error must NOT be mistaken for
    // a context-window 400 and recovered down the cw-400 path.
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("malformed-XML recovery retries the round and the turn completes");
    assert_eq!(reply, "done");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [xml error, nudged tool round, summary]; a 2-request run means \
             the nudge burned the only tool round and reached only the summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(1)["tools"].is_array(),
        "the nudged retry must still carry tools — a real tool round"
    );
    let req1 = serde_json::to_string(&body(1)).unwrap_or_default();
    assert!(
        req1.contains("failed inside Ollama's XML tool-call parser"),
        "the recovered request must carry the corrective XML nudge"
    );
    assert!(
        body(2)["tools"].is_null(),
        "only the final summary is tools-disabled"
    );
}

#[tokio::test]
async fn ollama_chat_malformed_xml_is_bounded_to_two_nudges() {
    // Persistent malformed-XML errors are bounded to the configured 2 nudges
    // (`ollama_xml_retry_nudges < 2`); after that the error surfaces. Exactly
    // 1 + 2 dispatches, then Err — no unbounded in-round loop.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {"message": "ollama xml syntax error in the generated tool call"}
        })))
        .expect(3)
        .mount(&server)
        .await;

    let task = "BOUNDED XML: never loop forever on a persistent parser error";
    let messages = vec![MemMessage::system("base policy"), MemMessage::user(task)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    // No recover_cw_400: the XML error must fall through to a terminal error,
    // not be laundered into a cw-400 recovery.
    ctx.max_tool_rounds = 1;

    chat_complete(ctx, &mut NoMcp)
        .await
        .expect_err("a persistent malformed-XML error surfaces after the bounded nudges");
    // `.expect(3)` verified on drop: initial + exactly 2 nudged retries.
}

#[tokio::test]
async fn ollama_chat_tools_unsupported_recovers_in_the_same_round() {
    // #1533 review: unsupported-tools recovery must retry the SAME round with
    // tools dropped, returning the model's answer DIRECTLY — not burn the
    // round into the tools-disabled cap summary. req2 returns a tool call so
    // the recovered round is provably tool-processing: fixed → 3 requests
    // (tool executes, then summary); buggy `continue 'round_loop` → 2 requests
    // (the burned round's summary can't use the tool call → cap-exit).
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct UnsupportedThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UnsupportedThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "this model does not support tools"}
                })),
                1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "",
                        "tool_calls": [{"function": {
                            "name": "get_context_remaining", "arguments": {}}}]},
                    "prompt_eval_count": 10, "eval_count": 1
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered directly"}
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(UnsupportedThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "TOOLS UNSUPPORTED: retry the same round, don't burn it";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Ollama);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("unsupported-tools recovery retries the same round");
    assert_eq!(reply, "recovered directly");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [tools-unsupported, recovered tool round, summary]; a 2-request \
             run means the round was burned into the tools-disabled cap summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(0)["tools"].is_array(),
        "the first request advertised tools"
    );
    assert!(
        body(1)["tools"].is_null(),
        "the recovered same-round request drops tools"
    );
}

#[tokio::test]
async fn openai_chat_tools_unsupported_recovers_in_the_same_round() {
    // #1533 review: OpenAI-chat unsupported-tools recovery — same as the Ollama
    // case, additionally proving the recovered request drops BOTH `tools` and
    // `tool_choice`.
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    struct UnsupportedThenToolThenDone {
        served: Arc<AtomicUsize>,
    }
    impl Respond for UnsupportedThenToolThenDone {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            match self.served.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"message": "this model does not support tools"}
                })),
                1 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": null,
                        "tool_calls": [{"id": "c1", "type": "function",
                            "function": {"name": "get_context_remaining", "arguments": "{}"}}]}}]
                })),
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "recovered directly"}}]
                })),
            }
        }
    }
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(UnsupportedThenToolThenDone {
            served: served.clone(),
        })
        .mount(&server)
        .await;

    let task = "TOOLS UNSUPPORTED (openai): retry the same round, don't burn it";
    let messages = vec![
        MemMessage::system("base policy"),
        MemMessage::user("historical A"),
        MemMessage::assistant("A done"),
        MemMessage::user(task),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(&uri, &messages, &caveats, task, BackendKind::Openai);
    ctx.safe_context = None;
    ctx.max_ok_input = None;
    ctx.num_ctx = None;
    ctx.max_tool_rounds = 1;

    let (reply, _, _, _) = openai_chat_complete(ctx, &mut NoMcp)
        .await
        .expect("unsupported-tools recovery retries the same round");
    assert_eq!(reply, "recovered directly");

    let reqs = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        reqs.len(),
        3,
        "expected [tools-unsupported, recovered tool round, summary]; a 2-request \
             run means the round was burned into the tools-disabled cap summary"
    );
    let body = |i: usize| -> serde_json::Value {
        serde_json::from_slice(&reqs[i].body).unwrap_or_default()
    };
    assert!(
        body(0)["tools"].is_array(),
        "the first request advertised tools"
    );
    assert!(
        body(1)["tools"].is_null() && body(1)["tool_choice"].is_null(),
        "the recovered same-round request drops BOTH tools and tool_choice"
    );
}
