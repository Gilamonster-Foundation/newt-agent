use super::*;

// MCP configuration writers and the stdio environment boundary (owned by shell.rs).

#[test]
fn mcp_stdio_env_allowlist_excludes_secrets_and_is_closed() {
    // #1155: the stdio-MCP env allow-list must NOT be a passthrough of the
    // whole environment — secret-bearing vars are absent, and it stays a
    // superset of the shell default (a subprocess needs PATH to exec).
    let allow = mcp_stdio_env_passthrough();
    for secret in [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "AWS_SECRET_ACCESS_KEY",
        "GITHUB_TOKEN",
        "DGX_API_KEY",
        "NVIDIA_API_KEY",
        // The encrypted-token-store unlock channel (crate::secrets):
        // a child process must never inherit the vault passphrase.
        crate::secrets::PASSPHRASE_ENV,
    ] {
        assert!(!allow.contains(&secret), "{secret} must never be inherited");
    }
    assert!(allow.contains(&"PATH"), "a child needs PATH to exec");
    for base in shell_env_passthrough_default() {
        assert!(
            allow.contains(&base.as_str()),
            "{base} (shell default) should be covered"
        );
    }
}

// ---- #1149: /mcp enable|disable config writer ----

#[test]
fn with_mcp_enabled_toggles_and_preserves_comments() {
    let text = "# my config\n[[mcp_servers]]\nname = \"modulex\"\ncommand = \"modulex-mcp\"\n";
    // disable → enabled = false written, comment preserved
    let off = Config::with_mcp_enabled(text, "modulex", false).unwrap();
    assert!(off.contains("enabled = false"));
    assert!(off.contains("# my config"));
    // re-enable → key REMOVED (default is enabled; file stays minimal)
    let on = Config::with_mcp_enabled(&off, "modulex", true).unwrap();
    assert!(!on.contains("enabled"));
    // unknown name errors loudly
    assert!(Config::with_mcp_enabled(text, "nope", false).is_err());
    // entry parses with default enabled=true; explicit false honored
    let e: crate::mcp::McpServerEntry = toml::from_str("name = \"x\"\ncommand = \"x\"\n").unwrap();
    assert!(e.enabled);
    let d: crate::mcp::McpServerEntry =
        toml::from_str("name = \"x\"\ncommand = \"x\"\nenabled = false\n").unwrap();
    assert!(!d.enabled);
}

#[test]
fn with_mcp_server_merged_updates_imported_fields_and_keeps_local_fields() {
    let text = "# local MCP settings\n[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://old.example/mcp\"\nenabled = false\nlogin_argv = [\"client\", \"login\"]\nrequest_timeout_secs = 90\n";
    let incoming = crate::mcp::McpServerEntry {
        name: "review".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Http,
        command: None,
        args: Vec::new(),
        env: std::collections::BTreeMap::new(),
        url: Some("https://new.example/mcp/".into()),
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: crate::mcp::McpTrust::Untrusted,
        origin: None,
    };

    let updated = Config::with_mcp_server_merged(text, &incoming).unwrap();
    assert!(updated.contains("# local MCP settings"));
    assert!(updated.contains("url = \"https://new.example/mcp/\""));
    assert!(updated.contains("enabled = false"));
    assert!(updated.contains("login_argv = [\"client\", \"login\"]"));
    assert!(updated.contains("request_timeout_secs = 90"));
    let parsed: Config = toml::from_str(&updated).unwrap();
    assert_eq!(
        parsed.mcp_servers[0].url.as_deref(),
        Some("https://new.example/mcp/")
    );
    assert!(!parsed.mcp_servers[0].enabled);
    assert_eq!(parsed.mcp_servers[0].login_argv, vec!["client", "login"]);
    assert_eq!(parsed.mcp_servers[0].request_timeout_secs, Some(90));
}

#[test]
fn with_mcp_server_merged_is_a_noop_when_source_fields_match() {
    let text = "[[mcp_servers]]\nname = \"review\"\ncommand = \"review-mcp\"\n";
    let incoming: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ncommand = \"review-mcp\"\n").unwrap();
    assert_eq!(
        Config::with_mcp_server_merged(text, &incoming).unwrap(),
        text
    );
}

#[test]
fn with_mcp_server_merged_resets_a_changed_transport_to_default_stdio() {
    let text = "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://old.example/mcp\"\nheaders = { Authorization = \"Bearer old\" }\n";
    let incoming: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ncommand = \"review-mcp\"\n").unwrap();

    let updated = Config::with_mcp_server_merged(text, &incoming).unwrap();
    let parsed: Config = toml::from_str(&updated).unwrap();
    assert_eq!(
        parsed.mcp_servers[0].transport,
        crate::mcp::TransportKind::Stdio
    );
    assert_eq!(parsed.mcp_servers[0].command.as_deref(), Some("review-mcp"));
    assert!(parsed.mcp_servers[0].url.is_none());
    assert!(parsed.mcp_servers[0].headers.is_empty());
    assert!(!updated.contains("type ="));
}

