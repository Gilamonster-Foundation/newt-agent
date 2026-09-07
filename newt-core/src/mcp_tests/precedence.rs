use super::*;

#[test]
fn invalid_entries_are_dropped() {
    // stdio with no command, and sse with no url — both invalid.
    let stdio_no_cmd = McpServerEntry {
        enabled: true,
        name: "a".into(),
        transport: TransportKind::Stdio,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        request_timeout_secs: None,
        trust: McpTrust::Trusted,
    };
    let sse_no_url = McpServerEntry {
        transport: TransportKind::Sse,
        ..stdio_no_cmd.clone()
    };
    assert!(!stdio_no_cmd.is_valid());
    assert!(!sse_no_url.is_valid());
    // discover drops them.
    let got = discover(
        &[stdio_no_cmd, sse_no_url],
        None,
        None,
        Path::new("/nonexistent"),
    );
    assert!(got.is_empty());
}
#[test]
fn newt_entry_wins_on_name_clash_and_dedups() {
    // Two newt entries with the same name -> first wins; a later source with
    // the same name is ignored.
    let newt = vec![
        McpServerEntry {
            enabled: true,
            name: "dup".into(),
            transport: TransportKind::Stdio,
            command: Some("newt-one".into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: McpTrust::Trusted,
        },
        McpServerEntry {
            enabled: true,
            name: "dup".into(),
            transport: TransportKind::Stdio,
            command: Some("newt-two".into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: McpTrust::Trusted,
        },
    ];
    let got = discover(&newt, None, None, Path::new("/nonexistent"));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].command.as_deref(), Some("newt-one"));
}
#[test]
fn discover_ranks_config_over_mcp_toml_over_claude() {
    // Pure precedence over in-memory sources: config.toml newt entry wins,
    // then ~/.newt/mcp.toml, then the Claude overlays. First-name-wins,
    // order preserved.
    let merged = dedup_valid_first_wins(
        vec![
            stdio("dup", "config-wins"),
            stdio("mcp-only", "m"),
            stdio("dup", "mcp-toml-loses"),
            stdio("claude-only", "c"),
            stdio("dup", "claude-loses"),
        ],
        false,
    );
    let names: Vec<&str> = merged.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["dup", "mcp-only", "claude-only"]);
    assert_eq!(merged[0].command.as_deref(), Some("config-wins"));
}
#[test]
fn runtime_namespace_dedup_is_mode_aware_and_precedence_ordered() {
    let sources = vec![
        stdio("review-source", "higher-precedence"),
        stdio("review_source", "lower-precedence"),
    ];

    let sanitized = dedup_valid_first_wins(sources.clone(), true);
    assert_eq!(sanitized.len(), 1);
    assert_eq!(sanitized[0].name, "review-source");
    assert_eq!(sanitized[0].command.as_deref(), Some("higher-precedence"));

    let raw = dedup_valid_first_wins(sources, false);
    assert_eq!(
        raw.iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        vec!["review-source", "review_source"]
    );
}
#[test]
fn runtime_namespace_drops_names_that_contain_the_wire_separator() {
    let sources = vec![
        stdio("plain", "plain-server"),
        stdio("raw__ambiguous", "raw-server"),
        stdio("sanitized--ambiguous", "sanitized-server"),
    ];

    let sanitized = dedup_valid_first_wins(sources.clone(), true);
    assert_eq!(
        sanitized
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["plain"]
    );

    let raw = dedup_valid_first_wins(sources, false);
    assert_eq!(
        raw.iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["plain", "sanitized--ambiguous"]
    );
}
