//! Live UAT (#1243 Leg 3, #1180): a REAL stdio MCP server, spawned **confined**
//! through [`newt_mcp_client::connect_stdio`], connects, lists its tools, and
//! answers a tool call — proving the `agent_bridle::ConfinedCommand::spawn_tokio`
//! boundary preserves a working JSON-RPC stdio transport end-to-end.
//!
//! This is the UAT tier (see `CLAUDE.md` "Testing strategy"): it spawns a real
//! subprocess, so it is `#[ignore]`d out of the mocked per-PR unit run and runs
//! on the integration lane / by hand. It takes the server command from an env
//! var so **no host path is baked into the repo**.
//!
//! Run against a minimal (scrybe) or maximal (modulex-mcp) server, e.g.:
//! ```text
//! NEWT_UAT_MCP_CMD=/path/to/modulex-mcp \
//!   cargo test -p newt-mcp-client --test live_confined_mcp -- --ignored --nocapture
//! ```
//! Optional: `NEWT_UAT_MCP_ARGS="stdio"` (space-split), `NEWT_UAT_MCP_NAME=scrybe`.

use newt_core::caveats::Caveats;
use newt_core::mcp::{McpServerEntry, TransportKind};
use std::collections::BTreeMap;

fn entry_from_env() -> Option<McpServerEntry> {
    let command = std::env::var("NEWT_UAT_MCP_CMD").ok()?;
    let args = std::env::var("NEWT_UAT_MCP_ARGS")
        .ok()
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let name = std::env::var("NEWT_UAT_MCP_NAME").unwrap_or_else(|_| "uat".to_string());
    Some(McpServerEntry {
        name,
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some(command),
        args,
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
    })
}

#[tokio::test]
#[ignore = "spawns a REAL MCP server subprocess (UAT tier); set NEWT_UAT_MCP_CMD"]
async fn confined_stdio_server_connects_lists_and_calls() {
    let Some(entry) = entry_from_env() else {
        eprintln!("skipping: set NEWT_UAT_MCP_CMD to a stdio MCP server binary");
        return;
    };

    // Advisory (top) leash: the server runs *inside* the ConfinedCommand boundary
    // (exec admission-check + env scrub + OS sandbox machinery) but with nothing
    // restricted, so it connects regardless of host sandbox availability. Kernel
    // ENFORCEMENT of a restricted axis is proven by agent-bridle's own Landlock
    // child tests; here the point is that the confined transport works E2E.
    let caveats = Caveats::top();

    // step-1.1: connect through the admission gate (the test entry is trusted).
    let admitted = newt_core::mcp::admit(&entry).expect("test entry is trusted → admitted");
    let mut connected = newt_mcp_client::connect_stdio(&admitted, &caveats)
        .await
        .expect("confined connect_stdio should spawn + initialize + list tools");

    println!(
        "connected `{}` — {} tool(s): {}",
        connected.name,
        connected.tools.len(),
        connected
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        !connected.tools.is_empty(),
        "a real MCP server should advertise at least one tool over the confined transport"
    );

    // Call a tool to prove request/response round-trips over the confined pipes.
    // Prefer a plausibly no-arg "list"-style tool; else the first advertised one.
    let tool = connected
        .tools
        .iter()
        .find(|t| t.name.contains("list"))
        .or_else(|| connected.tools.first())
        .map(|t| t.name.clone())
        .expect("at least one tool");

    let res = connected.conn.call_tool(&tool, serde_json::json!({})).await;
    match &res {
        Ok(v) => println!("tool `{tool}` returned OK: {v}"),
        Err(e) => println!("tool `{tool}` returned a server error: {e}"),
    }
    // Either outcome proves the CONFINED transport carried the call to the server
    // and carried a reply back. A pure Ok is a clean success; a "server error on
    // `tools/call`" means the server received it and replied (only the args were
    // wrong) — both are end-to-end round-trips. A TRANSPORT failure (timeout /
    // closed pipe / spawn error) would be a different error and fail here.
    let ok_or_server_reply = res.is_ok()
        || res
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("server error on");
    assert!(
        ok_or_server_reply,
        "tool call must round-trip over the confined transport (got a transport-level failure)"
    );
}
