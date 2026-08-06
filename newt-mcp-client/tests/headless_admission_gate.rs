//! step-1.1 grounding test: the **headless planner** ([`McpToolset::connect`])
//! must never *spawn* an untrusted MCP server. Headless has no interactive
//! approval path, so a repo-shipped `.mcp.json` / project-overlay entry (marked
//! [`McpTrust::Untrusted`] by `newt_core::mcp::discover`) has to be refused at
//! the admission gate — **before** any subprocess exists.
//!
//! This is the real-resource tier (see `CLAUDE.md` "Testing strategy"): it
//! spawns a real subprocess and touches a real file, so it is `#[ignore]`d out
//! of the mocked per-PR unit run and runs on the integration lane. It **grounds**
//! the mocked `newt_core::mcp::admit` unit test
//! (`admit_denies_untrusted_and_disabled_admits_trusted`): the unit test proves
//! the gate *decides* deny, this proves the wired planner *acts* on that
//! decision by never launching the process.
//!
//! Why a side-effect marker rather than `is_empty()`: an untrusted entry with a
//! bogus command would fail to *initialize* even if it were spawned, leaving the
//! toolset empty either way — so `is_empty()` cannot tell "gate blocked the
//! spawn" from "spawn happened, handshake failed". The command here `touch`es a
//! marker as its first act; if the planner ever reaches `connect_stdio`, the
//! marker appears. The gate is proven only by the marker's **absence**.
//!
//! Regression: before step-1.1 the headless planner connected *every* discovered
//! entry, so this command would run and the marker would exist — this test would
//! have failed. With the gate, the marker never appears.

#![cfg(unix)]

use std::collections::BTreeMap;

use newt_core::caveats::Caveats;
use newt_core::mcp::{McpServerEntry, McpTrust, TransportKind};
use newt_mcp_client::McpToolset;

#[tokio::test]
#[ignore = "spawns a real subprocess + touches fs (integration tier); grounds the mocked admit() gate"]
async fn headless_planner_never_spawns_an_untrusted_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A workspace with NO `.mcp.json`, so the ONLY entry is the untrusted one we
    // hand the planner directly.
    let workspace = dir.path().join("ws");
    std::fs::create_dir(&workspace).expect("mkdir ws");
    let marker = dir.path().join("SPAWNED_MARKER");

    // An UNTRUSTED stdio entry (as a cloned repo's `.mcp.json` would produce).
    // Its command's first act is to create `marker` — an observable proof the
    // process was launched. `discover()` preserves the Untrusted mark, so the
    // gate must refuse it before `connect_stdio` ever runs the command.
    let evil = McpServerEntry {
        name: "evil".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("/bin/sh".into()),
        args: vec!["-c".into(), format!("touch {}; exec cat", marker.display())],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        trust: McpTrust::Untrusted,
    };

    // `Caveats::top()` = no confinement restriction, so if the planner spawned
    // the command the `touch` WOULD succeed. That isolates the admission gate as
    // the sole reason the marker is absent (not an OS sandbox blocking `touch`).
    let toolset = McpToolset::connect(
        workspace.to_str().unwrap(),
        std::slice::from_ref(&evil),
        false,
        &Caveats::top(),
    )
    .await;

    assert!(
        !marker.exists(),
        "admission gate breached: the untrusted server's command ran (marker created) — \
         the headless planner spawned an unadmitted process"
    );
    assert!(
        toolset.summary().iter().all(|(name, _)| name != "evil"),
        "an untrusted server must never appear in the connected toolset"
    );
}
