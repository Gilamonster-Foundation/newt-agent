use super::*;

#[test]
fn installable_server_names_the_entry_and_the_catalog_it_came_from() {
    // A drop-in entry that parses but can never connect must error at
    // install time, pointing at the file to fix — not surface as a bare
    // config-writer error with no provenance.
    let broken = &parse_catalog("[[servers]]\nname = \"half\"\n").unwrap()[0];
    let origin = CatalogOrigin::DropIn(PathBuf::from("/proj/.newt/mcp-catalog.toml"));
    let err = installable_server(broken, &origin).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("half"), "names the entry: {msg}");
    assert!(msg.contains("mcp-catalog.toml"), "names the file: {msg}");
    assert!(msg.contains("command"), "names the missing field: {msg}");
    // Bundled origin is named as such.
    let err = installable_server(broken, &CatalogOrigin::Bundled).unwrap_err();
    assert!(err.to_string().contains("bundled"), "{err}");
    // A valid entry passes through with its name filled in.
    let good = &parse_catalog("[[servers]]\nname = \"ok\"\ncommand = \"ok-mcp\"\n").unwrap()[0];
    let server = installable_server(good, &origin).unwrap();
    assert_eq!(server.name, "ok");
    assert_eq!(server.command.as_deref(), Some("ok-mcp"));
}

#[test]
fn merged_rows_dedup_by_precedence_and_flag_invalid() {
    let newt = vec![
        stdio_entry("dup", Some("newt-wins")),
        stdio_entry("broken", None), // invalid: stdio, no command
    ];
    let claude_user = vec![stdio_entry("dup", Some("shadowed")), {
        let mut e = stdio_entry("user-only", Some("u"));
        e.enabled = false;
        e
    }];
    let mcp_toml = vec![
        stdio_entry("brokenout", Some("bo")),
        stdio_entry("dup", Some("shadowed-by-config")),
    ];
    let claude_project = vec![stdio_entry("proj-only", Some("p"))];
    let rows = merged_rows(&newt, &mcp_toml, &claude_user, &claude_project);
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["dup", "broken", "brokenout", "user-only", "proj-only"]
    );
    assert_eq!(rows[0].source, McpSource::NewtConfig, "config wins the dup");
    assert!(!rows[1].valid, "invalid entries are shown, flagged");
    assert_eq!(
        rows[2].source,
        McpSource::NewtMcpToml,
        "the broken-out mcp.toml source is attributed"
    );
    assert_eq!(rows[3].source, McpSource::ClaudeUser);
    assert!(!rows[3].enabled);
    assert_eq!(rows[4].source, McpSource::ClaudeProject);
}

#[test]
fn merged_rows_never_let_an_invalid_entry_shadow_the_real_winner() {
    // discover() only lets VALID entries claim a name: with an invalid
    // newt "x" and a valid claude-code "x", the session connects the
    // claude one. The view must show BOTH — the invalid row flagged, and
    // the valid row that actually wins — never hide the winner.
    let newt = vec![stdio_entry("x", None)]; // invalid: stdio, no command
    let claude_user = vec![stdio_entry("x", Some("claude-wins"))];
    let rows = merged_rows(&newt, &[], &claude_user, &[]);
    assert_eq!(
        rows.len(),
        2,
        "both the flagged row and the winner: {rows:?}"
    );
    assert_eq!(rows[0].source, McpSource::NewtConfig);
    assert!(!rows[0].valid);
    assert_eq!(rows[1].source, McpSource::ClaudeUser);
    assert!(rows[1].valid, "the connecting entry must be visible");
    // A valid claimant still shadows a later VALID duplicate.
    let rows = merged_rows(
        &[stdio_entry("y", Some("newt-wins"))],
        &[],
        &[stdio_entry("y", Some("shadowed"))],
        &[],
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, McpSource::NewtConfig);
}

#[test]
fn render_rows_lists_name_transport_enabled_and_source() {
    let rows = merged_rows(
        &[stdio_entry("scrybe", Some("scrybe-mcp-server"))],
        &[stdio_entry("brokenout", Some("bo-mcp"))],
        &[stdio_entry("broken", None)],
        &[],
    );
    let mut out = Vec::new();
    render_rows(&rows, &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("scrybe"), "{text}");
    assert!(text.contains("stdio"), "{text}");
    assert!(text.contains("yes"), "{text}");
    assert!(text.contains("newt config"), "{text}");
    assert!(text.contains("newt mcp.toml"), "{text}");
    assert!(text.contains("claude-code (user)"), "{text}");
    assert!(
        text.contains("invalid"),
        "an unconnectable entry is flagged: {text}"
    );
}

/// **The byte golden for `newt mcp list` as it ships today** (#1916).
/// Captured from the shipping renderer — see `models_cmd::d3c`.
#[test]
fn the_mcp_listing_is_byte_exact() {
    let rows = merged_rows(
        &[stdio_entry("scrybe", Some("scrybe-mcp-server"))],
        &[stdio_entry("brokenout", Some("bo-mcp"))],
        &[stdio_entry("broken", None)],
        &[],
    );
    let mut out = Vec::new();
    render_rows(&rows, &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        concat!(
            "| NAME      | TRANSPORT | ENABLED | SOURCE                                                                 |\n",
            "| --------- | --------- | ------- | ---------------------------------------------------------------------- |\n",
            "| scrybe    | stdio     | yes     | newt config                                                            |\n",
            "| brokenout | stdio     | yes     | newt mcp.toml                                                          |\n",
            "| broken    | stdio     | yes     | claude-code (user)  (invalid \u{2014} dropped at discovery; fix or remove it) |\n",
        )
    );
}

#[test]
fn render_rows_empty_view_points_at_add_and_install() {
    let mut out = Vec::new();
    render_rows(&[], &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("newt mcp add"), "{text}");
    assert!(text.contains("newt mcp install"), "{text}");
}

// ---- scrybe smart-install: binary resolution order (injected paths) ----
