//! Integration tests for the wired `goal_run` MCP tool.
//!
//! These tests exercise the public surface of the MCP server end-to-end:
//! they build an `McpServer`, wire in a [`BackendRegistry`] (with mock
//! backends from `tests-common`), drive the server via in-memory async
//! streams, and assert on the JSON-RPC response envelope.
//!
//! The contract being pinned:
//!   1. `prompt` is required — missing it surfaces as a JSON-RPC
//!      `-32603` error.
//!   2. `tier` is optional; if present it must be one of FAST /
//!      STANDARD / COMPLEX / REVIEW (case-insensitive). Bogus values
//!      surface as errors.
//!   3. With no backend registered for the routed tier, the registry's
//!      `NoBackendForTier` propagates as a JSON-RPC error rather than
//!      a successful empty reply.
//!   4. A registered backend's reply text reaches the caller, wrapped
//!      in the MCP content envelope and prefixed with the model id.
//!   5. An explicit `tier` argument overrides the router and picks the
//!      backend that supports that tier specifically.

use std::sync::Arc;

use newt_core::router::Tier;
use newt_core::Router;
use newt_inference::BackendRegistry;
use newt_mcp_client::McpToolset;
use newt_mcp_server::handlers::register_handlers;
use newt_mcp_server::server::McpServer;
use serde_json::Value;
use tests_common::MockBackend;
use tokio::sync::Mutex;

/// Drive a single JSON-RPC request through a freshly-built server. No
/// persona (empty toolset, unrestricted catalog) — these tests are about
/// `goal_run`, not #1021's persona restriction.
async fn rpc_with(registry: Arc<BackendRegistry>, router: Arc<Router>, request: &Value) -> Value {
    let mut server = McpServer::new();
    register_handlers(
        &mut server,
        registry,
        router,
        Arc::new(Mutex::new(McpToolset::empty())),
        Arc::new(None),
    );

    let input = format!("{}\n", serde_json::to_string(request).unwrap());
    let mut output: Vec<u8> = Vec::new();
    server.run(input.as_bytes(), &mut output).await.unwrap();
    let text = String::from_utf8(output).unwrap();
    serde_json::from_str(text.trim()).unwrap()
}

/// Build a goal_run request body with the given arguments.
fn goal_run_request(id: i64, args: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "goal_run",
            "arguments": args,
        }
    })
}

/// Bullet 1: missing `prompt` surfaces as a JSON-RPC error.
#[tokio::test]
async fn goal_run_missing_prompt_errors() {
    let resp = rpc_with(
        Arc::new(BackendRegistry::new()),
        Arc::new(Router::new()),
        &goal_run_request(1, serde_json::json!({})),
    )
    .await;

    assert!(resp["error"].is_object(), "expected error, got: {resp}");
    assert_eq!(resp["error"]["code"], -32603);
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(msg.contains("prompt"), "expected 'prompt' in error: {msg}");
}

/// Bullet 2: an unrecognized `tier` string is rejected before the
/// backend is even consulted.
#[tokio::test]
async fn goal_run_invalid_tier_errors() {
    let mut registry = BackendRegistry::new();
    registry.register(Arc::new(MockBackend::all_tiers("mock", "should not run")));

    let resp = rpc_with(
        Arc::new(registry),
        Arc::new(Router::new()),
        &goal_run_request(
            2,
            serde_json::json!({
                "prompt": "hello",
                "tier": "BOGUS",
            }),
        ),
    )
    .await;

    assert!(resp["error"].is_object(), "expected error, got: {resp}");
    assert_eq!(resp["error"]["code"], -32603);
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("invalid tier") && msg.contains("BOGUS"),
        "expected invalid-tier error mentioning BOGUS, got: {msg}"
    );
}

