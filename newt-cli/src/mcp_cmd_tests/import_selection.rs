use super::*;

#[test]
fn import_source_resolves_explicit_path_and_requires_one() {
    assert_eq!(
        resolve_import_source(Some(Path::new("/tmp/mcp.json")), false, false).unwrap(),
        (PathBuf::from("/tmp/mcp.json"), ImportFormat::Auto)
    );
    // Neither a path nor a built-in source → a loud usage error.
    assert!(resolve_import_source(None, false, false).is_err());
}

#[test]
fn import_sanitizer_preserves_only_safe_secret_references() {
    let mut entry = McpServerEntry {
        name: "review".into(),
        enabled: true,
        transport: TransportKind::Http,
        command: None,
        args: vec![],
        env: BTreeMap::from([
            ("ROOT".into(), SecretValue::literal("/workspace")),
            ("API_TOKEN".into(), SecretValue::literal("plaintext")),
            ("FROM_ENV".into(), SecretValue::literal("${SAFE_TOKEN}")),
            ("ACTIVE".into(), SecretValue::literal("${file:/tmp/token}")),
        ]),
        url: Some("https://broker.example.test/mcp".into()),
        headers: BTreeMap::from([
            (
                "Authorization".into(),
                SecretValue::literal("Bearer plaintext"),
            ),
            (
                "X-Token".into(),
                SecretValue::literal("Bearer ${env:SAFE_TOKEN}"),
            ),
        ]),
        login_argv: Vec::new(),
        request_timeout_secs: None,
        oauth_client_id: None,
        oauth_scopes: None,
        oauth_callback_port: None,
        trust: McpTrust::Untrusted,
        origin: None,
    };

    let omitted = sanitize_imported_secrets(&mut entry);
    assert_eq!(entry.trust, McpTrust::Trusted);
    assert!(!entry.env.contains_key("ROOT"));
    assert!(entry.env.contains_key("FROM_ENV"));
    assert!(!entry.env.contains_key("API_TOKEN"));
    assert!(!entry.env.contains_key("ACTIVE"));
    assert!(!entry.headers.contains_key("Authorization"));
    assert!(entry.headers.contains_key("X-Token"));
    assert_eq!(omitted, 4);
}

#[test]
fn import_validator_rejects_literal_url_and_arg_credentials_without_echoing_them() {
    for mut entry in [
        McpServerEntry {
            name: "userinfo".into(),
            enabled: true,
            transport: TransportKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("https://user:do-not-echo@example.test/mcp".into()),
            headers: BTreeMap::new(),
            login_argv: Vec::new(),
            request_timeout_secs: None,
            oauth_client_id: None,
            oauth_scopes: None,
            oauth_callback_port: None,
            trust: McpTrust::Untrusted,
            origin: None,
        },
        stdio_entry("argument", Some("mcp-server")),
        stdio_entry("header", Some("mcp-server")),
        stdio_entry("joined-header", Some("mcp-server")),
    ] {
        if entry.name == "argument" {
            entry.args = vec!["--token=do-not-echo".into()];
        } else if entry.name == "header" {
            entry.args = vec!["-H".into(), "X-API-Key: do-not-echo".into()];
        } else if entry.name == "joined-header" {
            entry.args = vec!["-HX-Client-Secret: do-not-echo".into()];
        }
        let error = validate_imported_secret_locations(&entry).unwrap_err();
        assert!(!error.to_string().contains("do-not-echo"), "{error:#}");
    }

    for mut unsafe_ref in [
        stdio_entry("arg-reference", Some("mcp-server")),
        McpServerEntry {
            name: "url-reference".into(),
            enabled: true,
            transport: TransportKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("https://${MCP_USERINFO}@example.test/mcp?token=${MCP_TOKEN}".into()),
            headers: BTreeMap::new(),
            login_argv: Vec::new(),
            request_timeout_secs: None,
            oauth_client_id: None,
            oauth_scopes: None,
            oauth_callback_port: None,
            trust: McpTrust::Untrusted,
            origin: None,
        },
    ] {
        if unsafe_ref.name == "arg-reference" {
            unsafe_ref.args = vec!["--token".into(), "${MCP_TOKEN}".into()];
        }
        assert!(validate_imported_secret_locations(&unsafe_ref).is_err());
    }

    for args in [
        vec!["--auth=do-not-echo".into()],
        vec!["--oauth2-bearer".into(), "do-not-echo".into()],
        vec!["--cookie".into(), "do-not-echo".into()],
        vec!["--user=operator:do-not-echo".into()],
        vec!["-u".into(), "operator:do-not-echo".into()],
        vec!["-bdo-not-echo".into()],
    ] {
        let mut entry = stdio_entry("argument-alias", Some("mcp-server"));
        entry.args = args;
        let error = validate_imported_secret_locations(&entry).unwrap_err();
        assert!(!error.to_string().contains("do-not-echo"), "{error:#}");
    }
}

