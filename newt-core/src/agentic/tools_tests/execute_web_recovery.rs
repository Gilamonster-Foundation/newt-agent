use super::*;

struct CatalogOnlyMcp(Vec<serde_json::Value>);

#[async_trait::async_trait]
impl McpTools for CatalogOnlyMcp {
    fn handles(&self, _name: &str) -> bool {
        false
    }

    fn tool_defs(&self) -> Vec<serde_json::Value> {
        self.0.clone()
    }

    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        "catalog-only MCP must not be called".to_string()
    }
}

/// web_fetch with a gate: an out-of-allowlist host consults the gate
/// with the parsed host; on deny the dispatch runs under the ORIGINAL
/// caveats, so the leash produces today's denial (an `error:` result —
/// nothing is fetched).
#[tokio::test]
async fn web_fetch_gate_deny_dispatches_under_original_caveats() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path()); // net: Scope::none()
    let mut gate = MockGate::new(false, &caveats);
    let out = run_tool_gated(
        "web_fetch",
        serde_json::json!({"url": "https://denied.example.com:8443/page"}),
        ws.path(),
        &caveats,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("error:"), "leash denial surfaces: {out}");
    assert_eq!(
        gate.asks,
        vec![(
            "web_fetch".to_string(),
            "net:denied.example.com".to_string()
        )]
    );
}

/// Regression for the field report: github.com is outside the default net
/// scope, so a TUI-provided gate must be consulted before the bridle leash
/// returns the denial to the model.
#[tokio::test]
async fn web_fetch_github_denial_consults_permission_gate() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path()); // net: Scope::none()
    let mut gate = MockGate::new(false, &caveats);
    let out = run_tool_gated(
        "web_fetch",
        serde_json::json!({"url": "https://github.com/openai/codex"}),
        ws.path(),
        &caveats,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("error:"), "leash denial surfaces: {out}");
    assert_eq!(
        gate.asks,
        vec![("web_fetch".to_string(), "net:github.com".to_string())]
    );
}

/// An unparseable URL skips the net pre-check entirely — the gate is
/// never consulted and the dispatch (with the original caveats) answers.
#[tokio::test]
async fn web_fetch_unparseable_url_never_prompts() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let mut gate = MockGate::new(true, &caveats);
    let out = run_tool_gated(
        "web_fetch",
        serde_json::json!({"url": "not-a-url"}),
        ws.path(),
        &caveats,
        &mut gate,
    )
    .await;
    assert!(out.starts_with("error:"), "got: {out}");
    assert!(gate.asks.is_empty(), "no prompt for an unparseable URL");
}

/// Field-regression: a private code-review URL may be intentionally blocked
/// by the raw-fetch SSRF policy while an authenticated MCP source is already
/// connected. The result must preserve the refusal and put catalog discovery
/// plus the namespaced connector ahead of shell/user-configuration fallbacks.
#[tokio::test]
async fn private_address_fetch_failure_routes_to_connected_mcp_first() {
    let mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    let error = "denied: SSRF block: \"reviews.example.test\" resolved to \
                     private/loopback address 10.0.0.1 (not in the net allowlist)";

    let url = "https://reviews.example.test/reviews/42";
    let out = render_web_fetch_error(url, error, &mcp, None, PromptDisposition::Act);

    assert!(out.starts_with(&format!("error: {error}")), "got: {out}");
    let discovery = out.find("`tool_search`").expect("discovery instruction");
    let connector = out
        .find("opaque_bridge__read_object")
        .expect("connected namespaced MCP tool");
    assert!(
        discovery < connector,
        "tool discovery must be presented before its MCP candidate: {out}"
    );
    assert!(
        out.contains("Do not fall back to `run_command`/curl or `request_user_input`"),
        "the two field-seen dead ends must be explicitly fenced: {out}"
    );

    // Exercise the instructed route against the same live catalog: discovery
    // returns the exact MCP name, and the dispatcher invokes that remote tool
    // under an explicit persona grant. No shell or human-input tool enters the
    // sequence.
    let catalog = callable_mcp_catalog(&mcp, None, PromptDisposition::Act);
    let discovered =
        crate::agentic::tool_search::execute_tool_search("opaque_bridge__read_object", &catalog);
    assert!(
        discovered.contains("opaque_bridge__read_object"),
        "tool_search must discover the connector: {discovered}"
    );
    let allowed = vec!["opaque_bridge__read_object".to_string()];
    let mut routed_mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    let result = run_remote_gated(
        "opaque_bridge__read_object",
        std::path::Path::new("."),
        &Caveats::top(),
        Some(&allowed),
        &mut routed_mcp,
        None,
    )
    .await;
    assert_eq!(result, "remote-tool-ran");
    assert!(routed_mcp.called, "the namespaced MCP route must dispatch");
}

