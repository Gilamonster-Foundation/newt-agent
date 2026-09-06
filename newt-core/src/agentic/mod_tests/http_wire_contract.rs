use super::*;

/// Records whether every inference request preserves normal multi-turn wire
/// ordering while also carrying the protected recovery copy near the system
/// prompt.
struct ActivePromptWireOrderResponder {
    exact_task: &'static str,
    requests: Arc<AtomicUsize>,
    order_valid: Arc<AtomicBool>,
}

impl Respond for ActivePromptWireOrderResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let card_index = messages.iter().position(|message| {
            message["role"].as_str() == Some("system")
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(prompt_read::ACTIVE_PROMPT_PREFIX))
        });
        let protected_copy_is_earlier = card_index.is_some_and(|index| {
            index + 1 < messages.len().saturating_sub(1)
                && messages[index + 1]["role"].as_str() == Some("user")
                && messages[index + 1]["content"].as_str() == Some(self.exact_task)
        });
        let live_task_is_newest = messages.last().is_some_and(|message| {
            message["role"].as_str() == Some("user")
                && message["content"].as_str() == Some(self.exact_task)
        });
        let exact_task_copies = messages
            .iter()
            .filter(|message| {
                message["role"].as_str() == Some("user")
                    && message["content"].as_str() == Some(self.exact_task)
            })
            .count();
        self.order_valid.fetch_and(
            protected_copy_is_earlier && live_task_is_newest && exact_task_copies == 2,
            Ordering::SeqCst,
        );
        self.requests.fetch_add(1, Ordering::SeqCst);

        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": "wire order preserved"},
                "done": true,
                "prompt_eval_count": 20,
                "eval_count": 4
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "wire order preserved"},
                "prompt_eval_count": 20,
                "eval_count": 4
            }))
        }
    }
}

#[tokio::test]
async fn multi_turn_wire_keeps_live_operator_task_newest_after_protected_copy() {
    const CURRENT_TASK: &str = "CURRENT OPERATOR TASK: report the exact wire ordering";
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let order_valid = Arc::new(AtomicBool::new(true));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ActivePromptWireOrderResponder {
            exact_task: CURRENT_TASK,
            requests: requests.clone(),
            order_valid: order_valid.clone(),
        })
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("an older operator request"),
        MemMessage::assistant("the older request is complete"),
        MemMessage::user(CURRENT_TASK),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.task = CURRENT_TASK;
    let (reply, streamed, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("ordinary multi-turn request should complete");

    assert_eq!(reply, "wire order preserved");
    assert!(streamed);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        2,
        "both the probe and streaming reissue reached the wire"
    );
    assert!(
        order_valid.load(Ordering::SeqCst),
        "every request must keep the protected copy earlier and the live task newest"
    );
}

// -----------------------------------------------------------------------
// OpenAI-path coverage
// -----------------------------------------------------------------------

/// Large MCP surface from the field transcript: 169 remote definitions before
/// Newt's built-ins. None is invoked; this fixture grounds the request-envelope
/// regression without a live MCP server.
struct ManyToolsMcp {
    count: usize,
}

#[async_trait::async_trait]
impl McpTools for ManyToolsMcp {
    fn handles(&self, _name: &str) -> bool {
        false
    }

    fn tool_defs(&self) -> Vec<serde_json::Value> {
        (0..self.count)
            .map(|index| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": format!("field_mcp__tool_{index:03}"),
                        "description": "Synthetic remote tool for the provider-envelope regression.",
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect()
    }

    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        "unexpected MCP call".to_string()
    }
}

/// Simulates an OpenAI-compatible gateway that rejects more than 128 function
/// tools with the same opaque invalid_argument shape seen in the transcript.
struct OpenAiToolEnvelopeResponder {
    max_seen: Arc<AtomicUsize>,
    kernel_seen: Arc<AtomicBool>,
}

impl Respond for OpenAiToolEnvelopeResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let tools = body["tools"].as_array().cloned().unwrap_or_default();
        self.max_seen.fetch_max(tools.len(), Ordering::SeqCst);
        let names = tools
            .iter()
            .filter_map(|tool| {
                tool.pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
            })
            .collect::<Vec<_>>();
        self.kernel_seen.store(
            names.contains(&"run_command") && names.contains(&"tool_search"),
            Ordering::SeqCst,
        );
        if tools.len() > crate::agentic::tools::exposure::OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "code": "invalid_argument",
                    "message": "an internal error occurred",
                    "type": "invalid_request_error"
                }
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "tool envelope accepted"}}]
        }))
    }
}

#[tokio::test]
async fn openai_large_mcp_catalog_stays_within_wire_contract_and_keeps_shell() {
    let server = MockServer::start().await;
    let max_seen = Arc::new(AtomicUsize::new(0));
    let kernel_seen = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiToolEnvelopeResponder {
            max_seen: max_seen.clone(),
            kernel_seen: kernel_seen.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = "test \"gh auth status\" now";
    let mut mcp = ManyToolsMcp { count: 169 };

    let (reply, _, _, _) = chat_complete(c, &mut mcp)
        .await
        .expect("the provider-compatible request should reach inference");

    assert_eq!(reply, "tool envelope accepted");
    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        crate::agentic::tools::exposure::OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS
    );
    assert!(
        kernel_seen.load(Ordering::SeqCst),
        "run_command and tool_search must survive optional MCP clipping"
    );
}

