use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn http_test_entry(url: String) -> McpServerEntry {
    McpServerEntry {
        enabled: true,
        name: "headless-http".into(),
        transport: TransportKind::Http,
        command: None,
        args: Vec::new(),
        env: BTreeMap::new(),
        url: Some(url),
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
    }
}

async fn mount_toolset_lifecycle(server: &MockServer, connections: u64) {
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("Mcp-Session-Id", "headless-session")
                .set_body_string(format!(
                    r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{PROTOCOL_VERSION}","capabilities":{{}},"serverInfo":{{"name":"headless-http","version":"1"}}}}}}"#
                )),
        )
        .expect(connections)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains(
            "\"method\":\"notifications/initialized\"",
        ))
        .respond_with(ResponseTemplate::new(202))
        .expect(connections)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/list\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"review","description":"","inputSchema":{"type":"object"}}]}}"#,
                ),
        )
        .expect(connections)
        .mount(server)
        .await;
}

#[derive(Clone)]
struct ExpireFirstSessionCall {
    calls: Arc<AtomicUsize>,
}

impl Respond for ExpireFirstSessionCall {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(404)
        } else {
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"recovered"}]}}"#,
                )
        }
    }
}

#[derive(Clone)]
struct ExpireThenRejectReplay {
    calls: Arc<AtomicUsize>,
}

impl Respond for ExpireThenRejectReplay {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(404),
            1 => ResponseTemplate::new(401),
            _ => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"recovered connection retained"}]}}"#,
                ),
        }
    }
}

#[test]
fn empty_toolset_has_no_tools_and_handles_nothing() {
    let toolset = McpToolset::empty();
    assert!(toolset.is_empty());
    assert!(toolset.tool_defs().is_empty());
    assert!(!toolset.handles("modulex__routine_run"));
    assert!(toolset.summary().is_empty());
}

#[test]
fn server_prefix_sanitizes_hyphens_when_enabled() {
    assert_eq!(server_prefix("my-server", true), "my_server");
    assert_eq!(server_prefix("my-server", false), "my-server");
}

#[test]
fn handles_matches_sanitized_prefix_only() {
    let toolset = McpToolset {
        servers: vec![ToolsetServer {
            live: ConnectedServer {
                name: "modulex".to_string(),
                conn: McpConnection::new(AnyTransport::Mock(MockTransport::new([]))),
                tools: vec![RemoteTool {
                    name: "routine_run".to_string(),
                    description: String::new(),
                    input_schema: json!({}),
                    meta: Some(json!({
                        "newt/resourceUrlPrefixes": ["https://review.example/resources/"]
                    })),
                }],
                sandbox_kind: None,
                net_posture: crate::NetPosture::Advisory,
                server_info: None,
                instructions: None,
            },
            http: None,
        }],
        sanitize_server_names: true,
    };
    assert!(toolset.handles("modulex__routine_run"));
    // `handles` matches the SERVER prefix only, not the specific tool
    // name — same as the TUI's `Mcp::handles` it's ported from. A
    // namespaced call for an unlisted tool on a connected server still
    // routes there; the server itself rejects an unknown tool name.
    assert!(toolset.handles("modulex__some_other_tool_on_the_same_server"));
    assert!(!toolset.handles("no_separator_here"));
    assert!(!toolset.handles("other_server__routine_run"));

    let defs = toolset.tool_defs();
    assert_eq!(defs.len(), 1);
    assert_eq!(
        defs[0]["function"]["name"],
        Value::String("modulex__routine_run".to_string())
    );
    assert_eq!(
        defs[0]["_meta"][newt_core::MCP_RESOURCE_URL_PREFIXES_META_KEY],
        json!(["https://review.example/resources/"])
    );
    assert_eq!(
        toolset.mcp_tool_list()[0]["_meta"][newt_core::MCP_RESOURCE_URL_PREFIXES_META_KEY],
        json!(["https://review.example/resources/"])
    );
}

#[test]
fn format_toolset_result_joins_text_and_flags_errors() {
    let r =
        json!({"content": [{"type": "text", "text": "hello"}, {"type": "text", "text": "world"}]});
    assert_eq!(format_toolset_result(&r), "hello\nworld");
    let err = json!({"content": [{"type":"text","text":"boom"}], "isError": true});
    assert_eq!(format_toolset_result(&err), "tool error: boom");
}

#[tokio::test]
async fn call_wraps_a_successful_result_as_untrusted_data() {
    let mut toolset = McpToolset {
        servers: vec![ToolsetServer {
            live: ConnectedServer {
                name: "modulex".to_string(),
                conn: McpConnection::new(AnyTransport::Mock(MockTransport::new([
                    r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"3 dirty trees"}]}}"#,
                ]))),
                tools: vec![],
                sandbox_kind: None,
                net_posture: crate::NetPosture::Advisory,
                server_info: None,
                instructions: None,
            },
            http: None,
        }],
        sanitize_server_names: true,
    };
    let out = toolset
        .call("modulex__routine_run", &json!({"routine": "morning"}))
        .await;
    assert!(out.starts_with("<untrusted-data source=\"modulex__routine_run\">"));
    assert!(out.contains("3 dirty trees"));
}

