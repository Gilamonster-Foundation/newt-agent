use super::*;

#[test]
fn add_then_list_shows_the_entry_with_its_source() {
    let sb = sandbox();
    newt(&sb)
        .args([
            "mcp",
            "add",
            "scrybe",
            "--command",
            "scrybe-mcp-server",
            "--arg",
            "stdio",
            "--env",
            "SCRYBE_LOG=info",
            "--timeout-secs",
            "120",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Registered MCP server 'scrybe'"))
        .stdout(predicate::str::contains("newt doctor"))
        .stdout(predicate::str::contains("/mcp"));

    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers.len(), 1);
    let entry = &cfg.mcp_servers[0];
    assert_eq!(entry.name, "scrybe");
    assert_eq!(entry.command.as_deref(), Some("scrybe-mcp-server"));
    assert_eq!(entry.args, vec!["stdio"]);
    assert_eq!(
        entry
            .env
            .get("SCRYBE_LOG")
            .and_then(newt_core::mcp::SecretValue::as_literal),
        Some("info")
    );
    assert_eq!(entry.request_timeout_secs, Some(120));

    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("scrybe"))
        .stdout(predicate::str::contains("stdio"))
        .stdout(predicate::str::contains("yes"))
        .stdout(predicate::str::contains("newt config"));
}

#[test]
fn duplicate_add_fails_nonzero_with_a_clear_error() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "add", "scrybe", "--command", "scrybe-mcp-server"])
        .assert()
        .success();
    newt(&sb)
        .args(["mcp", "add", "scrybe", "--command", "other"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("scrybe"))
        .stderr(predicate::str::contains("already exists"));

    // The losing write changed nothing.
    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(
        cfg.mcp_servers[0].command.as_deref(),
        Some("scrybe-mcp-server")
    );
}

#[test]
fn remove_deletes_the_entry_and_an_absent_name_errors() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "add", "scrybe", "--command", "scrybe-mcp-server"])
        .assert()
        .success();
    newt(&sb)
        .args(["mcp", "remove", "scrybe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed MCP server 'scrybe'"));
    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert!(cfg.mcp_servers.is_empty());

    newt(&sb)
        .args(["mcp", "remove", "scrybe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("scrybe"));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn remove_edits_the_file_that_owns_the_effective_server() {
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    let mcp = sb.config_dir.join("mcp.toml");
    std::fs::write(
        &config,
        "[[mcp_servers]]\nname = \"main\"\ncommand = \"main-mcp\"\n",
    )
    .unwrap();
    std::fs::write(
        &mcp,
        "[[mcp_servers]]\nname = \"broken-out\"\ncommand = \"mcp-file-server\"\n",
    )
    .unwrap();

    newt(&sb)
        .args(["mcp", "remove", "broken-out"])
        .assert()
        .success();

    assert!(std::fs::read_to_string(config)
        .unwrap()
        .contains("main-mcp"));
    assert!(newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(mcp).unwrap()).is_empty());
}

#[test]
fn list_reports_malformed_mcp_toml_without_echoing_its_contents() {
    let sb = sandbox();
    let marker = "private-mcp-marker";
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        format!("[[mcp_servers]]\nname = \"{marker}\"\ncommand = =\n"),
    )
    .unwrap();

    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid MCP TOML syntax"))
        .stderr(predicate::str::contains(marker).not());
}
