use super::*;

#[test]
fn detects_tools_unsupported_400_phrasings() {
    assert!(is_tools_unsupported_error(&anyhow::anyhow!(
        "Ollama 400 Bad Request: registry.ollama.ai/library/deepseek-r1:70b does not support tools"
    )));
    // Looser OpenAI-compatible phrasing.
    assert!(is_tools_unsupported_error(&anyhow::anyhow!(
        "this model does not support tools at this time"
    )));
    // Unrelated 400s must NOT trip the no-tools path.
    assert!(!is_tools_unsupported_error(&anyhow::anyhow!(
        "Ollama 400 Bad Request: context window exceeded"
    )));
}

#[test]
fn detects_ollama_tool_xml_parser_errors() {
    assert!(is_ollama_tool_xml_error(&anyhow::anyhow!(
        "{}",
        r#"Ollama 500 Internal Server Error: {"error":"XML syntax error on line 7: element \u003cparameter\u003e closed by \u003c/function\u003e"}"#
    )));
    assert!(is_ollama_tool_xml_error(&anyhow::anyhow!(
        "{}",
        r#"Ollama 500 Internal Server Error: {"error":"XML syntax error on line 2: element <parameter> closed by </function>"}"#
    )));
    assert!(is_ollama_tool_xml_error(&anyhow::anyhow!(
        "{}",
        r#"Ollama 500 Internal Server Error: {"error":"XML syntax error on line 3: unexpected end element \u003c/parameter\u003e"}"#
    )));
    assert!(!is_ollama_tool_xml_error(&anyhow::anyhow!(
        "Ollama 500 Internal Server Error: model runner crashed"
    )));
    assert!(!is_ollama_tool_xml_error(&anyhow::anyhow!(
        "OpenAI 400 Bad Request: XML syntax error in user supplied file"
    )));
}

/// A model that rejects the `tools` field (deepseek-r1) 400s on the first
/// dispatch; newt must drop tools and re-dispatch, answering normally. The
/// tools-absent retry is the one that succeeds — no tools-400 loop.
struct NoToolsResponder {
    rejections: Arc<AtomicUsize>,
    served_without_tools: Arc<AtomicBool>,
}
impl Respond for NoToolsResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let has_tools = body_json(req).get("tools").is_some();
        if has_tools {
            self.rejections.fetch_add(1, Ordering::SeqCst);
            return ResponseTemplate::new(400).set_body_string(
                "registry.ollama.ai/library/deepseek-r1:70b does not support tools",
            );
        }
        self.served_without_tools.store(true, Ordering::SeqCst);
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": "hello there"}, "done": true,
                "prompt_eval_count": 4, "eval_count": 2
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "probe answer"},
                "prompt_eval_count": 4, "eval_count": 2,
            }))
        }
    }
}

#[tokio::test]
async fn no_tools_model_recovers_by_dropping_tools() {
    let server = MockServer::start().await;
    let rejections = Arc::new(AtomicUsize::new(0));
    let served_without_tools = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(NoToolsResponder {
            rejections: rejections.clone(),
            served_without_tools: served_without_tools.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("a no-tools model still answers a bare prompt");

    assert_eq!(reply, "hello there", "the tools-absent retry answered");
    assert!(streamed);
    assert!(
        served_without_tools.load(Ordering::SeqCst),
        "a request without the tools field was eventually served"
    );
    assert_eq!(
        rejections.load(Ordering::SeqCst),
        1,
        "exactly one tools-bearing request 400s — the drop is self-limiting"
    );
}

/// Ollama can 500 before returning assistant content when its XML parser
/// sees malformed Qwen-style tool-call tags. That is not the same as
/// "model does not support tools": Newt should retry with tools still
/// advertised so the model can make forward progress on the next round.
struct MalformedToolXmlResponder {
    rejections: Arc<AtomicUsize>,
    served_with_tools_after_error: Arc<AtomicBool>,
    served_without_tools: Arc<AtomicBool>,
}
impl Respond for MalformedToolXmlResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            if self.rejections.fetch_add(1, Ordering::SeqCst) == 0 {
                return ResponseTemplate::new(500).set_body_json(serde_json::json!({
                    "error": "XML syntax error on line 7: element <parameter> closed by </function>"
                }));
            }
            self.served_with_tools_after_error
                .store(true, Ordering::SeqCst);
            if is_stream(req) {
                return ndjson(&[serde_json::json!({
                    "message": {"content": "recovered with tools still available"},
                    "done": true,
                    "prompt_eval_count": 4,
                    "eval_count": 3
                })]);
            }
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "probe answer with tools still available"},
                "prompt_eval_count": 4,
                "eval_count": 3,
            }));
        }
        self.served_without_tools.store(true, Ordering::SeqCst);
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": "unexpected no-tools stream"},
                "done": true,
                "prompt_eval_count": 4, "eval_count": 3
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "unexpected no-tools probe"},
                "prompt_eval_count": 4, "eval_count": 3,
            }))
        }
    }
}

