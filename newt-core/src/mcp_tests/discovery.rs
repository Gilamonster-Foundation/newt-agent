use super::*;

#[test]
fn management_discovery_retains_sources_and_namespace_conflicts_without_changing_winners() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let workspace = dir.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let native = home.join("mcp.toml");
    std::fs::write(&native, "[[mcp_servers]]\nname = 'docs_server'\ncommand = 'native-docs'\n[[mcp_servers]]\nname = 'native'\ncommand = 'native-tool'\n").unwrap();
    std::fs::write(
        home.join(".claude.json"),
        r#"{"mcpServers":{"borrowed":{"command":"borrowed-tool"}}}"#,
    )
    .unwrap();
    std::fs::write(workspace.join(".mcp.json"), r#"{"mcpServers":{"borrowed":{"command":"project-tool"},"project":{"command":"project-only"}}}"#).unwrap();
    let configured = [stdio("docs-server", "configured-docs")];
    let report = discover_report_with_namespace_mode(
        &configured,
        Some(&native),
        Some(&home),
        &workspace,
        true,
    );
    assert_eq!(
        report
            .servers
            .iter()
            .map(|server| (server.entry.name.as_str(), server.source))
            .collect::<Vec<_>>(),
        vec![
            ("docs-server", McpSource::Configuration),
            ("native", McpSource::UserMcpFile),
            ("borrowed", McpSource::ClaudeUser),
            ("project", McpSource::ClaudeProject),
        ]
    );
    assert_eq!(report.conflicts.len(), 2);
    assert_eq!(report.conflicts[0].name, "docs_server");
    assert_eq!(report.conflicts[0].source, McpSource::UserMcpFile);
    assert_eq!(report.conflicts[0].winner, "docs-server");
    assert_eq!(report.conflicts[1].source, McpSource::ClaudeProject);
    assert_eq!(report.conflicts[1].winner, "borrowed");
    let legacy =
        discover_with_namespace_mode(&configured, Some(&native), Some(&home), &workspace, true);
    assert_eq!(
        legacy,
        report
            .servers
            .into_iter()
            .map(|server| server.entry)
            .collect::<Vec<_>>()
    );
}

#[test]
fn management_discovery_does_not_elevate_project_configuration() {
    let mut entry = stdio("project", "tool");
    entry.trust = McpTrust::Untrusted;
    let dir = tempfile::tempdir().unwrap();
    let report = discover_report_with_namespace_mode(&[entry], None, None, dir.path(), true);
    assert_eq!(report.servers[0].source, McpSource::Configuration);
    assert_eq!(report.servers[0].entry.trust, McpTrust::Untrusted);
    assert!(admit(&report.servers[0].entry).is_err());
}

#[test]
fn discovers_from_claude_user_and_project_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(
        home.join(".claude.json"),
        r#"{ "mcpServers": { "user_srv": { "command": "u" } } }"#,
    )
    .unwrap();
    std::fs::write(
        ws.join(".mcp.json"),
        r#"{ "mcpServers": { "proj_srv": { "command": "p" } } }"#,
    )
    .unwrap();

    let got = discover(&[], None, Some(&home), &ws);
    let names: std::collections::BTreeSet<_> = got.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains("user_srv"),
        "user config discovered: {names:?}"
    );
    assert!(
        names.contains("proj_srv"),
        "project config discovered: {names:?}"
    );
}
#[test]
fn discover_marks_newt_sources_trusted_and_claude_untrusted() {
    // Trust provenance is stamped by discover(): the in-memory newt source is
    // trusted; a Claude project overlay is untrusted.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join(".mcp.json"),
        r#"{ "mcpServers": { "proj": { "command": "p" } } }"#,
    )
    .unwrap();
    let got = discover(&[stdio("owned", "o")], None, None, ws);
    let owned = got.iter().find(|e| e.name == "owned").unwrap();
    let proj = got.iter().find(|e| e.name == "proj").unwrap();
    assert_eq!(owned.trust, McpTrust::Trusted);
    assert_eq!(proj.trust, McpTrust::Untrusted);
}
#[test]
fn discover_preserves_untrusted_project_origin_and_does_not_re_elevate() {
    // #1301 residual vector: `Config::resolve` stamps a walked-up project
    // `.newt/config.toml`'s entries UNTRUSTED before handing `cfg.mcp_servers`
    // to discover() as `newt_servers`. discover() must PRESERVE that mark —
    // never re-elevate it to Trusted — while still stamping a genuine
    // newt-owned entry Trusted.
    let mut project_origin = stdio("proj", "p");
    project_origin.trust = McpTrust::Untrusted;
    let owned = stdio("owned", "o"); // default Trusted

    let got = discover(
        &[project_origin, owned],
        None,
        None,
        Path::new("/nonexistent"),
    );
    assert_eq!(
        got.iter().find(|e| e.name == "proj").unwrap().trust,
        McpTrust::Untrusted,
        "a project-origin Untrusted mark must survive discover(), not be re-elevated"
    );
    assert_eq!(
        got.iter().find(|e| e.name == "owned").unwrap().trust,
        McpTrust::Trusted,
        "a genuine newt-owned entry is (still) trusted"
    );
}
#[test]
fn discover_reads_mcp_toml_as_a_newt_owned_source() {
    let dir = tempfile::tempdir().unwrap();
    let mcp_toml = dir.path().join("mcp.toml");
    std::fs::write(
        &mcp_toml,
        "[[mcp_servers]]\nname = \"broken-out\"\ncommand = \"bo-mcp\"\n",
    )
    .unwrap();
    let got = discover(&[], Some(&mcp_toml), None, Path::new("/nonexistent"));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].name, "broken-out");
    // A missing mcp.toml path is simply skipped (non-fatal).
    assert!(discover(
        &[],
        Some(Path::new("/no/such/mcp.toml")),
        None,
        Path::new("/nope")
    )
    .is_empty());
}