/// HTTP authentication failures are structured successful transports in
/// agent-bridle. Newt must not feed their login/error body to the model as
/// page evidence; both statuses take the same MCP-first route.
#[test]
fn unauthorized_fetch_results_route_to_connected_mcp_not_error_body() {
    let mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    for status in [401_u64, 403] {
        let out = render_web_fetch_result(
            "https://reviews.example.test/reviews/42",
            &serde_json::json!({
                "status": status,
                "final_url": "https://reviews.example.test/login",
                "title": "Sign in",
                "markdown": "configure a local checkout client instead"
            }),
            &mcp,
            None,
            PromptDisposition::Act,
        );

        assert!(
            out.starts_with(&format!("error: web_fetch returned HTTP {status}")),
            "got: {out}"
        );
        assert!(out.contains("`tool_search`"), "missing discovery: {out}");
        assert!(
            out.contains("opaque_bridge__read_object"),
            "missing MCP route: {out}"
        );
        assert!(
            !out.contains("configure a local checkout client instead"),
            "an auth error body must not masquerade as review evidence: {out}"
        );
    }
}

#[test]
fn unauthorized_fetch_errors_also_route_to_connected_mcp() {
    let mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    for error in [
        "request failed with HTTP 401 Unauthorized",
        "request failed with HTTP status 403 Forbidden",
    ] {
        let out = render_web_fetch_error(
            "https://reviews.example.test/reviews/42",
            error,
            &mcp,
            None,
            PromptDisposition::Act,
        );
        assert!(out.starts_with(&format!("error: {error}")), "got: {out}");
        assert!(out.contains("`tool_search`"), "missing discovery: {out}");
        assert!(
            out.contains("opaque_bridge__read_object"),
            "missing MCP route: {out}"
        );
    }
}

/// Recovery is honest: without a callable MCP tool, Newt returns the raw
/// SSRF failure and does not claim that an authenticated route exists.
#[test]
fn private_address_fetch_without_mcp_keeps_original_failure() {
    let error = "denied: SSRF block: \"reviews.example.test\" resolved to \
                     private/loopback address 10.0.0.1 (not in the net allowlist)";
    let out = render_web_fetch_error(
        "https://reviews.example.test/reviews/42",
        error,
        &NoMcp,
        None,
        PromptDisposition::Act,
    );
    assert_eq!(out, format!("error: {error}"));
}

#[test]
fn private_address_fetch_with_undeclared_mcp_offers_non_authoritative_discovery() {
    let error = "denied: SSRF block: private address";
    // The name deliberately looks relevant. Without explicit URL affinity
    // it is only a connected-catalog discovery candidate, never an asserted
    // authenticated route.
    let mcp = OneRemoteTool::new("reviews_source__get_review");
    let out = render_web_fetch_error(
        "https://reviews.example.test/reviews/42",
        error,
        &mcp,
        None,
        PromptDisposition::Act,
    );
    assert!(out.starts_with(&format!("error: {error}")), "got: {out}");
    assert!(out.contains("non-authoritative discovery"), "got: {out}");
    assert!(out.contains("reviews_source__get_review"), "got: {out}");
    assert!(out.contains("discovery only"), "got: {out}");
    assert!(
        out.contains("do not assume that a candidate can read or authenticate"),
        "got: {out}"
    );
}