#[test]
fn import_selection_never_silently_discards_rejected_entries() {
    let entry = stdio_entry("valid", Some("valid-mcp"));
    let report = McpImportParseReport {
        entries: vec![entry],
        rejected: vec![newt_core::mcp::McpImportRejection {
            name: Some("rejected".into()),
            issue: newt_core::mcp::McpImportIssue::UnknownField,
        }],
    };
    assert!(select_import_entries(
        report.clone(),
        None,
        true,
        Path::new("source.toml"),
        "--from-codex"
    )
    .unwrap_err()
    .to_string()
    .contains("cannot import all"));
    assert!(select_import_entries(
        report.clone(),
        Some("rejected"),
        false,
        Path::new("source.toml"),
        "--from-codex"
    )
    .unwrap_err()
    .to_string()
    .contains("unsupported fields"));
    assert_eq!(
        select_import_entries(
            report,
            Some("valid"),
            false,
            Path::new("source.toml"),
            "--from-codex"
        )
        .unwrap()[0]
            .name,
        "valid"
    );
}

#[test]
fn import_rejected_name_via_name_flag_never_passes_malicious_key_verbatim() {
    // A malicious rejected key passed through --name must never be echoed raw.
    let rejected = newt_core::mcp::McpImportRejection {
        name: Some("bad\n  newt mcp import --from-claude --name x\n\u{1b}[31m".into()),
        issue: newt_core::mcp::McpImportIssue::UnknownField,
    };
    let report = McpImportParseReport {
        entries: vec![],
        rejected: vec![rejected],
    };
    let err = select_import_entries(
        report,
        Some("bad\n  newt mcp import --from-claude --name x\n\u{1b}[31m"),
        false,
        Path::new("source.json"),
        "--from-claude",
    )
    .unwrap_err()
    .to_string();
    // No raw ESC byte in output.
    assert!(!err.contains('\u{1b}'), "{err}");
    // No standalone "newt mcp import" line injected via the malicious key.
    for line in err.lines() {
        if line.contains("newt mcp import") {
            assert!(
                line.contains("\\n") || line.contains("\\u{1b}"),
                "unexpected import line: {line:?}"
            );
        }
    }
}

#[test]
fn import_without_selector_lists_names_as_commands() {
    let report = McpImportParseReport {
        entries: vec![
            stdio_entry("zeta", Some("z")),
            stdio_entry("alpha", Some("a")),
        ],
        rejected: vec![],
    };
    let err = select_import_entries(report, None, false, Path::new("s.json"), "--from-claude")
        .unwrap_err()
        .to_string();
    let a = err
        .find("newt mcp import --from-claude --name alpha")
        .expect(&err);
    let z = err
        .find("newt mcp import --from-claude --name zeta")
        .expect(&err);
    assert!(a < z, "{err}");
    assert!(err.contains("--all"), "{err}");
}