#[tokio::test]
async fn headless_call_does_not_reflect_remote_json_rpc_error_content() {
    let mut toolset = McpToolset {
        servers: vec![ToolsetServer {
            live: ConnectedServer {
                name: "modulex".to_string(),
                conn: McpConnection::new(AnyTransport::Mock(MockTransport::new([
                    r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"TOP-SECRET\u001b[31m\nforged log","data":{"token":"TOP-SECRET"}}}"#,
                ]))),
                tools: vec![],
                sandbox_kind: None,
                net_posture: crate::NetPosture::Advisory,
                server_info: None,
                instructions: None,
            },
            http: None,
        }],
        sanitize_server_names: true,
    };

    let output = toolset.call("modulex__review", &json!({})).await;
    assert_eq!(
        output,
        "error: MCP server error on `tools/call` (JSON-RPC code -32000)"
    );
    for forbidden in ["TOP-SECRET", "forged log", "\u{1b}", "\n", "\r"] {
        assert!(
            !output.contains(forbidden),
            "reflected {forbidden:?}: {output:?}"
        );
    }
}

#[tokio::test]
async fn headless_toolset_reconnects_and_replays_once_after_session_404() {
    let server = MockServer::start().await;
    mount_toolset_lifecycle(&server, 2).await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .respond_with(ExpireFirstSessionCall {
            calls: Arc::clone(&calls),
        })
        .expect(2)
        .mount(&server)
        .await;

    let entry = http_test_entry(format!("{}/mcp", server.uri()));
    let caveats = Caveats::top();
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    let connected = connect_http(&admitted, &caveats).await.unwrap();
    let mut toolset = McpToolset {
        servers: vec![ToolsetServer {
            live: connected,
            http: Some(ToolsetHttpReconnectState { entry, caveats }),
        }],
        sanitize_server_names: true,
    };

    let output = toolset.call("headless_http__review", &json!({})).await;
    assert!(output.contains("recovered"), "{output}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.verify().await;
}

#[tokio::test]
async fn headless_recovers_404_then_replay_401_with_configured_authorization() {
    let server = MockServer::start().await;
    mount_toolset_lifecycle(&server, 3).await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .respond_with(ExpireThenRejectReplay {
            calls: Arc::clone(&calls),
        })
        .expect(3)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let secret_path = temp.path().join("mcp-token");
    std::fs::write(&secret_path, "configured-secret\n").unwrap();
    let mut entry = http_test_entry(format!("{}/mcp", server.uri()));
    entry.headers.insert(
        "Authorization".into(),
        newt_core::mcp::SecretValue::literal(format!("Bearer ${{file:{}}}", secret_path.display())),
    );
    let caveats = Caveats::top();
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    let connected = connect_http(&admitted, &caveats).await.unwrap();
    let mut toolset = McpToolset {
        servers: vec![ToolsetServer {
            live: connected,
            http: Some(ToolsetHttpReconnectState { entry, caveats }),
        }],
        sanitize_server_names: true,
    };

    let recovered = toolset.call("headless_http__review", &json!({})).await;
    assert!(
        recovered.contains("recovered connection retained"),
        "{recovered}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    server.verify().await;
}

#[tokio::test]
async fn headless_toolset_reresolves_file_authorization_after_401() {
    let server = MockServer::start().await;
    mount_toolset_lifecycle(&server, 2).await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .and(header("authorization", "Bearer old-secret"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .and(header("authorization", "Bearer new-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"rotated credential accepted"}]}}"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let temp = tempfile::tempdir().unwrap();
    let secret_path = temp.path().join("mcp-token");
    std::fs::write(&secret_path, "old-secret\n").unwrap();
    let mut entry = http_test_entry(format!("{}/mcp", server.uri()));
    entry.headers.insert(
        "Authorization".into(),
        newt_core::mcp::SecretValue::literal(format!("Bearer ${{file:{}}}", secret_path.display())),
    );
    let caveats = Caveats::top();
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    let connected = connect_http(&admitted, &caveats).await.unwrap();
    std::fs::write(&secret_path, "new-secret\n").unwrap();
    let mut toolset = McpToolset {
        servers: vec![ToolsetServer {
            live: connected,
            http: Some(ToolsetHttpReconnectState { entry, caveats }),
        }],
        sanitize_server_names: true,
    };

    let output = toolset.call("headless_http__review", &json!({})).await;
    assert!(output.contains("rotated credential accepted"), "{output}");
    server.verify().await;
}

#[tokio::test]
async fn call_reports_unknown_server_without_wrapping() {
    let mut toolset = McpToolset::empty();
    let out = toolset.call("ghost__tool", &json!({})).await;
    assert_eq!(out, "error: no connected MCP server `ghost`");
}

#[test]
fn call_reports_non_namespaced_name_without_wrapping() {
    // Sync check of the pre-dispatch branch via a blocking runtime, since
    // `call` is async but this path returns before touching a connection.
    let out = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut toolset = McpToolset::empty();
            toolset.call("not_namespaced", &json!({})).await
        });
    assert_eq!(out, "error: `not_namespaced` is not a namespaced MCP tool");
}
