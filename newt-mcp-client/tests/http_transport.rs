//! Integration tests for the streamable-HTTP MCP transport (issue #211).
//!
//! Drives `connect_http` against a `wiremock` server that speaks MCP JSON-RPC
//! over HTTP — covering the `application/json` and `text/event-stream` response
//! shapes, `Mcp-Session-Id` capture/echo, and configured-header passthrough.
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:53 EDT | Date: 2026-08-12

use std::collections::BTreeMap;

use newt_core::mcp::{McpServerEntry, SecretValue, TransportKind};
use newt_mcp_client::{connect_http, connect_http_with_runtime_bearer, PROTOCOL_VERSION};
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
                    format!(
                        r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{PROTOCOL_VERSION}","capabilities":{{}},"serverInfo":{{"name":"test-http","version":"1"}}}}}}"#
                    ),
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
        .and(header("mcp-protocol-version", PROTOCOL_VERSION))
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
        .and(header("mcp-protocol-version", PROTOCOL_VERSION))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "application/json").set_body_string(
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"find things","inputSchema":{"type":"object"}}]}}"#,
        ))
        .mount(&server)
        .await;

    // tools/call (id 3) — JSON content result.
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .and(header("mcp-protocol-version", PROTOCOL_VERSION))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "application/json").set_body_string(
            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hit"}]}}"#,
        ))
        .mount(&server)
        .await;

    let entry = http_entry(format!("{}/mcp", server.uri()), BTreeMap::new());

    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");
    let mut connected = connect_http_with_runtime_bearer(
        &admitted,
        &newt_core::caveats::Caveats::top(),
        Some("secret"),
        false,
    )
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
        .and(header("mcp-protocol-version", PROTOCOL_VERSION))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"fetch\",\"description\":\"\",\"inputSchema\":{\"type\":\"object\"}}]}}\n\n",
                "text/event-stream",
            ),
        )
        .mount(&server)
        .await;

    let entry = http_entry(format!("{}/mcp", server.uri()), BTreeMap::new());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");
    let connected = connect_http(&admitted, &newt_core::caveats::Caveats::top())
        .await
        .expect("connect_http (SSE) should succeed");
    assert_eq!(connected.tools.len(), 1);
    assert_eq!(connected.tools[0].name, "fetch");
}

#[tokio::test]
async fn server_selected_legacy_version_is_the_subsequent_http_header() {
    let server = MockServer::start().await;
    let selected = "2025-03-26";
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(format!(
                    r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{selected}","capabilities":{{}},"serverInfo":{{"name":"test-http","version":"1"}}}}}}"#
                )),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains(
            "\"method\":\"notifications/initialized\"",
        ))
        .and(header("mcp-protocol-version", selected))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/list\""))
        .and(header("mcp-protocol-version", selected))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#),
        )
        .mount(&server)
        .await;

    let entry = http_entry(format!("{}/mcp", server.uri()), BTreeMap::new());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");
    let connected = connect_http(&admitted, &newt_core::caveats::Caveats::top())
        .await
        .expect("legacy handshake revision remains compatible");
    assert!(connected.tools.is_empty());
    server.verify().await;
}

#[tokio::test]
async fn http_rejects_the_pre_streamable_2024_revision() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"legacy-http","version":"1"}}}"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let entry = http_entry(format!("{}/mcp", server.uri()), BTreeMap::new());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");
    let error = match connect_http(&admitted, &newt_core::caveats::Caveats::top()).await {
        Ok(_) => panic!("2024-11-05 predates streamable HTTP"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("unsupported by this transport"),
        "{error:#}"
    );
    server.verify().await;
}

