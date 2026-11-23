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
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
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
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
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
            fs_write: newt_policy::caveats::Scope::none(),
            ..Caveats::top()
        };
        let mut transport = StdioTransport::spawn(&admitted, &caveats).unwrap();
        // With `confine` the restricted `fs_write` is kernel-enforced; without it the
        // server is spawned advisory, with the same environment.
        #[cfg(feature = "confine")]
        assert_ne!(transport.sandbox_kind, SandboxKind::None);
        #[cfg(not(feature = "confine"))]
        assert_eq!(transport.sandbox_kind, SandboxKind::None);
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

/// Grounds the advisory spawn's descriptor hygiene (#3160). A descriptor the parent holds with
/// close-on-exec cleared is inherited across exec by a plain spawn, and an inherited descriptor
/// is a capability the child was never granted (agent-bridle#319). The confined spawn closes it;
/// without `confine` there is no bridle to do that, so the advisory spawn calls
/// `newt_fdguard::deny_inherited_fds`, and this test fails when that call is missing.
///
/// The oracle is the child's own descriptor table, listed by `ls`, never a shell redirection:
/// a shell answers `>&N` from descriptors of its own (newt-fdguard's `tests/hostile_fds.rs`
/// says why). The descriptor sits above everything this process holds, so the child opens no
/// descriptor with its number. Needs a kernel with `close_range` (Linux 5.11), as the confined
/// spawn does.
///
/// ```text
/// cargo test -p newt-mcp-client --no-default-features --lib -- --ignored --test-threads=1 \
///   lean_stdio_spawn_does_not_inherit_an_ambient_descriptor
/// ```
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(not(feature = "confine"))]
#[tokio::test]
#[ignore = "real stdio subprocess; Linux and macOS weekly/release lane, single-threaded"]
async fn lean_stdio_spawn_does_not_inherit_an_ambient_descriptor() {
    use rustix::io::{fcntl_dupfd_cloexec, fcntl_getfd, fcntl_setfd, FdFlags};
    use std::os::fd::AsRawFd;

    let null = std::fs::File::open("/dev/null").expect("open /dev/null");
    let ambient = fcntl_dupfd_cloexec(null, 100).expect("duplicate above the descriptors held");
    fcntl_setfd(&ambient, FdFlags::empty()).expect("clear close-on-exec");
    // The control: the descriptor is inheritable, so a spawn that keeps it passes it on.
    assert!(
        fcntl_getfd(&ambient).expect("read the flags").is_empty(),
        "the descriptor must not be close-on-exec"
    );
    let number = ambient.as_raw_fd().to_string();
    #[cfg(target_os = "linux")]
    let table = "/proc/self/fd";
    #[cfg(target_os = "macos")]
    let table = "/dev/fd";
    let entry = McpServerEntry {
        name: "fd-probe".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("ls".into()),
        args: vec![table.into()],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    let admitted = newt_core::mcp::admit(&entry).expect("trusted test entry admits");
    let mut transport = StdioTransport::spawn(&admitted, &Caveats::top()).expect("spawn");
    let mut listed = Vec::new();
    while let Some(line) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        transport.stdout.next_line(),
    )
    .await
    .expect("the listing must finish")
    .expect("listing output")
    {
        listed.push(line);
    }
    // The oracle reads a real table: stdio, and the directory `ls` itself opened.
    for stdio in ["0", "1", "2"] {
        assert!(
            listed.iter().any(|line| line == stdio),
            "no descriptor {stdio} in {listed:?}"
        );
    }
    assert!(
        listed
            .iter()
            .any(|line| line.parse::<i32>().is_ok_and(|n| n >= 3)),
        "no descriptor above stdio in {listed:?}"
    );
    assert!(
        !listed.contains(&number),
        "descriptor {number}, left open by the parent, reached the advisory child: {listed:?}"
    );
}

/// Regression for the agent-bridle 0.8 upgrade: a `net: none` stdio MCP
/// server — the common/default posture, no `net` allowlist configured — must
/// still be SPAWNABLE. Before `StdioTransport::spawn` set `child_network:
/// DenyDirect` on its `ConfinedCommand`, agent-bridle 0.8's L3 admission
/// bound refused every such spawn outright ("backend authority on the Net
/// axis is not decidable ... (L3 BOUND)"), which is exactly what broke
/// `newt doctor`'s MCP server listing in CI.
#[cfg(all(target_os = "linux", feature = "confine"))]
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
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    let caveats = Caveats {
        net: newt_policy::caveats::Scope::none(),
        ..Caveats::top()
    };
    StdioTransport::spawn(&admitted, &caveats)
        .expect("a net:none stdio MCP server must spawn (not L3-BOUND-refused)");
}