#[test]
fn discovery_query_uses_only_bounded_host_and_path_terms() {
    let url = reqwest::Url::parse(
        "https://review-broker.example.test/reviews/42?token=must-not-appear#fragment-secret",
    )
    .unwrap();
    let query = resource_url_discovery_query(&url);
    assert!(query.contains("reviews"), "got: {query}");
    assert!(query.contains("review"), "got: {query}");
    assert!(query.contains("broker"), "got: {query}");
    assert!(!query.contains("must"), "query value leaked: {query}");
    assert!(!query.contains("fragment"), "fragment leaked: {query}");
    assert!(query.split_whitespace().count() <= 8, "got: {query}");
}

#[test]
fn authoritative_recovery_lists_every_matching_tool_without_choosing_first() {
    let tools = ["review_source__get_review", "review_source__get_version"]
        .into_iter()
        .map(|name| {
            let mut definition = serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": "Read an authenticated review resource.",
                    "parameters": {"type": "object"}
                }
            });
            preserve_mcp_resource_url_affinity(
                &mut definition,
                Some(&serde_json::json!({
                    "newt/resourceUrlPrefixes": [
                        "https://reviews.example.test/reviews/"
                    ]
                })),
            );
            definition
        })
        .collect();
    let mcp = CatalogOnlyMcp(tools);
    let out = authenticated_url_recovery(
        "error: HTTP 401 Unauthorized".to_string(),
        "https://reviews.example.test/reviews/42",
        &mcp,
        None,
        PromptDisposition::Act,
    );

    assert!(out.contains("explicitly declares one or more URL-affine tools"));
    assert!(out.contains("review_source__get_review"), "got: {out}");
    assert!(out.contains("review_source__get_version"), "got: {out}");
    assert!(!out.contains("the exact candidate name"), "got: {out}");
}

#[test]
fn resource_affinity_requires_exact_origin_and_path_boundary() {
    for declared in [
        "https://reviews.example.test/reviews",
        "https://reviews.example.test:443/reviews/",
    ] {
        let prefix = resource_url_prefix(declared).unwrap();
        for matching in [
            "https://reviews.example.test/reviews",
            "https://reviews.example.test/reviews/42",
            "https://reviews.example.test/reviews/42?version=2",
        ] {
            let url = reqwest::Url::parse(matching).unwrap();
            assert!(
                resource_url_has_prefix(&url, &prefix),
                "expected {declared} to match {matching}"
            );
        }
        for unrelated in [
            "http://reviews.example.test/reviews/42",
            "https://reviews.example.test:444/reviews/42",
            "https://reviews.example.test/reviews-extra/42",
            "https://reviews.example.test.evil/reviews/42",
        ] {
            let url = reqwest::Url::parse(unrelated).unwrap();
            assert!(
                !resource_url_has_prefix(&url, &prefix),
                "must not overmatch {declared} against {unrelated}"
            );
        }
    }
}

#[test]
fn affinity_adapter_preserves_valid_declaration_and_wire_scrubs_metadata() {
    let mut definition = serde_json::json!({
        "type": "function",
        "function": {
            "name": "opaque_bridge__read_object",
            "description": "Retrieve an object.",
            "parameters": {"type": "object"}
        }
    });
    preserve_mcp_resource_url_affinity(
        &mut definition,
        Some(&serde_json::json!({
            "newt/resourceUrlPrefixes": [
                "https://reviews.example.test/reviews/"
            ],
            "unrelated/serverMetadata": "must not cross the provider wire"
        })),
    );
    assert_eq!(
        definition["_meta"][MCP_RESOURCE_URL_PREFIXES_META_KEY],
        serde_json::json!(["https://reviews.example.test/reviews/"])
    );
    assert!(definition["_meta"]
        .get("unrelated/serverMetadata")
        .is_none());

    strip_mcp_catalog_metadata(&mut definition);
    assert!(definition.get("_meta").is_none());
    assert_eq!(definition["function"]["name"], "opaque_bridge__read_object");
}

