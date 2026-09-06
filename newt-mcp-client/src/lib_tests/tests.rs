use super::*;

#[test]
fn oauth_fence_rejects_private_and_ipv4_mapped_addresses() {
    let link_local = std::net::Ipv4Addr::new(169, 254, 1, 1).to_string();
    for address in [
        "127.0.0.1",
        "10.0.0.1",
        link_local.as_str(),
        "::1",
        "fc00::1",
        "::ffff:10.0.0.1",
        "64:ff9b:1::a00:1",
        "2002:7f00:1::",
        "2002:a00:1::",
    ] {
        let address = address.parse().unwrap();
        assert!(ip_is_non_global(address), "{address} must be non-global");
    }
    assert!(!ip_is_non_global("8.8.8.8".parse().unwrap()));
    assert!(!ip_is_non_global("2606:4700:4700::1111".parse().unwrap()));
    assert!(ip_is_non_global("2001:30::1".parse().unwrap()));
    assert!(!ip_is_non_global("2001:2f::1".parse().unwrap()));
    assert!(ip_is_non_global("3fff:0::1".parse().unwrap()));
    assert!(!ip_is_non_global("3fff:1000::1".parse().unwrap()));
    assert!(!ip_is_non_global("3ffe::1".parse().unwrap()));
}

#[test]
fn private_resolution_requires_an_exact_host_policy_decision() {
    let url = reqwest::Url::parse("http://127.0.0.1:9/mcp").unwrap();
    assert!(FencedHttpClient::for_url(&url, Duration::from_secs(1), false).is_err());
    assert!(FencedHttpClient::for_url(&url, Duration::from_secs(1), true).is_ok());
}