#[test]
fn with_mcp_server_merged_removes_stdio_fields_when_switching_to_http() {
    let text = "[[mcp_servers]]\nname = \"review\"\ncommand = \"old-mcp\"\nargs = [\"serve\"]\nenv = { TOKEN = \"secret\" }\n";
    let incoming: crate::mcp::McpServerEntry =
        toml::from_str("name = \"review\"\ntype = \"http\"\nurl = \"https://new.example/mcp\"\n")
            .unwrap();

    let updated = Config::with_mcp_server_merged(text, &incoming).unwrap();
    let parsed: Config = toml::from_str(&updated).unwrap();
    let server = &parsed.mcp_servers[0];
    assert_eq!(server.transport, crate::mcp::TransportKind::Http);
    assert_eq!(server.url.as_deref(), Some("https://new.example/mcp"));
    assert!(server.command.is_none());
    assert!(server.args.is_empty());
    assert!(server.env.is_empty());
}

#[test]
fn with_mcp_server_removed_does_not_echo_malformed_config_contents() {
    let text = "[[mcp_servers]]\nname = \"private-marker\"\ncommand = =\n";
    let error = Config::with_mcp_server_removed(text, "private-marker").unwrap_err();
    assert!(error.to_string().contains("invalid MCP TOML syntax"));
    assert!(!error.to_string().contains("private-marker"));
}

#[test]
#[serial_test::serial(real_fs)]
fn set_mcp_server_enabled_in_file_uses_atomic_writeback() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    let dir = tempfile::tempdir().unwrap();
    let _sandbox = HomeSandbox::enter(dir.path());
    let path = dir.path().join("mcp.toml");
    let original =
        "# keep this comment\n[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\n";
    std::fs::write(&path, original).unwrap();

    Config::set_mcp_server_enabled_in_file(&path, "docs", false).unwrap();

    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("# keep this comment"));
    assert!(updated.contains("enabled = false"));
    assert!(!newt_core_lock_path(&path).exists());
}

fn newt_core_lock_path(path: &std::path::Path) -> std::path::PathBuf {
    crate::atomic_fs::ResolvedPath::resolve(path)
        .unwrap()
        .lock_path()
}

// ---- `newt mcp add|remove` comment-preserving config writers ----

#[test]
fn with_mcp_server_added_appends_and_preserves_comments() {
    let text = "\
# hand-authored config
default_backend = \"local\" # keep me

[[mcp_servers]]
name = \"modulex\"
command = \"modulex-mcp\"
";
    let entry = crate::mcp::McpServerEntry {
        name: "scrybe".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Stdio,
        command: Some("scrybe-mcp-server".into()),
        args: vec!["stdio".into()],
        env: std::collections::BTreeMap::from([(
            "SCRYBE_LOG".to_string(),
            crate::mcp::SecretValue::literal("info"),
        )]),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: Some(120),
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    let out = Config::with_mcp_server_added(text, &entry).unwrap();
    assert!(
        out.contains("# hand-authored config"),
        "comment lost: {out}"
    );
    assert!(out.contains("# keep me"), "inline comment lost: {out}");
    assert!(out.contains("modulex-mcp"), "existing entry lost: {out}");
    // Round-trips through the typed config with both entries intact.
    let cfg: Config = toml::from_str(&out).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 2);
    let added = cfg.mcp_servers.iter().find(|s| s.name == "scrybe").unwrap();
    assert_eq!(added.command.as_deref(), Some("scrybe-mcp-server"));
    assert_eq!(added.args, vec!["stdio"]);
    assert_eq!(
        added
            .env
            .get("SCRYBE_LOG")
            .and_then(crate::mcp::SecretValue::as_literal),
        Some("info")
    );
    assert_eq!(added.request_timeout_secs, Some(120));
    assert!(added.enabled);
    // Defaults stay implicit — the file stays minimal.
    assert!(!out.contains("enabled"), "default enabled written: {out}");
    assert!(!out.contains("type"), "default transport written: {out}");
}

