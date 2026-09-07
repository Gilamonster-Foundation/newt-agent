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
    assert!(
        select_import_entries(report.clone(), None, true, Path::new("source.toml"))
            .unwrap_err()
            .to_string()
            .contains("cannot import all")
    );
    assert!(select_import_entries(
        report.clone(),
        Some("rejected"),
        false,
        Path::new("source.toml")
    )
    .unwrap_err()
    .to_string()
    .contains("unsupported fields"));
    assert_eq!(
        select_import_entries(report, Some("valid"), false, Path::new("source.toml")).unwrap()[0]
            .name,
        "valid"
    );
}
