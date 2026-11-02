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

/// A config holding `[[mcp_servers]]` whose `headers` line, here with a
/// credential, does not decode: unterminated (`syntax`), or the wrong type
/// (`shape`), each on line 5.
fn malformed_config(marker: &str) -> (String, String) {
    let head = "[[mcp_servers]]\nname = \"docs\"\ntype = \"http\"\nurl = \"https://docs.example.test/mcp\"\n";
    (
        format!("{head}headers = {{ Authorization = \"Bearer {marker}\" \n"),
        format!("{head}headers = \"Bearer {marker}\"\n"),
    )
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn list_reports_a_malformed_config_by_file_and_position_not_its_contents() {
    // #3062 review (R3-9): a config that did not decode failed `newt mcp
    // list` with the parser's own message, which quotes the offending line,
    // here a credential, and names no file. Every file the resolver reads
    // (the user config, `--config`, a walked-up project config, an ambient
    // ./newt.toml) is named with where it stops, never its contents.
    let sb = sandbox();
    let marker = "SYNTHETIC_SECRET_MARKER";
    let (syntax, shape) = malformed_config(marker);
    let user = sb.config_dir.join("config.toml");
    let pinned = sb.cwd.join("pinned.toml");
    let project = sb.cwd.join("proj").join(".newt").join("config.toml");
    let nested = sb.cwd.join("proj").join("sub");
    let ambient = sb.cwd.join("newt.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    let list = |cwd: &Path, args: &[&str], shown: &str, error: &str| {
        newt(&sb)
            .current_dir(cwd)
            .args(args)
            .args(["mcp", "list"])
            .assert()
            .failure()
            .stderr(predicate::str::contains(format!("{shown}: {error}")))
            .stdout(predicate::str::contains(marker).not())
            .stderr(predicate::str::contains(marker).not());
    };
    let shown = |path: &Path| path.display().to_string();
    let syntax_at = "TOML parse error at line 5, column 61";

    std::fs::write(&user, &syntax).unwrap();
    list(&sb.cwd, &[], &shown(&user), syntax_at);
    std::fs::write(&user, &shape).unwrap();
    list(
        &sb.cwd,
        &[],
        &shown(&user),
        "TOML parse error at line 5, column 11",
    );
    std::fs::remove_file(&user).unwrap();

    std::fs::write(&pinned, &syntax).unwrap();
    let pinned_arg = pinned.to_str().unwrap();
    list(
        &sb.cwd,
        &["--config", pinned_arg],
        &shown(&pinned),
        syntax_at,
    );
    std::fs::remove_file(&pinned).unwrap();

    // The walk-up and the ambient base are named as the resolver found them.
    let project_shown = shown(&Path::new("proj").join(".newt").join("config.toml"));
    for (text, error) in [(&syntax, syntax_at), (&shape, "TOML parse error")] {
        std::fs::write(&project, text).unwrap();
        list(&nested, &[], &project_shown, error);
    }
    std::fs::remove_file(&project).unwrap();
    for (text, error) in [(&syntax, syntax_at), (&shape, "TOML parse error")] {
        std::fs::write(&ambient, text).unwrap();
        list(&sb.cwd, &[], "newt.toml", error);
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn doctor_reports_a_malformed_config_by_file_and_position_not_its_contents() {
    // #3062 review (R3-9): `newt doctor` reports a config that did not
    // decode as a finding, and printed the parser's message, the credential
    // on the offending line included. The finding names the file and where
    // it stops, by default and with `--config`.
    let sb = sandbox();
    let marker = "SYNTHETIC_SECRET_MARKER";
    let config = sb.config_dir.join("config.toml");
    std::fs::write(&config, malformed_config(marker).0).unwrap();
    let config_arg = config.to_str().unwrap();
    for args in [&["doctor"][..], &["--config", config_arg, "doctor"][..]] {
        newt(&sb)
            .env("OLLAMA_HOST", "http://127.0.0.1:1")
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains(format!(
                "FINDING: config did not resolve — config error: {}: TOML parse error at line 5, column 61",
                config.display()
            )))
            .stdout(predicate::str::contains(marker).not())
            .stderr(predicate::str::contains(marker).not());
    }
}
