use super::*;

/// Mock MCP that handles exactly one namespaced tool for routing tests.
struct OneToolMcp {
    name: &'static str,
    result: &'static str,
    calls: usize,
}
#[async_trait::async_trait]
impl McpTools for OneToolMcp {
    fn handles(&self, name: &str) -> bool {
        name == self.name
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        self.calls += 1;
        self.result.to_string()
    }
}

/// An API proxy may put Anthropic-native tool-use blocks
/// (`{"name":"…","input":{}}`) inside the OpenAI `tool_calls` array
/// instead of converting them to `{"function":{"name":"…","arguments":"…"}}`.
/// The loop must detect the missing `function` key, fall back to the
/// Anthropic-native fields, and route the call correctly.
#[tokio::test]
async fn openai_anthropic_native_tool_calls_route_correctly() {
    let server = MockServer::start().await;
    // Round 1: Anthropic-native tool-use block in the tool_calls array.
    // Round 2: plain text final answer after receiving the tool result.
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cc = call_count.clone();
    let replay = DisplayReplay::default();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            if replay.take(req) {
                return sse_replay("done after anthropic-native tool");
            }
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "tool_calls": [{
                            "type": "tool_use",
                            "id": "toolu_01ABC",
                            "name": "my_server__my_tool",
                            "input": {"key": "value"}
                        }]
                    }}],
                    "usage": {"prompt_tokens": 50, "completion_tokens": 10}
                }))
            } else {
                replay.arm(req);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "done after anthropic-native tool"}}],
                    "usage": {"prompt_tokens": 60, "completion_tokens": 8}
                }))
            }
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let allowed_tools = ["my_server__my_tool".to_string()];
    c.persona_tools = Some(&allowed_tools);
    let mut mcp = OneToolMcp {
        name: "my_server__my_tool",
        result: "tool-result-text",
        calls: 0,
    };
    let (reply, _, _, hallu) = chat_complete(c, &mut mcp)
        .await
        .expect("should succeed with anthropic-native tool format");
    assert_eq!(reply, "done after anthropic-native tool");
    assert_eq!(
        mcp.calls, 1,
        "the granted MCP tool must actually execute once"
    );
    assert_eq!(hallu, 0, "should not be counted as hallucination");
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "must have done both rounds (tool call + final answer)"
    );
}

/// Regression: some OpenAI-compatible API proxies normalise hyphens to
/// underscores in tool names (`acme-server` → `acme_server`).  Verify that
/// the underscore form routes through MCP rather than falling to "unknown tool".
#[tokio::test]
async fn openai_hyphenated_server_name_routes_through_mcp() {
    let server = MockServer::start().await;
    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cc = call_count.clone();
    let replay = DisplayReplay::default();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &Request| {
            if replay.take(req) {
                return sse_replay("outlook routed correctly");
            }
            let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                // Proxy returns the underscore-normalised form of the
                // server prefix even though we advertised hyphens.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": "",
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "{}",
                                "name": "acme_server__probe_tool"
                            },
                            "id": "call_probe_01",
                            "type": "function"
                        }]
                    }}],
                    "usage": {"prompt_tokens": 599, "completion_tokens": 30}
                }))
            } else {
                replay.arm(req);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "outlook routed correctly"}}]
                }))
            }
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let allowed_tools = ["acme_server__probe_tool".to_string()];
    c.persona_tools = Some(&allowed_tools);
    // OneToolMcp.handles() must match the underscore form the proxy returns.
    let mut mcp = OneToolMcp {
        name: "acme_server__probe_tool",
        result: "ok",
        calls: 0,
    };
    let (reply, _, _, _) = chat_complete(c, &mut mcp)
        .await
        .expect("should route hyphenated server name through mcp");
    assert_eq!(reply, "outlook routed correctly");
    assert_eq!(
        mcp.calls, 1,
        "the granted MCP tool must actually execute once"
    );
    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "must have completed both rounds"
    );
}