/// Bullet 3: with an empty registry, `NoBackendForTier` propagates as
/// a JSON-RPC error.
#[tokio::test]
async fn goal_run_no_backend_errors() {
    let resp = rpc_with(
        Arc::new(BackendRegistry::new()),
        Arc::new(Router::new()),
        &goal_run_request(3, serde_json::json!({ "prompt": "hi" })),
    )
    .await;

    assert!(resp["error"].is_object(), "expected error, got: {resp}");
    assert_eq!(resp["error"]["code"], -32603);
    let msg = resp["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("no backend") || msg.contains("NoBackendForTier"),
        "expected NoBackendForTier-like error, got: {msg}"
    );
}

/// Bullet 4: a registered backend's reply reaches the caller.
#[tokio::test]
async fn goal_run_routes_to_backend() {
    let mut registry = BackendRegistry::new();
    registry.register(Arc::new(MockBackend::all_tiers("mock1", "rename complete")));

    let resp = rpc_with(
        Arc::new(registry),
        Arc::new(Router::new()),
        &goal_run_request(4, serde_json::json!({ "prompt": "rename foo to bar" })),
    )
    .await;

    assert!(
        resp["error"].is_null() || resp.get("error").is_none(),
        "expected success, got: {resp}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("rename complete"),
        "expected backend reply text, got: {text}"
    );
    // Reply is prefixed with model_id (MockBackend::all_tiers sets
    // model_id = "{name}-model").
    assert!(
        text.contains("mock1-model"),
        "expected model_id prefix in: {text}"
    );
}

/// Bullet 5: with two backends each restricted to a different tier,
/// an explicit `tier` argument routes to the correct one.
#[tokio::test]
async fn goal_run_tier_override_picks_specific_backend() {
    // Fast-only and Complex-only backends with distinguishable replies.
    let fast = MockBackend::new("fast-only", "fast-model", vec![Tier::Fast], "FAST_REPLY");
    let complex = MockBackend::new(
        "complex-only",
        "complex-model",
        vec![Tier::Complex],
        "COMPLEX_REPLY",
    );

    let mut registry = BackendRegistry::new();
    registry.register(Arc::new(fast));
    registry.register(Arc::new(complex));
    let registry = Arc::new(registry);

    // tier: COMPLEX must reach the complex backend even though the
    // router (looking at the short prompt "hi") would otherwise pick
    // FAST.
    let resp = rpc_with(
        registry.clone(),
        Arc::new(Router::new()),
        &goal_run_request(5, serde_json::json!({ "prompt": "hi", "tier": "COMPLEX" })),
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("COMPLEX_REPLY"),
        "expected complex backend reply, got: {text}"
    );
    assert!(
        text.contains("complex-model"),
        "expected complex backend model_id, got: {text}"
    );
    assert!(
        !text.contains("FAST_REPLY"),
        "did not expect fast backend reply, got: {text}"
    );

    // And the inverse: tier: FAST routes to the fast backend.
    let resp = rpc_with(
        registry,
        Arc::new(Router::new()),
        &goal_run_request(6, serde_json::json!({ "prompt": "hi", "tier": "FAST" })),
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("FAST_REPLY"),
        "expected fast backend reply, got: {text}"
    );
    assert!(
        text.contains("fast-model"),
        "expected fast backend model_id, got: {text}"
    );
}

/// Bonus: when no `tier` arg is supplied the router classifies the
/// prompt, and the registry picks the first backend supporting that
/// tier. Pin this so a future router change doesn't silently break
/// the "default route" path.
#[tokio::test]
async fn goal_run_uses_router_when_tier_absent() {
    // Router::classify("review this PR") returns Tier::Review.
    let review = MockBackend::new("review-only", "review-model", vec![Tier::Review], "graded");
    let mut registry = BackendRegistry::new();
    registry.register(Arc::new(review));

    let resp = rpc_with(
        Arc::new(registry),
        Arc::new(Router::new()),
        &goal_run_request(7, serde_json::json!({ "prompt": "review this PR" })),
    )
    .await;

    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("graded"),
        "expected review backend reply, got: {text}"
    );
}