#[test]
fn system_dns_resolution_has_an_explicit_deadline() {
    let error = resolve_with_timeout(
        "slow.example",
        443,
        Duration::from_millis(5),
        |_host, _port| {
            std::thread::sleep(Duration::from_millis(100));
            Ok(Vec::new())
        },
    )
    .expect_err("a stalled resolver must time out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

/// A resolver that parks until the test opens it, standing in for a
/// `getaddrinfo` call that outlives its caller and cannot be cancelled.
#[derive(Default)]
struct StalledResolverGate {
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
}

impl StalledResolverGate {
    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
    }

    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}

#[test]
fn dns_timeout_returns_on_the_caller_deadline_not_the_resolver() {
    let pool = std::sync::Arc::new(DnsWorkerPool::new(2));
    let gate = std::sync::Arc::new(StalledResolverGate::default());
    let worker_gate = std::sync::Arc::clone(&gate);
    let start = std::time::Instant::now();
    let error = resolve_with_timeout_in(
        &pool,
        "stalled.example",
        443,
        Duration::from_millis(20),
        move |_host, _port| {
            worker_gate.wait();
            Ok(Vec::new())
        },
    )
    .expect_err("a resolver that never returns must not block the caller");
    let elapsed = start.elapsed();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    // The fix must not be "join the stuck worker after the deadline".
    assert!(
        elapsed < Duration::from_secs(2),
        "the caller waited {elapsed:?} on a resolver that never returned"
    );
    gate.open();
}

#[test]
fn dns_resolver_capacity_is_bounded_and_exhaustion_fails_closed() {
    let pool = std::sync::Arc::new(DnsWorkerPool::new(2));
    let gate = std::sync::Arc::new(StalledResolverGate::default());
    let (started, worker_started) = std::sync::mpsc::channel();

    for _ in 0..2 {
        let worker_gate = std::sync::Arc::clone(&gate);
        let started = started.clone();
        let error = resolve_with_timeout_in(
            &pool,
            "stalled.example",
            443,
            Duration::from_millis(5),
            move |_host, _port| {
                let _ = started.send(());
                worker_gate.wait();
                Ok(Vec::new())
            },
        )
        .expect_err("a stalled resolver must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }
    for _ in 0..2 {
        worker_started
            .recv_timeout(Duration::from_secs(5))
            .expect("both workers must reach the resolver");
    }
    assert_eq!(
        pool.outstanding(),
        2,
        "both slots stay charged after timeout"
    );

    // Fail closed: no uncounted fallback worker for the third caller, and
    // the wait for a slot is charged against that caller's own deadline.
    let refusal_started = std::time::Instant::now();
    let refused = resolve_with_timeout_in(
        &pool,
        "third.example",
        443,
        Duration::from_millis(50),
        |_host, _port| -> std::io::Result<Vec<std::net::SocketAddr>> {
            unreachable!("capacity exhaustion must not start another resolver")
        },
    )
    .expect_err("resolver capacity exhaustion must fail closed");
    let refusal_elapsed = refusal_started.elapsed();
    assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock);
    assert!(
        refusal_elapsed < Duration::from_secs(2),
        "queueing for a slot waited {refusal_elapsed:?}, past the caller deadline"
    );
    assert_eq!(pool.outstanding(), 2, "a refusal must not charge a slot");

    gate.open();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while pool.outstanding() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pool.outstanding(),
        0,
        "slots are released by the worker once the resolver returns"
    );
    let resolved = resolve_with_timeout_in(
        &pool,
        "recovered.example",
        443,
        Duration::from_secs(5),
        |_host, _port| Ok(vec!["93.184.216.34:443".parse().unwrap()]),
    )
    .expect("capacity must be reusable once workers finish");
    assert_eq!(resolved.len(), 1);
}

#[test]
fn repeated_dns_timeouts_do_not_accumulate_unbounded_workers() {
    const CAPACITY: usize = 3;
    const ATTEMPTS: usize = 64;
    let pool = std::sync::Arc::new(DnsWorkerPool::new(CAPACITY));
    let gate = std::sync::Arc::new(StalledResolverGate::default());
    let mut timed_out = 0usize;
    let mut refused = 0usize;

    for attempt in 0..ATTEMPTS {
        let worker_gate = std::sync::Arc::clone(&gate);
        let error = resolve_with_timeout_in(
            &pool,
            "stalled.example",
            443,
            Duration::from_millis(2),
            move |_host, _port| {
                worker_gate.wait();
                Ok(Vec::new())
            },
        )
        .expect_err("a stalled resolver must never succeed");
        match error.kind() {
            std::io::ErrorKind::TimedOut => timed_out += 1,
            std::io::ErrorKind::WouldBlock => refused += 1,
            other => panic!("attempt {attempt} produced unexpected error kind {other:?}"),
        }
        assert!(
            pool.outstanding() <= CAPACITY,
            "attempt {attempt} left {} workers outstanding, above the {CAPACITY} cap",
            pool.outstanding()
        );
    }

    assert_eq!(
        timed_out, CAPACITY,
        "only capacity-many workers ever started"
    );
    assert_eq!(
        refused,
        ATTEMPTS - CAPACITY,
        "every later resolution is refused instead of spawning a worker"
    );
    gate.open();
}

#[test]
fn private_host_approval_never_overrides_special_address_fences() {
    let six_to_four: std::net::IpAddr = "2002:7f00:1::".parse().unwrap();
    assert!(ip_is_non_global(six_to_four));
    assert!(!ip_is_approvable_private(six_to_four));
    assert!(!fenced_ip_is_allowed(six_to_four, true));
    let link_local_metadata = std::net::Ipv4Addr::new(169, 254, 169, 254).to_string();
    let shared_address_space = std::net::Ipv4Addr::new(100, 64, 0, 1).to_string();
    for forbidden in [
        link_local_metadata.as_str(),
        shared_address_space.as_str(),
        "fe80::1",
        "2001:100::1",
        "5f00::1",
    ] {
        let address = forbidden.parse().unwrap();
        assert!(ip_is_non_global(address), "{address}");
        assert!(!ip_is_approvable_private(address), "{address}");
        assert!(!fenced_ip_is_allowed(address, true), "{address}");
    }
}

#[test]
fn exact_private_grant_is_canonical_but_never_url_shaped() {
    use newt_core::caveats::Scope;

    let canonical = Caveats {
        net: Scope::only(["REVIEW.INTERNAL.EXAMPLE".to_string()]),
        ..Caveats::top()
    };
    assert!(exact_host_is_explicitly_granted(
        &canonical,
        "review.internal.example"
    ));
    for not_a_host in [
        "review.internal.example:8443",
        "https://review.internal.example",
        "review.internal.example/path",
        "*.internal.example",
        " review.internal.example",
        "[::1",
        "::1]",
    ] {
        let caveats = Caveats {
            net: Scope::only([not_a_host.to_string()]),
            ..Caveats::top()
        };
        assert!(
            !exact_host_is_explicitly_granted(&caveats, "review.internal.example"),
            "{not_a_host} must not become an exact hostname grant"
        );
    }
}

fn private_http_entry(url: String) -> McpServerEntry {
    McpServerEntry {
        enabled: true,
        name: "private-review".to_string(),
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

#[test]
fn ungranted_private_dns_answer_fails_before_dial() {
    let entry = private_http_entry("http://review.internal.example:8443/mcp".to_string());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
    let resolver =
        |_host: &str, port: u16| Ok(vec![std::net::SocketAddr::from(([10, 0, 0, 42], port))]);
    let error = match HttpTransport::connect_with_runtime_bearer_and_resolver(
        &admitted,
        &Caveats::top(),
        None,
        false,
        &resolver,
    ) {
        Ok(_) => panic!("private DNS without an exact host grant must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("without an exact net grant"),
        "{error:#}"
    );
}

#[test]
fn public_host_outside_scope_fails_before_dns() {
    use newt_core::caveats::Scope;

    let entry = private_http_entry("https://public.example/mcp".to_string());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
    let resolver = |_host: &str, _port: u16| -> std::io::Result<Vec<std::net::SocketAddr>> {
        panic!("out-of-scope host must fail before DNS")
    };
    let deny = Caveats {
        net: Scope::only([] as [String; 0]),
        ..Caveats::top()
    };
    let error = match HttpTransport::connect_with_runtime_bearer_and_resolver(
        &admitted, &deny, None, false, &resolver,
    ) {
        Ok(_) => panic!("public DNS outside the net scope must fail"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("outside the session net"),
        "{error:#}"
    );
}

#[test]
fn localhost_must_resolve_only_to_loopback_even_when_explicitly_granted() {
    use newt_core::caveats::Scope;

    let entry = private_http_entry("http://localhost:8443/mcp".to_string());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
    let explicitly_granted = Caveats {
        net: Scope::only(["localhost".to_string()]),
        ..Caveats::top()
    };
    for caveats in [Caveats::top(), explicitly_granted] {
        for address in [
            std::net::SocketAddr::from(([10, 0, 0, 42], 8443)),
            std::net::SocketAddr::from(([8, 8, 8, 8], 8443)),
        ] {
            let resolver = |_host: &str, _port: u16| Ok(vec![address]);
            let error = match HttpTransport::connect_with_runtime_bearer_and_resolver(
                &admitted, &caveats, None, false, &resolver,
            ) {
                Ok(_) => panic!("localhost mapped outside loopback must fail"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("outside loopback"), "{error:#}");
        }
    }
}

#[test]
fn resolver_cannot_pivot_the_pinned_origin_to_another_port() {
    use newt_core::caveats::Scope;

    let entry = private_http_entry("http://review.internal.example:8443/mcp".to_string());
    let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
    let caveats = Caveats {
        net: Scope::only(["review.internal.example".to_string()]),
        ..Caveats::top()
    };
    let resolver =
        |_host: &str, _port: u16| Ok(vec![std::net::SocketAddr::from(([10, 0, 0, 42], 9443))]);
    let error = match HttpTransport::connect_with_runtime_bearer_and_resolver(
        &admitted, &caveats, None, false, &resolver,
    ) {
        Ok(_) => panic!("a resolver-provided port pivot must fail"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("wrong port"), "{error:#}");
}

#[test]
fn unsafe_http_url_shapes_fail_before_dns() {
    let resolver = |_host: &str, _port: u16| -> std::io::Result<Vec<std::net::SocketAddr>> {
        panic!("unsafe URL must be rejected before DNS")
    };
    for url in [
        "ftp://review.internal.example/mcp",
        "https://user@review.internal.example/mcp",
        "https://review.internal.example/mcp?token=x",
        "https://review.internal.example/mcp#fragment",
    ] {
        let entry = private_http_entry(url.to_string());
        let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
        assert!(
            HttpTransport::connect_with_runtime_bearer_and_resolver(
                &admitted,
                &Caveats::top(),
                None,
                false,
                &resolver,
            )
            .is_err(),
            "{url} must fail"
        );
    }
}

#[tokio::test]
#[ignore = "real loopback private-host MCP lifecycle"]
async fn exact_private_hostname_grant_pins_dns_for_full_mcp_lifecycle() {
    use newt_core::caveats::Scope;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"initialize\""))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .insert_header("Mcp-Session-Id", "private-session")
                .set_body_string(format!(
                    r#"{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"{PROTOCOL_VERSION}","capabilities":{{}},"serverInfo":{{"name":"review","version":"1"}}}}}}"#,
                )),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains(
            "\"method\":\"notifications/initialized\"",
        ))
        .and(header("mcp-session-id", "private-session"))
        .and(header("mcp-protocol-version", PROTOCOL_VERSION))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/list\""))
        .and(header("mcp-session-id", "private-session"))
        .and(header("mcp-protocol-version", PROTOCOL_VERSION))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"review","description":"review a change","inputSchema":{"type":"object"}}]}}"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_string_contains("\"method\":\"tools/call\""))
        .and(header("mcp-session-id", "private-session"))
        .and(header("mcp-protocol-version", PROTOCOL_VERSION))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"review loaded"}]}}"#,
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let private_host = "review.internal.example";
    let server_url = reqwest::Url::parse(&server.uri()).expect("wiremock URL");
    let server_port = server_url.port().expect("wiremock has an explicit port");
    let entry = private_http_entry(format!("http://{private_host}:{server_port}/mcp"));
    let admitted = newt_core::mcp::admit(&entry).expect("trusted entry admits");
    let caveats = Caveats {
        net: Scope::only([private_host.to_string()]),
        ..Caveats::top()
    };
    let resolution_count = Arc::new(AtomicUsize::new(0));
    let resolver_count = Arc::clone(&resolution_count);
    let resolver = move |host: &str, port: u16| {
        assert_eq!(host, private_host);
        assert_eq!(port, server_port);
        resolver_count.fetch_add(1, Ordering::SeqCst);
        Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))])
    };
    let transport = HttpTransport::connect_with_runtime_bearer_and_resolver(
        &admitted, &caveats, None, false, &resolver,
    )
    .expect("an exact private-host grant builds a pinned transport");
    assert!(transport.private_origin_pinned());
    assert!(!transport.egress_proxied());
    let net = net_posture(
        &caveats,
        transport.egress_proxied(),
        transport.private_origin_pinned(),
    );
    let mut connected = finish_connect(&entry, AnyTransport::Http(Box::new(transport)), None, net)
        .await
        .expect("initialize and tools/list succeed through the pinned host");
    assert_eq!(connected.tools.len(), 1);
    assert_eq!(connected.tools[0].name, "review");
    let result = connected
        .conn
        .call_tool("review", json!({"review": 4242}))
        .await
        .expect("tool call succeeds through the same pinned host");
    assert_eq!(result["content"][0]["text"].as_str(), Some("review loaded"));
    assert_eq!(
        resolution_count.load(Ordering::SeqCst),
        1,
        "initialize, initialized, list, and call must reuse one DNS answer"
    );
    server.verify().await;
}