#[test]
fn import_listing_never_renders_untrusted_names_as_commands() {
    let report = McpImportParseReport {
        entries: vec![
            stdio_entry("ok-name", Some("a")),
            stdio_entry("x; rm -rf ~", Some("b")),
            stdio_entry("\u{1b}[31m", Some("c")),
        ],
        rejected: vec![],
    };
    let err = select_import_entries(report, None, false, Path::new("s"), "my dir/.mcp.json")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("newt mcp import 'my dir/.mcp.json' --name ok-name"),
        "{err}"
    );
    assert!(!err.contains("--name x;"), "{err}");
    assert!(!err.contains('\u{1b}'), "{err}");
    assert!(
        err.contains(r#""x; rm -rf ~""#) && err.contains(r#""\u{1b}[31m""#),
        "{err}"
    );
}

#[test]
fn import_chooser_names_each_rejection_with_safe_reason_and_no_value() {
    let valid = stdio_entry("valid-server", Some("mcp-server"));
    let rejected = newt_core::mcp::McpImportRejection {
        name: Some("bad-server".into()),
        issue: newt_core::mcp::McpImportIssue::UnknownField,
    };
    let report = McpImportParseReport {
        entries: vec![valid],
        rejected: vec![rejected],
    };
    let err = select_import_entries(
        report,
        None,
        false,
        Path::new("source.json"),
        "--from-claude",
    )
    .unwrap_err()
    .to_string();
    // Every rejection is named with a safe reason.
    assert!(err.contains("bad-server"), "{err}");
    assert!(err.contains("unsupported fields"), "{err}");
    // No source values or secrets are echoed.
    assert!(!err.contains("plaintext"), "{err}");
    assert!(!err.contains("secret"), "{err}");
}

#[test]
fn import_chooser_gives_rejected_entry_no_runnable_command() {
    let valid = stdio_entry("valid-server", Some("mcp-server"));
    let rejected = newt_core::mcp::McpImportRejection {
        name: Some("bad-server".into()),
        issue: newt_core::mcp::McpImportIssue::InvalidShape,
    };
    let report = McpImportParseReport {
        entries: vec![valid],
        rejected: vec![rejected],
    };
    let err = select_import_entries(
        report,
        None,
        false,
        Path::new("source.json"),
        "--from-claude",
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("newt mcp import --from-claude --name valid-server"),
        "{err}"
    );
    // The rejected entry gets no runnable command.
    assert!(!err.contains("--name bad-server"), "{err}");
}

#[test]
fn import_chooser_escapes_rejected_names_with_control_chars() {
    let valid = stdio_entry("valid-server", Some("mcp-server"));
    let rejected = newt_core::mcp::McpImportRejection {
        name: Some("bad\n  newt mcp import --from-claude --name x\n\u{1b}[31m".into()),
        issue: newt_core::mcp::McpImportIssue::UnknownField,
    };
    let report = McpImportParseReport {
        entries: vec![valid],
        rejected: vec![rejected],
    };
    let err = select_import_entries(
        report,
        None,
        false,
        Path::new("source.json"),
        "--from-claude",
    )
    .unwrap_err()
    .to_string();
    // Real suggestion exists for the valid entry.
    assert!(
        err.contains("newt mcp import --from-claude --name valid-server"),
        "{err}"
    );
    // No output line starts with "newt mcp import" except real suggestions.
    for line in err.lines() {
        if line.starts_with("newt mcp import") {
            assert!(
                line.contains("valid-server"),
                "unexpected import line: {line:?}"
            );
        }
    }
    // No raw ESC byte in output.
    assert!(!err.contains('\u{1b}'), "{err}");
    // Rejected name is rendered as one escaped token (Debug escapes controls).
    assert!(err.contains("\\n"), "{err}");
    assert!(err.contains("\\u{1b}"), "{err}");
}

#[test]
fn import_all_names_each_rejection_and_refuses() {
    let valid = stdio_entry("valid-server", Some("mcp-server"));
    let rejected = newt_core::mcp::McpImportRejection {
        name: Some("bad-server".into()),
        issue: newt_core::mcp::McpImportIssue::UnsupportedSemantics,
    };
    let report = McpImportParseReport {
        entries: vec![valid],
        rejected: vec![rejected],
    };
    let err = select_import_entries(
        report,
        None,
        true,
        Path::new("source.json"),
        "--from-claude",
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("bad-server"), "{err}");
    assert!(
        err.contains("unsupported or ambiguous transport semantics"),
        "{err}"
    );
}