/// #2596 round 3: a HOST-SCOPED `net` allowlist (e.g. a session net grant
/// widened from a `web_fetch` prompt) must not L3-BOUND-refuse a stdio MCP
/// spawn on Linux either. Fixed by `spawn_caveats` narrowing the spawn's
/// `net` caveat to `none` before admission (`spawn_caveats` calls
/// `caveats::spawn_net_scope`, exercised here through the real
/// `StdioTransport::spawn` call path — `Scope::Only(_)` is `Unknown` to
/// Landlock and refuses fail-closed; `none` under `DenyDirect` is
/// `Kernel`-decidable). A stdio child was never able to enforce a host
/// allow-list anyway (Landlock's net rule is port-based, not
/// hostname-based; ADR 0015), so this loses nothing a spawn could actually
/// honor — it only fixes admission for a shape that previously ran
/// advisory pre-0.8 and started refusing outright under 0.8 (previously
/// pinned by this same test as a "known limitation"; fixed here in newt
/// rather than in bridle, since bridle correctly has no way to know a
/// spawned child can't bind a hostname allow-list).
#[cfg(all(target_os = "linux", feature = "confine"))]
#[tokio::test]
#[ignore = "real confined stdio subprocess; integration tier"]
async fn stdio_spawn_under_a_host_scoped_net_grant_is_narrowed_and_not_l3_bound_refused() {
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
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    let caveats = Caveats {
        net: newt_policy::caveats::Scope::only(["api.github.com".to_string()]),
        ..Caveats::top()
    };
    StdioTransport::spawn(&admitted, &caveats).expect(
        "a host-scoped net grant must be narrowed to net:none for the spawn, \
         not L3-BOUND-refused",
    );
}

/// #3033 GROUNDING TEST (macOS, weekly/release lane). Grounds the mocked
/// `confined_spawn_helper_tests::mcp_spawn_declares_audited_piped_piped_null_stdio`:
/// with the stdio declared `ConfinedStdio::Piped/Piped/Null` by name, a real
/// stdio MCP child under `net:none` passes Seatbelt admission, and the report
/// for the mechanism it ran under says net `Kernel`. Admission refuses an
/// `Unknown` net projection regardless of floor, so the spawn succeeding is
/// itself the witness; before #3033 (raw `Stdio`, i.e. `ConfinedStdio::Other`)
/// both shapes below were refused. The host-list session shape is the one
/// operators hit: `spawn_caveats` narrows it to `net:none` before admission.
/// Requires an unprivileged caller (bridle's Seatbelt witness precondition).
///
/// ```text
/// cargo test -p newt-mcp-client --lib -- --ignored --test-threads=1 \
///   stdio_spawn_under_net_none_is_admitted_with_net_kernel_on_seatbelt
/// ```
#[cfg(all(target_os = "macos", feature = "confine"))]
#[tokio::test]
#[ignore = "real Seatbelt stdio subprocess; macOS weekly/release lane, single-threaded"]
async fn stdio_spawn_under_net_none_is_admitted_with_net_kernel_on_seatbelt() {
    use agent_bridle::{enforcement_report, AxisEnforcement, ConfinementMechanism, StdioPosture};
    use newt_policy::caveats::Scope;
    let entry = McpServerEntry {
        name: "seatbelt-net-none-probe".into(),
        enabled: true,
        transport: TransportKind::Stdio,
        command: Some("/bin/cat".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: newt_core::mcp::McpTrust::Trusted,
        origin: None,
    };
    let admitted = newt_core::mcp::admit(&entry).unwrap();
    for session_net in [Scope::none(), Scope::only(["api.github.com".to_string()])] {
        let caveats = Caveats {
            net: session_net.clone(),
            ..Caveats::top()
        };
        let t = StdioTransport::spawn(&admitted, &caveats).unwrap_or_else(|e| {
            panic!("session net {session_net:?}: stdio MCP must be admitted under net:none: {e:#}")
        });
        assert_eq!(t.sandbox_kind(), SandboxKind::Seatbelt);
        let spawned = spawn_caveats(&caveats, "/bin/cat");
        // Bridle's kind, which `ConfinementMechanism` takes: the assertion above is that it is
        // the one the child ran under.
        let mechanism = ConfinementMechanism::new(
            agent_bridle::SandboxKind::Seatbelt,
            ChildNetworkPolicy::DenyDirect,
        )
        .with_stdio_posture(StdioPosture::Audited);
        assert_eq!(
            enforcement_report(&spawned, mechanism).net,
            Some(AxisEnforcement::Kernel),
            "session net {session_net:?}: the admitted spawn's net axis must be Kernel"
        );
    }
}
