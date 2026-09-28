use super::*;
use newt_core::mcp::{McpServerEntry, TransportKind};

// A real subprocess is the ONLY way to observe env leakage (this is the
// security boundary, not mockable logic) — kept out of the mocked unit
// tier by #[ignore]; run explicitly / on the integration lane.
#[tokio::test]
#[ignore = "spawns a real `sh` subprocess (integration tier)"]
async fn stdio_spawn_does_not_leak_secret_env() {
    // A secret in newt's environment must NOT reach the child.
    std::env::set_var("LEAKY_SECRET_TOKEN", "sk-should-not-appear");
    let entry = McpServerEntry {
        name: "envprobe".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("sh".into()),
        args: vec!["-c".into(), "env; sleep 0.1".into()],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    // top() = advisory leash: `sh` is permitted (exec unrestricted) and the
    // env is still scrubbed to the explicit grants, so this validates the
    // confined path's env isolation without a fail-closed on a restricted axis.
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry admits");
    let mut t = StdioTransport::spawn(&admitted, &Caveats::top()).expect("spawn");
    let mut leaked = false;
    let mut saw_path = false;
    while let Ok(Some(line)) = t.stdout.next_line().await {
        if line.starts_with("LEAKY_SECRET_TOKEN=") {
            leaked = true;
        }
        if line.starts_with("PATH=") {
            saw_path = true;
        }
    }
    assert!(
        !leaked,
        "secret env leaked into the stdio MCP subprocess (#1155)"
    );
    assert!(saw_path, "PATH should be passed so the child can exec");
}

/// Grounds the mocked timezone allowlist/assembly contract with a real stdio
/// server process. Run serially: the parent environment is process-global.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
#[ignore = "real confined stdio subprocess; integration tier, single-threaded"]
async fn stdio_timezone_preserves_parent_and_server_override() {
    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    let _restore = Restore(
        ["TZ", "NEWT_CONFIG_DIR"]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect(),
    );
    let config = tempfile::tempdir().unwrap();
    std::env::set_var("NEWT_CONFIG_DIR", config.path());
    let mut entry = McpServerEntry {
        name: "timezone-probe".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("/bin/sh".into()),
        args: vec!["-c".into(), "printf '%s:%s\\n' \"${TZ+x}\" \"$TZ\"".into()],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        login_argv: Vec::new(),
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    for (parent, override_value, expected) in [
        (Some("UTC-14"), None, "x:UTC-14"),
        (Some(""), None, "x:"),
        (None, None, ":"),
        (Some("UTC-14"), Some(""), "x:"),
    ] {
        match parent {
            Some(timezone) => std::env::set_var("TZ", timezone),
            None => std::env::remove_var("TZ"),
        }
        entry.env.clear();
        if let Some(timezone) = override_value {
            entry
                .env
                .insert("TZ".into(), newt_core::mcp::SecretValue::literal(timezone));
        }
        let admitted = newt_core::mcp::admit(&entry).unwrap();
        let caveats = Caveats {
            fs_write: newt_core::caveats::Scope::none(),
            ..Caveats::top()
        };
        let mut transport = StdioTransport::spawn(&admitted, &caveats).unwrap();
        assert_ne!(transport.sandbox_kind, agent_bridle::SandboxKind::None);
        let line = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            transport.stdout.next_line(),
        )
        .await
        .expect("timezone probe must finish")
        .expect("timezone output")
        .expect("timezone line");
        assert_eq!(line, expected);
    }
}

/// Regression for the agent-bridle 0.8 upgrade: a `net: none` stdio MCP
/// server — the common/default posture, no `net` allowlist configured — must
/// still be SPAWNABLE. Before `StdioTransport::spawn` set `child_network:
/// DenyDirect` on its `ConfinedCommand`, agent-bridle 0.8's L3 admission
/// bound refused every such spawn outright ("backend authority on the Net
/// axis is not decidable ... (L3 BOUND)"), which is exactly what broke
/// `newt doctor`'s MCP server listing in CI.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "real confined stdio subprocess; integration tier"]
async fn stdio_spawn_under_net_none_does_not_l3_bound_refuse() {
    let entry = McpServerEntry {
        name: "net-none-probe".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("true".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    let caveats = Caveats {
        net: newt_core::caveats::Scope::none(),
        ..Caveats::top()
    };
    StdioTransport::spawn(&admitted, &caveats)
        .expect("a net:none stdio MCP server must spawn (not L3-BOUND-refused)");
}

/// **KNOWN LIMITATION, not fixed by this PR** — a bridle-side gap, tracked for
/// the maintainer, not a newt-mcp-client defect: on Linux, a stdio MCP server
/// under a HOST-SCOPED `net` allowlist (the egress-proxy-eligible shape) is
/// now refused outright, where pre-0.8 it ran (net axis advisory/unconfined —
/// Landlock's net rule is port-based, not hostname-based, so it never could
/// bound a host allow-list; ADR 0015). `ChildNetworkPolicy::DenyDirect` does
/// NOT help here — by its own doc contract it only engages when `net` is
/// ALREADY deny-all (a granted scope leaves it inert), confirmed structurally
/// in `LandlockSandbox::resolved_authority`
/// (agent-bridle-core-0.8.0-rc.4/src/sandbox.rs:1429): every restricted `net`
/// axis OTHER than `net: none` under `DenyDirect` resolves `Unknown` and the
/// L3 admission bound fails closed. The Linux enabler (a netns egress fence)
/// is deferred, separately tracked. This test PINS today's honest, if worse,
/// behavior so a future bridle release that closes the gap is a visible test
/// flip, not a silent regression.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "real confined stdio subprocess; integration tier"]
async fn stdio_spawn_under_a_host_scoped_net_grant_is_l3_bound_refused_on_linux() {
    let entry = McpServerEntry {
        name: "net-grant-probe".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("true".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    let caveats = Caveats {
        net: newt_core::caveats::Scope::only(["api.github.com".to_string()]),
        ..Caveats::top()
    };
    let err = match StdioTransport::spawn(&admitted, &caveats) {
        Ok(_) => {
            panic!("expected a host-scoped net grant to be L3-BOUND-refused on Linux (bridle gap)")
        }
        Err(e) => e,
    };
    assert!(
        format!("{err:#}").contains("L3 BOUND"),
        "expected the L3 admission bound's refusal, got: {err:#}"
    );
}
