//! Integration tests for the streamable-HTTP MCP transport (issue #211).
//!
//! Drives `connect_http` against a `wiremock` server that speaks MCP JSON-RPC
//! over HTTP — covering the `application/json` and `text/event-stream` response
//! shapes, `Mcp-Session-Id` capture/echo, and configured-header passthrough.

use std::collections::BTreeMap;

use newt_core::mcp::{McpServerEntry, SecretValue, TransportKind};
use newt_mcp_client::connect_http;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn http_entry(url: String, headers: BTreeMap<String, SecretValue>) -> McpServerEntry {
    McpServerEntry {
        enabled: true,
        name: "test-http".to_string(),
        transport: TransportKind::Http,
        command: None,
        args: Vec::new(),
        env: BTreeMap::new(),
        url: Some(url),
        headers,
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
    }
}

/// The `initialize` handler: returns the result AND the session id header that
/// every later request must echo. McpConnection issues id 1 for initialize.
async fn mount_initialize(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("Mcp-Session-Id", "sess-xyz")
                .set_body_string(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
                ),
        )
        .mount(server)
        .await;
    // The `notifications/initialized` notification is acked with 202, no body.
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains(
            "\"method\":\"notifications/initialized\"",
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;
}

#[tokio::test]
async fn http_json_response_lists_and_calls_tools() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;

    // tools/list (id 2) — only matches when BOTH the configured Authorization
    // header AND the captured session id are echoed, proving passthrough +
    // session propagation. JSON content-type.
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/list\""))
        .and(header("authorization", "Bearer secret"))
        .and(header("mcp-session-id", "sess-xyz"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "application/json").set_body_string(
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"find things","inputSchema":{"type":"object"}}]}}"#,
        ))
        .mount(&server)
        .await;

    // tools/call (id 3) — JSON content result.
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "application/json").set_body_string(
            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hit"}]}}"#,
        ))
        .mount(&server)
        .await;

    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        SecretValue::literal("Bearer secret"),
    );
    let entry = http_entry(format!("{}/mcp", server.uri()), headers);

    let mut connected = connect_http(&entry, &newt_core::caveats::Caveats::top())
        .await
        .expect("connect_http should succeed");
    assert_eq!(connected.name, "test-http");
    assert_eq!(connected.tools.len(), 1);
    assert_eq!(connected.tools[0].name, "search");
    assert_eq!(connected.tools[0].description, "find things");

    let result = connected
        .conn
        .call_tool("search", serde_json::json!({"q": "x"}))
        .await
        .expect("call_tool should succeed");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "hit");
}

#[tokio::test]
async fn http_sse_response_is_parsed() {
    let server = MockServer::start().await;
    mount_initialize(&server).await;

    // tools/list answered as an SSE stream (text/event-stream) — the transport
    // must extract the `data:` JSON-RPC message.
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/list\""))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"fetch\",\"description\":\"\",\"inputSchema\":{\"type\":\"object\"}}]}}\n\n",
                "text/event-stream",
            ),
        )
        .mount(&server)
        .await;

    let entry = http_entry(format!("{}/mcp", server.uri()), BTreeMap::new());
    let connected = connect_http(&entry, &newt_core::caveats::Caveats::top())
        .await
        .expect("connect_http (SSE) should succeed");
    assert_eq!(connected.tools.len(), 1);
    assert_eq!(connected.tools[0].name, "fetch");
}

#[tokio::test]
async fn http_error_status_surfaces() {
    let server = MockServer::start().await;
    // initialize returns 500 → connect must fail, not hang.
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream boom"))
        .mount(&server)
        .await;

    let entry = http_entry(format!("{}/mcp", server.uri()), BTreeMap::new());
    let err = connect_http(&entry, &newt_core::caveats::Caveats::top())
        .await
        .err()
        .expect("500 must surface as an error");
    assert!(err.to_string().contains("500"), "{err}");
}

/// #1243 Leg 4: the HTTP client is bound to the loopback egress proxy exactly
/// when the `net` grant warrants one (a general remote-host allow-list) — so
/// per-call traffic + redirects are gated, not just the connect-time host. No
/// network here: `HttpTransport::connect` only builds the client (and binds a
/// loopback proxy for the gated case). The proxy's per-host refusal itself is
/// proven in agent-bridle.
#[test]
fn http_connect_wires_the_egress_proxy_only_under_a_remote_host_grant() {
    use newt_core::caveats::{Caveats, Scope};
    use newt_mcp_client::HttpTransport;

    let entry = http_entry("http://example.com/mcp".to_string(), BTreeMap::new());

    // A general remote-host grant engages the proxy.
    let granted = Caveats {
        net: Scope::only(["api.example.com".to_string()]),
        ..Caveats::top()
    };
    assert!(
        HttpTransport::connect(&entry, &granted)
            .expect("build")
            .egress_proxied(),
        "a remote-host net grant must route the client through the proxy"
    );

    // `net: All` (top) warrants no proxy — egress advisory.
    assert!(!HttpTransport::connect(&entry, &Caveats::top())
        .expect("build")
        .egress_proxied());

    // Deny-all warrants no proxy either (it is kernel-fenced elsewhere; the
    // HTTP client simply has none wired).
    let deny = Caveats {
        net: Scope::only([] as [String; 0]),
        ..Caveats::top()
    };
    assert!(!HttpTransport::connect(&entry, &deny)
        .expect("build")
        .egress_proxied());
}