#[tokio::test]
async fn list_tools_metadata_survives_to_catalog_and_is_absent_safe() {
    // id 1 = initialize, id 2 = tools/list (notify carries no id/response).
    let mut conn = McpConnection::new(MockTransport::new([
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"find","inputSchema":{"type":"object"},"_meta":{"newt/resourceUrlPrefixes":["https://search.example/resources/"]}},{"name":"status","inputSchema":{"type":"object"}},{"name":"mixed","_meta":{"newt/resourceUrlPrefixes":["https://search.example/resources/",7]}}]}}"#,
    ]));
    conn.initialize().await.unwrap();
    let tools = conn.list_tools().await.unwrap();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0].name, "search");
    assert_eq!(tools[0].description, "find");
    assert_eq!(
        tools[0].meta.as_ref().unwrap()[newt_core::MCP_RESOURCE_URL_PREFIXES_META_KEY],
        json!(["https://search.example/resources/"])
    );
    assert!(tools[1].meta.is_none());
    assert!(
        tools[2].meta.is_some(),
        "deserialization retains server metadata"
    );

    let valid = openai_tool_definition("search-source", true, &tools[0]);
    assert_eq!(valid["function"]["name"], "search_source__search");
    assert_eq!(
        valid["_meta"][newt_core::MCP_RESOURCE_URL_PREFIXES_META_KEY],
        json!(["https://search.example/resources/"])
    );
    let absent = openai_tool_definition("search-source", true, &tools[1]);
    assert!(absent.get("_meta").is_none());
    let malformed = openai_tool_definition("search-source", true, &tools[2]);
    assert!(
        malformed.get("_meta").is_none(),
        "a mixed array must grant no routing affinity"
    );
}