#[test]
fn malformed_tool_xml_responder_flags_unexpected_no_tools_request() {
    let rejections = Arc::new(AtomicUsize::new(0));
    let served_with_tools_after_error = Arc::new(AtomicBool::new(false));
    let served_without_tools = Arc::new(AtomicBool::new(false));
    let responder = MalformedToolXmlResponder {
        rejections: rejections.clone(),
        served_with_tools_after_error: served_with_tools_after_error.clone(),
        served_without_tools: served_without_tools.clone(),
    };
    let req = Request {
        url: "http://localhost/api/chat".parse().unwrap(),
        method: "POST".parse().unwrap(),
        headers: Default::default(),
        body: serde_json::json!({ "stream": false })
            .to_string()
            .into_bytes(),
    };

    let _response = responder.respond(&req);

    assert!(
        served_without_tools.load(Ordering::SeqCst),
        "the defensive no-tools branch should be observable"
    );
    assert_eq!(rejections.load(Ordering::SeqCst), 0);
    assert!(!served_with_tools_after_error.load(Ordering::SeqCst));
}

#[tokio::test]
async fn ollama_tool_xml_error_recovers_with_tools_still_available() {
    let server = MockServer::start().await;
    let rejections = Arc::new(AtomicUsize::new(0));
    let served_with_tools_after_error = Arc::new(AtomicBool::new(false));
    let served_without_tools = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(MalformedToolXmlResponder {
            rejections: rejections.clone(),
            served_with_tools_after_error: served_with_tools_after_error.clone(),
            served_without_tools: served_without_tools.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("malformed XML tool-call parser errors should retry with tools");

    assert_eq!(reply, "recovered with tools still available");
    assert!(streamed);
    assert!(
        served_with_tools_after_error.load(Ordering::SeqCst),
        "a tools-bearing request was served after the XML parser failure"
    );
    assert!(
        !served_without_tools.load(Ordering::SeqCst),
        "malformed XML must not disable tools for the turn"
    );
    assert_eq!(
        rejections.load(Ordering::SeqCst),
        3,
        "the XML error probe, retry probe, and streaming re-issue all keep tools advertised"
    );
}

/// Ground chained recovery in a real native-read result: an XML parser retry
/// keeps tools initially, but its action instruction cannot outlive tool loss.
#[tokio::test]
async fn confined_act_ollama_xml_retry_then_tool_loss_preserves_evidence_without_action_pressure() {
    let (workspace, caveats) = readonly_count_workspace();
    let original = caveats.clone();
    let server = MockServer::start().await;
    let request_count = Arc::new(AtomicUsize::new(0));
    let seen = request_count.clone();
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(
            move |_: &Request| match seen.fetch_add(1, Ordering::SeqCst) {
                0 => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"tool_calls": [{"function": {
                        "name": "read_file", "arguments": {"path": READONLY_COUNT_FILE}
                    }}]}, "done": true
                })),
                1 => ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": "XML syntax error on line 7: element <parameter> closed by </function>"
                })),
                2 => {
                    ResponseTemplate::new(400).set_body_string("this model does not support tools")
                }
                _ => ndjson(&[serde_json::json!({
                    "message": {"content": READONLY_COUNT_ANSWER}, "done": true
                })]),
            },
        )
        .mount(&server)
        .await;
    let messages = vec![MemMessage::user(READONLY_COUNT_TASK)];
    let uri = server.uri();
    let workspace_path = workspace.path().to_string_lossy();
    let mut events = Vec::new();
    let mut c = ctx(&uri, &messages, &caveats);
    c.workspace = &workspace_path;
    c.task = READONLY_COUNT_TASK;
    c.tool_events = Some(&mut events);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(reply, READONLY_COUNT_ANSWER);
    assert_eq!(events.len(), 1);
    assert!(events[0].tool == "read_file" && events[0].ok);
    assert_eq!(caveats, original);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join(READONLY_COUNT_FILE)).unwrap(),
        READONLY_COUNT_DATA
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        5,
        "read, XML failure, tools failure, answer and replay"
    );
    let xml_retry = body_json(&requests[2]);
    assert!(xml_retry.get("tools").is_some());
    assert!(
        xml_retry["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("exactly one valid native tool call"))
            }),
        "the first recovery must actually demand a native tool call: {xml_retry}"
    );
    for request in requests.iter().skip(3) {
        let body = body_json(request);
        assert!(body.get("tools").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert!(messages
            .iter()
            .any(|message| message["role"] == "user" && message["content"] == READONLY_COUNT_TASK));
        assert!(
            messages.iter().any(|message| message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("refs/heads/feature"))),
            "the actual read evidence must survive: {body}"
        );
        for message in messages.iter().filter(|message| message["role"] == "user") {
            assert!(
                !message["content"].to_string().contains("native tool call"),
                "XML action guidance survived tool loss: {message}"
            );
        }
    }
}