#[test]
fn affinity_declaration_is_a_strict_nonempty_array() {
    for malformed in [
        serde_json::json!([]),
        serde_json::json!("https://reviews.example.test/reviews/"),
        serde_json::json!(["https://reviews.example.test/reviews/", 7]),
        serde_json::json!(["https://reviews.example.test/reviews/", "/reviews/42"]),
        serde_json::json!([" https://reviews.example.test/reviews/"]),
        serde_json::json!(["https://user:secret@reviews.example.test/reviews/"]),
        serde_json::json!(["https://reviews.example.test/reviews/?token=secret"]),
        serde_json::json!(["file:///tmp/reviews/"]),
    ] {
        let meta = serde_json::json!({
            "newt/resourceUrlPrefixes": malformed
        });
        let mut definition = serde_json::json!({
            "type": "function",
            "function": {
                "name": "opaque_bridge__read_object",
                "description": "Retrieve an object.",
                "parameters": {"type": "object"}
            }
        });
        preserve_mcp_resource_url_affinity(&mut definition, Some(&meta));
        assert!(
            definition.get("_meta").is_none(),
            "malformed declaration must add no affinity: {meta}"
        );

        let raw = serde_json::json!({
            "type": "function",
            "function": definition["function"].clone(),
            "_meta": meta
        });
        let url = reqwest::Url::parse("https://reviews.example.test/reviews/42").unwrap();
        assert!(
            !tool_declares_resource_url(&raw, &url),
            "raw malformed metadata must not bypass the adapter"
        );
    }
}

#[test]
fn names_and_descriptions_never_infer_resource_affinity() {
    let decoy = serde_json::json!({
        "type": "function",
        "function": {
            "name": "reviews_source__get_review",
            "description": "Read https://reviews.example.test/reviews/42",
            "parameters": {"type": "object"}
        }
    });
    let url = reqwest::Url::parse("https://reviews.example.test/reviews/42").unwrap();
    assert!(!tool_declares_resource_url(&decoy, &url));
}

#[test]
fn merged_model_catalog_scrubs_affinity_but_recovery_catalog_retains_it() {
    let mcp = OneRemoteTool::new("opaque_bridge__read_object")
        .with_resource_url_prefixes(&["https://reviews.example.test/reviews/"]);
    let recovery_catalog = callable_mcp_catalog(&mcp, None, PromptDisposition::Act);
    assert_eq!(
        recovery_catalog[0]["_meta"][MCP_RESOURCE_URL_PREFIXES_META_KEY],
        serde_json::json!(["https://reviews.example.test/reviews/"])
    );

    let model_catalog = merged_tool_definitions(
        &mcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    let remote = model_catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "opaque_bridge__read_object")
        .expect("remote tool remains advertised after metadata scrubbing");
    assert!(remote.get("_meta").is_none());
}

/// Ordinary public content and unrelated transport errors keep their prior
/// behavior; the MCP route is specific to private-address/auth failures.
#[test]
fn ordinary_web_fetch_results_do_not_gain_mcp_recovery() {
    let mcp = OneRemoteTool::new("review_source__get_review");
    let ok = render_web_fetch_result(
        "https://docs.example.test/page",
        &serde_json::json!({
            "status": 200,
            "final_url": "https://docs.example.test/page",
            "title": "Guide",
            "markdown": "public content"
        }),
        &mcp,
        None,
        PromptDisposition::Act,
    );
    assert_eq!(
        ok,
        "# Guide\nhttps://docs.example.test/page\n\npublic content"
    );
    assert!(!ok.contains("tool_search"));

    let timeout = render_web_fetch_error(
        "https://docs.example.test/page",
        "denied: request to \"docs.example.test\" timed out",
        &mcp,
        None,
        PromptDisposition::Act,
    );
    assert_eq!(
        timeout,
        "error: denied: request to \"docs.example.test\" timed out"
    );
}