#[tokio::test]
async fn initialize_captures_server_identity_and_instructions() {
    let mut conn = McpConnection::new(MockTransport::new([
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"scrybe","title":"Scrybe","version":"1.2.3"},"instructions":"Edit Markdown documents."}}"#,
    ]));
    let info = conn.initialize().await.unwrap();
    let si = info.server_info.expect("serverInfo captured");
    assert_eq!(si.name, "scrybe");
    assert_eq!(si.title.as_deref(), Some("Scrybe"));
    assert_eq!(si.version, "1.2.3");
    assert_eq!(
        info.instructions.as_deref(),
        Some("Edit Markdown documents.")
    );
    assert_eq!(info.protocol_version.as_deref(), Some("2024-11-05"));
    assert_eq!(
        conn.transport.protocol_version.as_deref(),
        Some("2024-11-05")
    );
    assert!(info.capabilities.get("tools").is_some());
}

#[tokio::test]
async fn initialize_rejects_unknown_protocol_revision() {
    let mut conn = McpConnection::new(MockTransport::new([
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2099-01-01","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#,
    ]));
    let error = conn.initialize().await.unwrap_err().to_string();
    assert!(error.contains("unsupported by this transport"), "{error}");
}

#[tokio::test]
async fn malformed_initialize_diagnostics_do_not_reflect_remote_values() {
    let responses = [
        r#"{"jsonrpc":"2.0","id":1,"result":"TOP-SECRET\u001b[31m\nforged log"}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"TOP-SECRET\u001b[31m\nforged log","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":{"value":"TOP-SECRET\u001b[31m\nforged log"},"version":"1"}}}"#,
    ];

    for response in responses {
        let error = McpConnection::new(MockTransport::new([response]))
            .initialize()
            .await
            .unwrap_err()
            .to_string();
        for forbidden in ["TOP-SECRET", "forged log", "\u{1b}", "\n", "\r"] {
            assert!(
                !error.contains(forbidden),
                "reflected {forbidden:?}: {error:?}"
            );
        }
    }
}

