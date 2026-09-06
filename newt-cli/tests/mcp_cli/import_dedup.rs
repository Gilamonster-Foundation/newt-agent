use super::*;

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_dedup_errors_on_clash_unless_force_or_merge() {
    let sb = sandbox();
    let claude = sb.cwd.join("c.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": { "fs": { "command": "v2-cmd" } } }"#,
    )
    .unwrap();
    // Pre-seed an mcp.toml with a clashing `fs`.
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"fs\"\ncommand = \"v1-cmd\"\n",
    )
    .unwrap();

    // Default: a clash is a loud error, nothing overwritten.
    newt(&sb)
        .args(["mcp", "import", claude.to_str().unwrap(), "--all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
    let servers = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    assert_eq!(
        servers[0].command.as_deref(),
        Some("v1-cmd"),
        "unchanged on error"
    );

    // --merge: keep the existing entry, skip the import.
    newt(&sb)
        .args([
            "mcp",
            "import",
            claude.to_str().unwrap(),
            "--all",
            "--merge",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Skipped"));
    let servers = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    assert_eq!(
        servers[0].command.as_deref(),
        Some("v1-cmd"),
        "merge kept existing"
    );

    // --force: overwrite the existing entry.
    newt(&sb)
        .args([
            "mcp",
            "import",
            claude.to_str().unwrap(),
            "--all",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Overwrote"));
    let servers = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    assert_eq!(
        servers[0].command.as_deref(),
        Some("v2-cmd"),
        "force overwrote"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_refuses_a_name_already_in_config_toml_that_outranks_mcp_toml() {
    // FIX 3 (#1301): config.toml outranks ~/.newt/mcp.toml, so importing a name
    // already in config.toml would write a silently-shadowed entry. It must be a
    // loud error (not a misleading "Imported"), and even --force must not write.
    let sb = sandbox();
    std::fs::write(
        sb.config_dir.join("config.toml"),
        "[[mcp_servers]]\nname = \"scrybe\"\ncommand = \"config-scrybe\"\n",
    )
    .unwrap();
    let claude = sb.cwd.join("c.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": { "scrybe": { "command": "claude-scrybe" } } }"#,
    )
    .unwrap();

    for extra in [&[][..], &["--force"][..], &["--merge"][..]] {
        let mut args = vec!["mcp", "import", claude.to_str().unwrap(), "--all"];
        args.extend_from_slice(extra);
        newt(&sb)
            .args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("outranks"))
            .stderr(predicate::str::contains("scrybe"));
    }
    // Nothing was written to mcp.toml (the ineffective entry never lands).
    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "no shadowed entry should be written"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_targets_user_mcp_toml_even_with_an_ambient_project_newt_toml() {
    // FIX 4 (#1301): a plain `newt mcp import` from a dir that happens to hold a
    // `./newt.toml` must still write the user-global ~/.newt/mcp.toml — the
    // ambient file must NOT capture it.
    let sb = sandbox();
    std::fs::write(sb.cwd.join("newt.toml"), "# ambient project config\n").unwrap();
    let claude = sb.cwd.join("c.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": { "fs": { "command": "mcp-fs" } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args(["mcp", "import", claude.to_str().unwrap(), "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported 1 MCP server(s)"));

    // The user-global mcp.toml got it; the ambient ./newt.toml was untouched.
    assert!(sb.config_dir.join("mcp.toml").is_file());
    let ambient = std::fs::read_to_string(sb.cwd.join("newt.toml")).unwrap();
    assert!(
        !ambient.contains("mcp_servers"),
        "the ambient ./newt.toml must not receive the import: {ambient}"
    );
}