#[tokio::test]
async fn chat_complete_dispatches_openai_kind_and_returns_first_round_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "openai says hi"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4},
        })))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.api_key = Some("sk-test");
    // Calling chat_complete (not openai_chat_complete) pins the dispatch.
    let (reply, streamed, usage, hallu) = chat_complete(c, &mut NoMcp)
        .await
        .expect("openai dispatch should succeed");

    assert_eq!(reply, "openai says hi");
    assert!(!streamed, "openai path is non-streaming");
    let u = usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (10, 4));
    assert_eq!(hallu, 0);
}

#[test]
fn inference_endpoint_locality_distinguishes_hosted_from_home_lab_names() {
    assert!(!inference_endpoint_is_owned("https://api.moonshot.ai"));
    assert!(inference_endpoint_is_owned("http://dgx1.home.arpa:8080"));
    assert!(inference_endpoint_is_owned("http://dgx1:8000"));
    assert!(inference_endpoint_is_owned("http://127.0.0.1:8000"));
    assert!(inference_endpoint_is_owned("http://[fd00::1]:8000"));
    assert!(inference_endpoint_is_owned("http://[fe80::1]:8000"));
}

#[test]
fn openai_progress_label_exposes_attempt_and_deadline() {
    assert_eq!(
        inference_progress_label("kimi-k3", 2, 2, 120),
        "waiting for kimi-k3 · attempt 2/2 · 120s deadline…"
    );
}

/// Mirrors vLLM chat templates that accept exactly one system message and
/// require it to be the first message in the request.
struct StrictVllmSystemResponder;

impl Respond for StrictVllmSystemResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body = body_json(req);
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let system_messages: Vec<(usize, &serde_json::Value)> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message["role"].as_str() == Some("system"))
            .collect();
        let valid_system = system_messages.len() == 1
            && system_messages[0].0 == 0
            && system_messages[0].1["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("you are a test")
                        && content.contains(prompt_read::ACTIVE_PROMPT_PREFIX)
                });
        let expected_tail = [
            ("user", "hello"),
            ("user", "an earlier request"),
            ("assistant", "the earlier request is complete"),
            ("user", "hello"),
        ];
        let valid_tail = messages.get(1..).is_some_and(|tail| {
            tail.len() == expected_tail.len()
                && tail
                    .iter()
                    .zip(expected_tail)
                    .all(|(message, (role, content))| {
                        message["role"].as_str() == Some(role)
                            && message["content"].as_str() == Some(content)
                    })
        });

        if !valid_system || !valid_tail {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "message": "System message must be at the beginning.",
                    "type": "BadRequestError"
                }
            }));
        }

        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "strict vllm accepted the request"}}]
        }))
    }
}

#[tokio::test]
async fn openai_chat_coalesces_system_cards_for_strict_vllm_templates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(StrictVllmSystemResponder)
        .mount(&server)
        .await;

    let messages = vec![
        MemMessage::system("you are a test"),
        MemMessage::user("an earlier request"),
        MemMessage::assistant("the earlier request is complete"),
        MemMessage::user("hello"),
    ];
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.task = "hello";

    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("strict vLLM chat template should accept the next message after a backend switch");

    assert_eq!(reply, "strict vllm accepted the request");
}

#[test]
fn openai_chat_rejects_system_messages_after_conversation_history() {
    let messages = vec![
        serde_json::json!({"role": "system", "content": "base policy"}),
        serde_json::json!({"role": "user", "content": "hello"}),
        serde_json::json!({"role": "system", "content": "late policy"}),
    ];

    let error = openai_chat_wire_messages(&messages)
        .expect_err("a late system message must not be promoted across user history");

    assert_eq!(
        error.to_string(),
        "invalid OpenAI chat message order: system messages must precede conversation history"
    );
}

#[tokio::test]
async fn openai_chat_projects_cognition_only_for_an_explicitly_capable_endpoint() {
    let server = MockServer::start().await;
    let request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureOpenAiRequestResponder {
            request: request.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.cognition = Some(crate::role_profile::Cognition::Deliberating);
    c.chat_completions_capability = crate::model_card::ChatCompletionsCapability {
        cognition: Some(true),
        chat_template_kwargs: Some(true),
        parallel_tool_calls: Some(false),
        bounded_reasoning_continuation: Some(true),
    };

    chat_complete(c, &mut NoMcp)
        .await
        .expect("capable OpenAI-compatible dispatch succeeds");

    let request = request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("request captured");
    assert_eq!(request["max_tokens"], 10_000);
    assert_eq!(request["temperature"], 0.6);
    assert_eq!(request["top_p"], 0.95);
    assert_eq!(request["parallel_tool_calls"], false);
    assert_eq!(request["chat_template_kwargs"]["enable_thinking"], true);
    assert_eq!(
        request["chat_template_kwargs"]["truncate_history_thinking"],
        true
    );
}

#[tokio::test]
async fn openai_chat_omits_local_cognition_fields_for_an_unknown_endpoint() {
    let server = MockServer::start().await;
    let request = Arc::new(Mutex::new(None));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(CaptureOpenAiRequestResponder {
            request: request.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.cognition = Some(crate::role_profile::Cognition::Contemplating);

    chat_complete(c, &mut NoMcp)
        .await
        .expect("strict-compatible dispatch succeeds");

    let request = request
        .lock()
        .expect("capture lock")
        .clone()
        .expect("request captured");
    for field in [
        "max_tokens",
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "chat_template_kwargs",
    ] {
        assert!(
            request.get(field).is_none(),
            "unknown endpoints must not receive `{field}`"
        );
    }
}