#[test]
fn http_recovery_budget_allows_session_then_runtime_bearer_recovery_once() {
    let missing = anyhow::Error::new(HttpStatusError::new(404, "Not Found", ""));
    let unauthorized = anyhow::Error::new(HttpStatusError::new(401, "Unauthorized", ""));
    let mut budget = HttpRecoveryBudget::new(true, true, false);

    assert_eq!(
        budget.next(&missing),
        HttpRecoveryAction::ReconnectExpiredSession
    );
    assert_eq!(
        budget.next(&unauthorized),
        HttpRecoveryAction::RefreshRuntimeBearer
    );
    assert_eq!(budget.next(&unauthorized), HttpRecoveryAction::Stop);
    assert_eq!(budget.next(&missing), HttpRecoveryAction::Stop);
}

#[test]
fn session_reconnect_preserves_one_configured_authorization_recovery() {
    let missing = anyhow::Error::new(HttpStatusError::new(404, "Not Found", ""));
    let unauthorized = anyhow::Error::new(HttpStatusError::new(401, "Unauthorized", ""));
    let mut budget = HttpRecoveryBudget::new(true, false, true);

    assert_eq!(
        budget.next(&missing),
        HttpRecoveryAction::ReconnectExpiredSession
    );
    assert_eq!(
        budget.next(&unauthorized),
        HttpRecoveryAction::ReconnectConfiguredAuthorization
    );
    assert_eq!(budget.next(&unauthorized), HttpRecoveryAction::Stop);

    let mut direct = HttpRecoveryBudget::new(false, false, true);
    assert_eq!(
        direct.next(&unauthorized),
        HttpRecoveryAction::ReconnectConfiguredAuthorization
    );
    assert_eq!(direct.next(&unauthorized), HttpRecoveryAction::Stop);
}

