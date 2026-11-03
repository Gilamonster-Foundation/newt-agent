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
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn a_duplicate_add_names_the_remove_for_the_file_it_writes() {
    // #3062 review (R2-8): `newt mcp add` writes mcp.toml when it exists, but
    // the default `newt mcp remove` edits the owner. With the live `alpha` in
    // config.toml and an older copy in mcp.toml, "remove it first" led the
    // operator to delete the live entry. The refusal names the command that
    // removes the copy in the file the add writes, and the copy newt uses.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    let mcp = sb.config_dir.join("mcp.toml");
    let live = "[[mcp_servers]]\nname = \"alpha\"\ncommand = \"/bin/true\"\nrequest_timeout_secs = 90 # live, tuned\n";
    let stale = "[[mcp_servers]]\nname = \"alpha\"\ncommand = \"/bin/false\"\n";
    std::fs::write(&config, live).unwrap();
    std::fs::write(&mcp, stale).unwrap();
    let remove = |path: &Path| {
        format!(
            "`newt --config {} mcp remove alpha`",
            newt_core::mcp::shell_quote_arg(&path.to_string_lossy())
        )
    };
    let add = || {
        newt(&sb)
            .args(["mcp", "add", "alpha", "--command", "/bin/echo"])
            .assert()
            .failure()
    };

    add()
        .stderr(predicate::str::contains(remove(&mcp)))
        .stderr(predicate::str::contains(format!(
            "shadowed by the one newt uses in {}",
            config.display()
        )));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), live);
    assert_eq!(std::fs::read_to_string(&mcp).unwrap(), stale);

    // Without the copy, the add writes config.toml, whose entry newt uses.
    std::fs::remove_file(&mcp).unwrap();
    add()
        .stderr(predicate::str::contains(remove(&config)))
        .stderr(predicate::str::contains("shadowed").not());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), live);
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn a_duplicate_add_offers_the_toggle_only_where_it_may_edit_the_file() {
    // #3062 review (R3-5): `/mcp enable|disable` refuses a project or an
    // ambient ./newt.toml it was not named for, so a duplicate add there
    // gives the toggle's own hand edit instead of sending the operator to it.
    let sb = sandbox();
    let disabled = |name: &str| {
        format!("[[mcp_servers]]\nname = \"{name}\"\ncommand = \"/bin/echo\"\nenabled = false\n")
    };
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    for (file, flags, kind) in [
        (
            sb.cwd.join(".newt/config.toml"),
            &["--project"][..],
            "project",
        ),
        (sb.cwd.join("newt.toml"), &[][..], "ambient"),
    ] {
        std::fs::write(&file, disabled("borrowed")).unwrap();
        newt(&sb)
            .args(["mcp", "add", "borrowed", "--command", "/bin/echo"])
            .args(flags)
            .assert()
            .failure()
            .stderr(predicate::str::contains("/mcp enable|disable").not())
            .stderr(predicate::str::contains(format!(
                "defined in the {kind} config"
            )))
            .stderr(predicate::str::contains(
                "set `enabled = true` there by hand",
            ));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            disabled("borrowed")
        );
        std::fs::remove_file(&file).unwrap();
    }

    // The operator's own config.toml is one the toggle edits.
    std::fs::write(sb.config_dir.join("config.toml"), disabled("mine")).unwrap();
    newt(&sb)
        .args(["mcp", "add", "mine", "--command", "/bin/echo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "or toggle it with `/mcp enable|disable`",
        ));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn a_duplicate_add_offers_to_enable_only_a_disabled_server() {
    // #3062 review (R4-7): the duplicate-add advice appended whatever the
    // toggle check returned. An enabled project server got "set `enabled =
    // false` there by hand", an order to disable the server being added, and
    // an inline-array config.toml got the writer's own error. Only a disabled
    // server is offered enabling, as an alternative to the remove, and only
    // the toggle's ownership refusal counts as advice.
    let sb = sandbox();
    let add = |name: &str, flags: &[&str]| {
        let output = newt(&sb)
            .args(["mcp", "add", name, "--command", "/bin/echo"])
            .args(flags)
            .assert()
            .failure()
            .get_output()
            .clone();
        String::from_utf8(output.stderr).unwrap()
    };
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    let project = sb.cwd.join(".newt/config.toml");
    let proj = |enabled: bool| {
        format!("[[mcp_servers]]\nname = \"proj\"\ncommand = \"/bin/echo\"\nenabled = {enabled}\n")
    };
    std::fs::write(&project, proj(true)).unwrap();
    let refusal = add("proj", &["--project"]);
    assert!(!refusal.contains("enabled = "), "{refusal}");
    assert!(!refusal.contains("/mcp enable"), "{refusal}");
    std::fs::write(&project, proj(false)).unwrap();
    let refusal = add("proj", &["--project"]);
    assert!(
        refusal.contains(
            "mcp remove proj`, or enable it (MCP server `proj` is defined in the project config"
        ),
        "{refusal}"
    );

    std::fs::write(
        sb.config_dir.join("config.toml"),
        "mcp_servers = [ { name = \"alpha\", command = \"/bin/echo\" } ]\n",
    )
    .unwrap();
    let refusal = add("alpha", &[]);
    assert!(refusal.contains("already exists"), "{refusal}");
    assert!(!refusal.contains("config error"), "{refusal}");
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
