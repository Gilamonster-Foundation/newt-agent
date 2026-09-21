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
        trust: McpTrust::Untrusted,
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
            trust: McpTrust::Untrusted,
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
            trust: McpTrust::Untrusted,
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
fn import_flags_stdio_command_that_resolves_to_nothing() {
    let dirs = [PathBuf::from("/bin")];
    let home = Path::new("/h");
    let has = |p: &Path| {
        p == Path::new("/bin/ok") || p == Path::new("/h/tool") || p == Path::new("/abs/x")
    };
    let msg = |cmd: &str| {
        let e = stdio_entry("srv", Some(cmd));
        missing_stdio_command(&e, &dirs, Some(home), has)
    };
    let m = msg("nope").expect("bare command not on PATH");
    assert!(m.contains("`srv`") && m.contains(r#""nope""#), "{m}");
    assert!(msg("ok").is_none());
    assert!(msg("~/tool").is_none() && msg("~/gone").is_some());
    assert!(msg("/abs/x").is_none() && msg("/abs/y").is_some());
    assert!(msg("x\u{1b}y").unwrap().contains("\\u{1b}"));
    let mut http = stdio_entry("h", None);
    http.transport = TransportKind::Http;
    assert!(missing_stdio_command(&http, &dirs, Some(home), has).is_none());
}