/// Reuse the existing two-stage empty-output fixture after tools become
/// unavailable. Both quality retries must remain useful without tool pressure.
#[tokio::test]
async fn confined_act_ollama_empty_retry_without_tools_never_demands_tool_calls() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    let rejections = Arc::new(AtomicUsize::new(0));
    let rejected = rejections.clone();
    let responder = super::stream_empty::SuspiciousEmptyTwiceThenRecover {
        probes: probes.clone(),
        saw_strong_nudge: Arc::new(AtomicBool::new(false)),
    };
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |request: &Request| {
            if body_json(request).get("tools").is_some() {
                rejected.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(400).set_body_string("this model does not support tools")
            } else {
                responder.respond(request)
            }
        })
        .mount(&server)
        .await;
    let messages = vec![MemMessage::user(READONLY_COUNT_TASK)];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.task = READONLY_COUNT_TASK;
    let (reply, streamed, _, _) = chat_complete(c, &mut NoMcp).await.unwrap();
    assert_eq!(reply, "recovered after strong hidden-only nudge");
    assert!(streamed);
    assert_eq!(rejections.load(Ordering::SeqCst), 1);
    assert_eq!(
        probes.load(Ordering::SeqCst),
        3,
        "two empty candidates then recovery"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        7,
        "one tools rejection and three probe/stream pairs"
    );
    for request in requests.iter().skip(1) {
        let body = body_json(request);
        assert!(body.get("tools").is_none());
        for message in body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
        {
            let text = message["content"].to_string();
            assert!(
                !text.contains("tool call"),
                "empty-output recovery demanded unavailable tools: {text}"
            );
        }
    }
}

#[test]
fn confined_act_append_nudge_preserves_operator_bytes_in_separate_tagged_notes() {
    let operator = "  Please count the branches.\n\tKeep this spacing — unchanged.\n";
    let original = vec![
        serde_json::json!({"role": "system", "content": "Follow the task."}),
        serde_json::json!({"role": "user", "content": operator}),
    ];
    let mut messages = original.clone();
    for correction in ["Recover the malformed call.", "Return visible content."] {
        append_nudge_line(&mut messages, correction);
        assert_eq!(&messages[..original.len()], original.as_slice());
        assert_eq!(
            messages.last().unwrap(),
            &serde_json::json!({
                "role": "user",
                "content": format!("{} {correction}", compress::LOOP_GUIDANCE_PREFIX)
            })
        );
    }
    assert_eq!(messages.len(), original.len() + 2);
    assert_eq!(
        messages[1]["content"].as_str().unwrap().as_bytes(),
        operator.as_bytes()
    );
}

#[test]
fn confined_act_tool_loss_cleanup_preserves_byte_identical_operator_collisions() {
    let correction = "Use the requested tool now.";
    let operator = format!("{} {correction}", compress::LOOP_GUIDANCE_PREFIX);
    let original = vec![
        serde_json::json!({"role": "system", "content": "System context."}),
        serde_json::json!({"role": "user", "content": operator}),
        serde_json::json!({
            "role": "assistant",
            "content": format!("{} Preserve this assistant evidence.", compress::LOOP_GUIDANCE_PREFIX)
        }),
        serde_json::json!({
            "role": "tool", "tool_call_id": "read-1",
            "content": format!("{} Preserve this tool evidence.", compress::LOOP_GUIDANCE_PREFIX)
        }),
    ];
    let protected = protected_operator_messages(&original);
    assert_eq!(protected, [operator.clone()]);
    let mut messages = original.clone();
    push_loop_guidance(&mut messages, correction);
    push_loop_guidance(&mut messages, "A distinct obsolete tool correction.");

    strip_unavailable_tool_guidance(&mut messages, &protected);

    // Equal strings are ambiguous: conservatively retain both rather than
    // inventing occurrence identity or deleting the original operator input.
    let mut expected = original;
    expected.push(serde_json::json!({"role": "user", "content": operator}));
    assert_eq!(messages, expected);
}