#[tokio::test]
async fn recovery_machine_handles_404_then_replay_401_then_one_bearer_refresh() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let reconnects = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let reconnect_counter = Arc::clone(&reconnects);
    let refresh_counter = Arc::clone(&refreshes);
    let call_counter = Arc::clone(&calls);
    let initial = anyhow::Error::new(HttpStatusError::new(404, "Not Found", ""));

    let outcome = recover_http_call_after_error(
        initial,
        true,
        Some("stale-bearer".to_string()),
        false,
        move |rejected| {
            assert_eq!(rejected, "stale-bearer");
            refresh_counter.fetch_add(1, Ordering::SeqCst);
            async { Some("fresh-bearer".to_string()) }
        },
        move |bearer| {
            let attempt = reconnect_counter.fetch_add(1, Ordering::SeqCst);
            async move {
                if attempt == 0 {
                    assert_eq!(bearer.as_deref(), Some("stale-bearer"));
                    Ok("stale-connection")
                } else {
                    assert_eq!(bearer.as_deref(), Some("fresh-bearer"));
                    Ok("fresh-connection")
                }
            }
        },
        move |connection| {
            let attempt = call_counter.fetch_add(1, Ordering::SeqCst);
            async move {
                let result = if attempt == 0 {
                    assert_eq!(connection, "stale-connection");
                    Err(anyhow::Error::new(HttpStatusError::new(
                        401,
                        "Unauthorized",
                        "",
                    )))
                } else {
                    assert_eq!(connection, "fresh-connection");
                    Ok("final-result")
                };
                (connection, result)
            }
        },
    )
    .await
    .unwrap()
    .expect("bounded recovery succeeds");

    assert_eq!(outcome.connection, "fresh-connection");
    assert_eq!(outcome.bearer.as_deref(), Some("fresh-bearer"));
    assert_eq!(outcome.result.unwrap(), "final-result");
    assert_eq!(reconnects.load(Ordering::SeqCst), 2);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn configured_auth_recovery_handles_404_then_replay_401_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let reconnects = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let reconnect_counter = Arc::clone(&reconnects);
    let call_counter = Arc::clone(&calls);
    let outcome = recover_http_call_after_error(
        anyhow::Error::new(HttpStatusError::new(404, "Not Found", "")),
        true,
        None,
        true,
        |_rejected| async { panic!("configured auth is re-resolved, not OAuth-refreshed") },
        move |bearer| {
            assert!(bearer.is_none());
            let attempt = reconnect_counter.fetch_add(1, Ordering::SeqCst);
            async move { Ok(attempt) }
        },
        move |connection| {
            let attempt = call_counter.fetch_add(1, Ordering::SeqCst);
            async move {
                let result = if attempt == 0 {
                    Err(anyhow::Error::new(HttpStatusError::new(
                        401,
                        "Unauthorized",
                        "",
                    )))
                } else {
                    Ok("configured credential accepted")
                };
                (connection, result)
            }
        },
    )
    .await
    .unwrap()
    .expect("session reset plus configured-credential recovery succeeds");

    assert_eq!(outcome.connection, 1);
    assert!(outcome.bearer.is_none());
    assert_eq!(outcome.result.unwrap(), "configured credential accepted");
    assert_eq!(reconnects.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn configured_auth_recovery_stops_after_session_and_credential_budgets() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let reconnects = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let reconnect_counter = Arc::clone(&reconnects);
    let call_counter = Arc::clone(&calls);
    let outcome = recover_http_call_after_error(
        anyhow::Error::new(HttpStatusError::new(404, "Not Found", "")),
        true,
        None,
        true,
        |_rejected| async { panic!("configured auth is re-resolved, not OAuth-refreshed") },
        move |_bearer| {
            let connection = reconnect_counter.fetch_add(1, Ordering::SeqCst);
            async move { Ok(connection) }
        },
        move |connection| {
            call_counter.fetch_add(1, Ordering::SeqCst);
            async move {
                (
                    connection,
                    Err::<(), _>(anyhow::Error::new(HttpStatusError::new(
                        401,
                        "Unauthorized",
                        "",
                    ))),
                )
            }
        },
    )
    .await
    .unwrap()
    .expect("final replay failure retains the second connection");

    assert_eq!(outcome.connection, 1);
    assert!(outcome.result.is_err());
    assert_eq!(reconnects.load(Ordering::SeqCst), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn recovery_machine_stops_after_final_replay_failure() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let reconnects = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let reconnect_counter = Arc::clone(&reconnects);
    let refresh_counter = Arc::clone(&refreshes);
    let call_counter = Arc::clone(&calls);
    let result = recover_http_call_after_error(
        anyhow::Error::new(HttpStatusError::new(404, "Not Found", "")),
        true,
        Some("stale-bearer".to_string()),
        false,
        move |_rejected| {
            refresh_counter.fetch_add(1, Ordering::SeqCst);
            async { Some("fresh-bearer".to_string()) }
        },
        move |bearer| {
            reconnect_counter.fetch_add(1, Ordering::SeqCst);
            async move { Ok(bearer.expect("both reconnects carry a bearer")) }
        },
        move |connection| {
            call_counter.fetch_add(1, Ordering::SeqCst);
            async move {
                (
                    connection,
                    Err::<(), _>(anyhow::Error::new(HttpStatusError::new(
                        401,
                        "Unauthorized",
                        "",
                    ))),
                )
            }
        },
    )
    .await;

    let outcome = result
        .unwrap()
        .expect("the final failed replay still returns the recovered connection");
    assert!(outcome.result.is_err());
    assert_eq!(reconnects.load(Ordering::SeqCst), 2);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn protocol_support_is_explicitly_limited_to_the_handshake_era() {
    assert_eq!(PROTOCOL_VERSION, "2025-11-25");
    assert!(SUPPORTED_PROTOCOL_VERSIONS.contains(&"2025-06-18"));
    assert!(!SUPPORTED_PROTOCOL_VERSIONS.contains(&"2026-07-28"));
    assert!(!HTTP_SUPPORTED_PROTOCOL_VERSIONS.contains(&"2024-11-05"));
}

#[tokio::test]
async fn initialize_rejects_missing_required_server_info() {
    let mut conn = McpConnection::new(MockTransport::new([
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
    ]));
    let error = conn.initialize().await.unwrap_err().to_string();
    assert!(error.contains("serverInfo"), "{error}");
}

#[test]
fn scheme_host_authority_ends_at_slash_query_or_fragment() {
    assert_eq!(
        parse_scheme_host(Some("https://mcp.example?key=v")),
        ("https".into(), "mcp.example".into())
    );
    assert_eq!(
        parse_scheme_host(Some("http://evil.example?@127.0.0.1/")),
        ("http".into(), "evil.example".into()),
        "an @ inside the query must not smuggle a fake host"
    );
    assert_eq!(
        parse_scheme_host(Some("http://user@[::1]:8080/x#f")),
        ("http".into(), "::1".into())
    );
}

#[test]
fn http_status_error_does_not_echo_untrusted_body_and_downcasts() {
    let err = HttpStatusError::new(401, "Unauthorized", "token missing");
    assert_eq!(err.to_string(), "MCP server returned HTTP 401 Unauthorized");
    assert!(!err.to_string().contains("token missing"));
    let chained = anyhow::Error::new(err).context("initializing MCP server `x`");
    let found = chained
        .chain()
        .find_map(|c| c.downcast_ref::<HttpStatusError>())
        .expect("typed error survives an anyhow context chain");
    assert_eq!(found.status, 401);
}

#[test]
fn loopback_is_an_ip_property() {
    for yes in ["localhost", "127.0.0.1", "127.9.8.7", "::1"] {
        assert!(host_is_loopback(yes), "{yes}");
    }
    for no in ["127.0.0.1.evil.com", "127.evil.example", "mcp.example", ""] {
        assert!(!host_is_loopback(no), "{no}");
    }
}

#[tokio::test]
async fn initialize_rejects_an_echoed_request_as_not_an_mcp_server() {
    // `/bin/cat` echoes our own initialize REQUEST back: id matches, no
    // `error`, no `result`. request() then yields Null — which must NOT
    // count as a handshake, or the probe would certify any stdin-echoing
    // process as an MCP server (and save it).
    let mut conn = McpConnection::new(MockTransport::new([
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
    ]));
    let err = conn.initialize().await.unwrap_err();
    assert!(err.to_string().contains("not an MCP server"), "{err}");
}

#[tokio::test]
async fn initialize_rejects_non_handshake_results() {
    // A result that is not an InitializeResult object (array / scalar /
    // object missing protocolVersion or capabilities) is not a handshake.
    for result in [
        r#"{"jsonrpc":"2.0","id":1,"result":[1,2]}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
    ] {
        let mut conn = McpConnection::new(MockTransport::new([result]));
        let err = conn.initialize().await.unwrap_err();
        assert!(err.to_string().contains("initialize"), "{result} → {err}");
    }
}

#[test]
fn session_identifier_requires_visible_ascii() {
    for valid in ["session", "opaque-123_~", "!"] {
        assert!(valid_mcp_session_id(valid), "{valid:?}");
    }
    for invalid in ["", "has space", "tab\t", "line\n", "non-ascii-é"] {
        assert!(!valid_mcp_session_id(invalid), "{invalid:?}");
    }
}

#[tokio::test]
async fn request_skips_notifications_and_mismatched_ids() {
    // A log notification (no id) and a stale response (wrong id) precede ours.
    let mut conn = McpConnection::new(MockTransport::new([
        r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":99,"result":{"stale":true}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
    ]));
    // First request → id 1; must skip the first two lines.
    let tools = conn.list_tools().await.unwrap();
    assert!(tools.is_empty());
}

#[tokio::test]
async fn server_error_exposes_only_method_and_numeric_code() {
    let mut conn = McpConnection::new(MockTransport::new([
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"TOP-SECRET\u001b[31m\nforged log","data":{"token":"TOP-SECRET"}}}"#,
    ]));
    let error = conn.list_tools().await.unwrap_err().to_string();
    assert_eq!(
        error,
        "MCP server error on `tools/list` (JSON-RPC code -32601)"
    );
    for forbidden in ["TOP-SECRET", "forged log", "\u{1b}", "\n", "\r"] {
        assert!(
            !error.contains(forbidden),
            "reflected {forbidden:?}: {error:?}"
        );
    }

    let mut non_numeric = McpConnection::new(MockTransport::new([
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":"TOP-SECRET\u001b[31m","message":"forged log"}}"#,
    ]));
    assert_eq!(
        non_numeric.list_tools().await.unwrap_err().to_string(),
        "MCP server error on `tools/list`"
    );
}

#[tokio::test]
async fn closed_stream_is_an_error_not_a_hang() {
    let mut conn = McpConnection::new(MockTransport::new([])); // EOF immediately
    let err = conn.list_tools().await.unwrap_err();
    assert!(err.to_string().contains("closed the connection"), "{err}");
}

#[test]
fn namespacing_roundtrips() {
    assert_eq!(namespaced("git", "status"), "git__status");
    assert_eq!(split_namespaced("git__status"), Some(("git", "status")));
    assert_eq!(split_namespaced("nounsep"), None);
}

#[test]
fn parse_sse_extracts_data_messages_in_order() {
    let body = "event: message\ndata: {\"id\":1}\n\nevent: message\ndata: {\"id\":2}\n\n";
    assert_eq!(parse_sse_messages(body), vec!["{\"id\":1}", "{\"id\":2}"]);
}

#[test]
fn parse_sse_joins_multiline_data_and_ignores_other_fields() {
    // Two data lines in one event join with '\n'; `id:`/comments are skipped.
    let body = ": keep-alive\nid: 7\ndata: {\"a\":1,\ndata: \"b\":2}\n\n";
    assert_eq!(parse_sse_messages(body), vec!["{\"a\":1,\n\"b\":2}"]);
}

#[test]
fn parse_sse_handles_trailing_event_without_blank_line() {
    let body = "data: {\"only\":true}";
    assert_eq!(parse_sse_messages(body), vec!["{\"only\":true}"]);
    assert!(parse_sse_messages("").is_empty());
}

/// Build an entry carrying just a `request_timeout_secs` override (all other
/// fields default) — every field is `#[serde(default)]`.
fn entry_with_timeout(json: &str) -> McpServerEntry {
    serde_json::from_str(json).unwrap()
}

#[test]
fn resolve_timeout_defaults_when_unset() {
    assert_eq!(
        resolve_timeout(&entry_with_timeout("{}")),
        DEFAULT_REQUEST_TIMEOUT
    );
}

#[test]
fn resolve_timeout_honors_override_and_camel_alias() {
    assert_eq!(
        resolve_timeout(&entry_with_timeout(r#"{"request_timeout_secs":180}"#)),
        Duration::from_secs(180)
    );
    // Claude-format JSON uses the camelCase alias.
    assert_eq!(
        resolve_timeout(&entry_with_timeout(r#"{"requestTimeoutSecs":45}"#)),
        Duration::from_secs(45)
    );
}

#[test]
fn resolve_timeout_clamps_zero_up_and_huge_down() {
    // 0 must never mean "no timeout".
    assert_eq!(
        resolve_timeout(&entry_with_timeout(r#"{"request_timeout_secs":0}"#)),
        Duration::from_secs(1)
    );
    // An over-large value is capped so a wedged call still gives up.
    assert_eq!(
        resolve_timeout(&entry_with_timeout(r#"{"request_timeout_secs":999999}"#)),
        MAX_REQUEST_TIMEOUT
    );
}

/// A transport whose `recv` never resolves — stands in for a wedged server.
struct HangingTransport;
impl Transport for HangingTransport {
    async fn send(&mut self, _line: String) -> Result<()> {
        Ok(())
    }
    async fn recv(&mut self) -> Result<Option<String>> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn request_gives_up_after_the_configured_timeout() {
    // Virtual clock (start_paused) auto-advances when idle, so the configured
    // 5s deadline fires deterministically with no real wall-clock spent.
    let mut conn = McpConnection::new_with_timeout(HangingTransport, Duration::from_secs(5));
    let err = conn.list_tools().await.unwrap_err();
    assert!(
        err.to_string().contains("timed out awaiting `tools/list`"),
        "{err}"
    );
}