#[test]
fn with_mcp_server_added_creates_section_in_empty_text() {
    let entry = crate::mcp::McpServerEntry {
        name: "fs".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Stdio,
        command: Some("mcp-fs".into()),
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    let out = Config::with_mcp_server_added("", &entry).unwrap();
    let cfg: Config = toml::from_str(&out).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(cfg.mcp_servers[0].name, "fs");
    assert_eq!(cfg.mcp_servers[0].command.as_deref(), Some("mcp-fs"));
}

#[test]
fn with_mcp_server_added_writes_sse_transport_and_url() {
    let entry = crate::mcp::McpServerEntry {
        name: "remote".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Sse,
        command: None,
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: Some("https://mcp.example/sse".into()),
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    let out = Config::with_mcp_server_added("", &entry).unwrap();
    let cfg: Config = toml::from_str(&out).unwrap();
    assert_eq!(cfg.mcp_servers[0].transport, crate::mcp::TransportKind::Sse);
    assert_eq!(
        cfg.mcp_servers[0].url.as_deref(),
        Some("https://mcp.example/sse")
    );
}

#[test]
fn with_mcp_server_added_rejects_duplicates_and_invalid_entries() {
    let text = "[[mcp_servers]]\nname = \"scrybe\"\ncommand = \"scrybe-mcp-server\"\n";
    let dup = crate::mcp::McpServerEntry {
        name: "scrybe".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Stdio,
        command: Some("other".into()),
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    let err = Config::with_mcp_server_added(text, &dup).unwrap_err();
    assert!(err.to_string().contains("scrybe"), "names the dup: {err}");

    // A stdio entry with no command / an sse entry with no url never lands
    // in the file — it could never connect (mcp::McpServerEntry::is_valid).
    let no_cmd = crate::mcp::McpServerEntry {
        name: "ghost".into(),
        command: None,
        ..dup.clone()
    };
    assert!(Config::with_mcp_server_added("", &no_cmd).is_err());
    let no_url = crate::mcp::McpServerEntry {
        name: "ghost".into(),
        transport: crate::mcp::TransportKind::Http,
        command: None,
        ..dup.clone()
    };
    assert!(Config::with_mcp_server_added("", &no_url).is_err());
    // An empty name can never be addressed again — reject it.
    let unnamed = crate::mcp::McpServerEntry {
        name: "  ".into(),
        ..dup.clone()
    };
    assert!(Config::with_mcp_server_added("", &unnamed).is_err());
}

#[test]
fn with_mcp_server_removed_deletes_only_the_named_entry() {
    let text = "\
# my config

[[mcp_servers]]
name = \"keep\"
command = \"keep-mcp\" # keep note

[[mcp_servers]]
name = \"drop\"
command = \"drop-mcp\"
";
    let out = Config::with_mcp_server_removed(text, "drop").unwrap();
    assert!(out.contains("# my config"), "comment lost: {out}");
    assert!(out.contains("# keep note"), "inline comment lost: {out}");
    let cfg: Config = toml::from_str(&out).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(cfg.mcp_servers[0].name, "keep");
    assert!(!out.contains("drop-mcp"));
}

#[test]
fn with_mcp_server_removed_reports_a_non_array_section_accurately() {
    // The inline-array form is valid TOML the serde reader accepts; the
    // writer must say it cannot edit that shape, not falsely claim the
    // entry is absent.
    let text = "mcp_servers = [ { name = \"x\", command = \"y\" } ]\n";
    let err = Config::with_mcp_server_removed(text, "x").unwrap_err();
    assert!(
        err.to_string().contains("not an array of tables"),
        "wrong-shape section misreported: {err}"
    );
    let err = Config::with_mcp_server_removed("mcp_servers = 3\n", "x").unwrap_err();
    assert!(
        err.to_string().contains("not an array of tables"),
        "scalar section misreported: {err}"
    );
}

#[test]
fn mcp_writer_error_branches_are_loud() {
    let entry = crate::mcp::McpServerEntry {
        name: "x".into(),
        enabled: true,
        transport: crate::mcp::TransportKind::Stdio,
        command: Some("x-mcp".into()),
        args: vec![],
        env: std::collections::BTreeMap::new(),
        url: None,
        headers: std::collections::BTreeMap::new(),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        trust: crate::mcp::McpTrust::Trusted,
        origin: None,
    };
    // Invalid TOML input text.
    let err = Config::with_mcp_server_added("not toml [", &entry).unwrap_err();
    assert!(err.to_string().contains("invalid MCP TOML syntax"), "{err}");
    let err = Config::with_mcp_server_removed("not toml [", "x").unwrap_err();
    assert!(err.to_string().contains("invalid MCP TOML syntax"), "{err}");
    // A section that is not an array of tables.
    let err = Config::with_mcp_server_added("mcp_servers = 3\n", &entry).unwrap_err();
    assert!(err.to_string().contains("not an array of tables"), "{err}");
    // A timeout that does not fit TOML's i64 integers.
    let oversized = crate::mcp::McpServerEntry {
        login_argv: Vec::new(),
        request_timeout_secs: Some(u64::MAX),
        ..entry
    };
    let err = Config::with_mcp_server_added("", &oversized).unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

#[test]
fn with_mcp_server_removed_errors_when_absent() {
    let text = "[[mcp_servers]]\nname = \"present\"\ncommand = \"x\"\n";
    let err = Config::with_mcp_server_removed(text, "ghost").unwrap_err();
    assert!(err.to_string().contains("ghost"), "names the miss: {err}");
    // No section at all errors the same way, not a panic.
    assert!(Config::with_mcp_server_removed("", "ghost").is_err());
}
