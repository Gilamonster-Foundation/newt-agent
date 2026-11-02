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
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn remove_edits_a_project_owner_only_when_named() {
    // #3062 review (G2): the walked-up project config is repo-tracked. A
    // default-scope remove never wrote it on main; it refuses and names the
    // explicit command, and --project still removes the entry there.
    let sb = sandbox();
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    let project = sb.cwd.join(".newt/config.toml");
    let project_text =
        "# shared team config\n[[mcp_servers]]\nname = \"proj\"\ncommand = \"/bin/true\"\n";
    std::fs::write(&project, project_text).unwrap();

    newt(&sb)
        .args(["mcp", "remove", "proj"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("project config"))
        .stderr(predicate::str::contains("mcp remove proj`"));
    assert_eq!(std::fs::read_to_string(&project).unwrap(), project_text);

    newt(&sb)
        .args(["mcp", "remove", "--project", "proj"])
        .assert()
        .success();
    assert!(
        newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(&project).unwrap()).is_empty()
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn remove_skips_a_same_name_entry_discovery_drops() {
    // #3062 review (F11): config.toml's `docs` has no url, so discovery drops
    // it and the mcp.toml `docs` is the live server; remove acts on that one.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    let mcp = sb.config_dir.join("mcp.toml");
    let dead = "[[mcp_servers]]\nname = \"docs\"\ntype = \"http\"\n";
    std::fs::write(&config, dead).unwrap();
    std::fs::write(
        &mcp,
        "[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\n",
    )
    .unwrap();

    newt(&sb).args(["mcp", "remove", "docs"]).assert().success();

    assert_eq!(std::fs::read_to_string(&config).unwrap(), dead);
    assert!(newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(mcp).unwrap()).is_empty());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn mcp_config_errors_name_the_file_and_position_not_the_contents() {
    // #3062 review (F13): `newt mcp add` writes to an ambient ./newt.toml; when
    // that file does not parse, the error names it and where it stops, never
    // the line, which here holds a credential.
    let sb = sandbox();
    let ambient = sb.cwd.join("newt.toml");
    std::fs::write(
        &ambient,
        "[[mcp_servers]]\ncommand = \"SYNTHETIC_SECRET_MARKER\n",
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "add", "foo", "--command", "foo-mcp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("newt.toml"))
        .stderr(predicate::str::contains(
            "invalid MCP TOML syntax at line 2, column 35",
        ))
        .stderr(predicate::str::contains("SYNTHETIC_SECRET_MARKER").not());

    // Valid TOML that does not fit the MCP schema is not a syntax error.
    std::fs::remove_file(&ambient).unwrap();
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"docs\"\ncommand = \"docs-mcp\"\nheaders = \"SYNTHETIC_SECRET_MARKER\"\n",
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "invalid MCP config at line 4, column 11",
        ))
        .stderr(predicate::str::contains("SYNTHETIC_SECRET_MARKER").not());
}

#[test]
fn list_reports_malformed_mcp_toml_without_echoing_its_contents() {
    let sb = sandbox();
    // The marker is on the line the parser stops at, the line a raw TOML
    // error would quote (#3062 review).
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"docs\"\nheaders = { Authorization = \"SYNTHETIC_SECRET_MARKER\" \n",
    )
    .unwrap();

    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid MCP TOML syntax"))
        .stderr(predicate::str::contains("SYNTHETIC_SECRET_MARKER").not());
}