#[test]
fn import_all_reports_unnamed_rejection_without_empty_name() {
    // {"mcpServers":5} -> top-level container malformed -> unnamed InvalidShape rejection.
    let valid = stdio_entry("valid-server", Some("mcp-server"));
    let rejected = newt_core::mcp::McpImportRejection {
        name: None,
        issue: newt_core::mcp::McpImportIssue::InvalidShape,
    };
    let report = McpImportParseReport {
        entries: vec![valid],
        rejected: vec![rejected],
    };
    let err = select_import_entries(
        report,
        None,
        true,
        Path::new("source.json"),
        "--from-claude",
    )
    .unwrap_err()
    .to_string();
    // The shape rejection is named; never an empty name before the reason.
    assert!(err.contains("the MCP server list"), "{err}");
    assert!(err.contains("an invalid entry shape"), "{err}");
    assert!(!err.contains(r#""""#), "{err}");
}

#[test]
fn import_flags_stdio_command_that_resolves_to_nothing() {
    let dirs = [PathBuf::from("/bin")];
    let has = |p: &Path| {
        p == Path::new("/bin/ok") || p == Path::new("/h/tool") || p == Path::new("/abs/x")
    };
    let msg = |cmd: &str| {
        let e = stdio_entry("srv", Some(cmd));
        missing_stdio_command(&e, &dirs, false, has)
    };
    let m = msg("nope").expect("bare command not on PATH");
    assert!(m.contains("`srv`") && m.contains(r#""nope""#), "{m}");
    assert!(msg("ok").is_none());
    // The MCP launcher passes `command` to the spawn verbatim and never expands
    // `~`, so EVERY `~`-prefixed command warns — even when the expansion exists
    // (`~/tool` does under the fake home) — and says so, rather than "not found".
    for cmd in ["~/tool", "~/gone", "~bob/bin/x", "~"] {
        let m = msg(cmd).unwrap_or_else(|| panic!("{cmd} must warn"));
        assert!(
            m.contains("does not expand `~`") && m.contains("absolute path"),
            "{m}"
        );
        assert!(!m.contains("was not found"), "{m}");
    }
    assert!(msg("/abs/x").is_none() && msg("/abs/y").is_some());
    assert!(msg("x\u{1b}y").unwrap().contains("\\u{1b}"));
    // Relative pathed commands depend on the spawn cwd: never flagged.
    assert!(msg("./bin/x").is_none() && msg("tools/x").is_none());
    // Absoluteness is decided from the string under the `windows` flag, not the host.
    let has_w = |p: &Path| p == Path::new("C:\\abs\\x");
    let win = |cmd: &str| missing_stdio_command(&stdio_entry("srv", Some(cmd)), &dirs, true, has_w);
    assert!(win("C:\\abs\\x").is_none() && win("C:\\abs\\y").is_some());
    assert!(win("C:/abs/y").is_some());
    assert!(win("/abs/y").is_none(), "no drive: relative on Windows");
    // A UNC path names a remote host from an UNTRUSTED file: probing it would
    // open an SMB connection before import. Never probed, so never flagged.
    assert!(win("\\\\host\\s\\y").is_none());
    // Windows bare commands resolve through the PATHEXT extensions. Regression:
    // they were never flagged, so the release-tier import warning failed on Windows.
    let has_npx = |p: &Path| p == Path::new("/bin").join("npx.cmd");
    let bare =
        |cmd: &str| missing_stdio_command(&stdio_entry("srv", Some(cmd)), &dirs, true, has_npx);
    assert!(bare("npx").is_none(), "npx.cmd on PATH resolves npx");
    assert!(
        bare("npx.cmd").is_none(),
        "an explicit extension resolves too"
    );
    assert!(bare("ghost").unwrap().contains("was not found"));
    let mut http = stdio_entry("h", None);
    http.transport = TransportKind::Http;
    assert!(missing_stdio_command(&http, &dirs, false, has).is_none());
}