#[test]
fn configured_transport_owned_headers_are_rejected() {
    for header_name in ["MCP-Protocol-Version", "mCp-SeSsIoN-iD", "hOsT"] {
        let mut headers = BTreeMap::new();
        headers.insert(header_name.to_string(), SecretValue::literal("stale"));
        let entry = http_entry("http://127.0.0.1:9/mcp".to_string(), headers);
        let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");
        let error = match newt_mcp_client::HttpTransport::connect(
            &admitted,
            &newt_core::caveats::Caveats::top(),
        ) {
            Ok(_) => panic!("transport-owned header must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("transport-owned"), "{error:#}");
    }
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
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");
    let err = connect_http(&admitted, &newt_core::caveats::Caveats::top())
        .await
        .err()
        .expect("500 must surface as an error");
    assert!(err.to_string().contains("500"), "{err}");
    assert!(!err.to_string().contains("upstream boom"), "{err}");
}

#[tokio::test]
async fn oversized_chunked_response_is_bounded() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0_u8; 8192];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let chunk = vec![b'x'; 16 * 1024];
        for _ in 0..=1024 {
            if socket.write_all(b"4000\r\n").await.is_err()
                || socket.write_all(&chunk).await.is_err()
                || socket.write_all(b"\r\n").await.is_err()
            {
                return;
            }
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
    });

    let entry = http_entry(format!("http://{address}/mcp"), BTreeMap::new());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");
    let error = connect_http(&admitted, &newt_core::caveats::Caveats::top())
        .await
        .err()
        .expect("oversized response must fail");
    assert!(error.to_string().contains("16 MiB limit"), "{error:#}");
    server.await.unwrap();
}

#[tokio::test]
async fn bearer_is_not_forwarded_to_a_redirect_target() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/capture", target.uri()).as_str()),
        )
        .expect(1)
        .mount(&source)
        .await;
    Mock::given(method("POST"))
        .and(path("/capture"))
        .and(header("authorization", "Bearer secret"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&target)
        .await;

    let entry = http_entry(format!("{}/mcp", source.uri()), BTreeMap::new());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");

    let err = match connect_http_with_runtime_bearer(
        &admitted,
        &newt_core::caveats::Caveats::top(),
        Some("secret"),
        false,
    )
    .await
    {
        Ok(_) => panic!("redirecting an authenticated MCP connection must fail"),
        Err(error) => error,
    };
    assert!(err.to_string().contains("307"), "{err:#}");
    source.verify().await;
    target.verify().await;
}

#[test]
fn persisted_plaintext_authorization_is_rejected_by_the_shared_transport() {
    let mut headers = BTreeMap::new();
    headers.insert(
        "Authorization".to_string(),
        SecretValue::literal("Bearer plaintext-secret"),
    );
    let entry = http_entry("http://127.0.0.1:9/mcp".to_string(), headers);
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry is admitted");
    let error = match newt_mcp_client::HttpTransport::connect(
        &admitted,
        &newt_core::caveats::Caveats::top(),
    ) {
        Ok(_) => panic!("plaintext config credential must fail before dialing"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("plaintext Authorization"),
        "{error:#}"
    );
}

/// #1243 Leg 4: the HTTP client is bound to the loopback egress proxy exactly
/// when the `net` grant warrants one (a general remote-host allow-list) — so
/// per-call traffic + redirects are gated, not just the connect-time host.
/// No network here: the globally routable literal is screened without DNS and
/// `HttpTransport::connect` only builds the client (and binds a loopback proxy
/// for the gated case). The proxy's per-host refusal itself is proven in
/// agent-bridle.
#[test]
fn http_connect_wires_the_egress_proxy_only_under_a_remote_host_grant() {
    use newt_core::caveats::{Caveats, Scope};
    use newt_mcp_client::HttpTransport;

    let entry = http_entry("http://8.8.8.8/mcp".to_string(), BTreeMap::new());
    // step-1.2: the transport constructor now requires the admission witness.
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry admits");

    // A general remote-host grant engages the proxy.
    let granted = Caveats {
        net: Scope::only(["8.8.8.8".to_string()]),
        ..Caveats::top()
    };
    assert!(
        HttpTransport::connect(&admitted, &granted)
            .expect("build")
            .egress_proxied(),
        "a remote-host net grant must route the client through the proxy"
    );

    // `net: All` (top) warrants no proxy — egress advisory.
    assert!(!HttpTransport::connect(&admitted, &Caveats::top())
        .expect("build")
        .egress_proxied());

    // Deny-all is enforced by the shared transport itself before DNS/dial.
    let deny = Caveats {
        net: Scope::only([] as [String; 0]),
        ..Caveats::top()
    };
    let error = match HttpTransport::connect(&admitted, &deny) {
        Ok(_) => panic!("deny-all must refuse the HTTP origin"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("outside the session net"),
        "{error:#}"
    );
}
