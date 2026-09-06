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
