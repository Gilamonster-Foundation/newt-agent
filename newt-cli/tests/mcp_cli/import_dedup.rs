use super::*;

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_dedup_errors_on_clash_unless_force_or_merge() {
    let sb = sandbox();
    let claude = sb.cwd.join("c.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": {
              "fs": { "type": "http", "url": "https://new.example/mcp" },
              "shell": { "command": "shell-mcp" }
        } }"#,
    )
    .unwrap();
    // Pre-seed an mcp.toml with a clashing `fs`.
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "# local settings\n[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://old.example/mcp\"\nenabled = false\nlogin_argv = [\"client\", \"login\"]\nrequest_timeout_secs = 90\n",
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
    let fs = servers.iter().find(|entry| entry.name == "fs").unwrap();
    assert_eq!(fs.url.as_deref(), Some("https://old.example/mcp"));
    assert!(!fs.enabled, "unchanged on error");
    assert_eq!(fs.login_argv, vec!["client", "login"]);
    assert_eq!(fs.request_timeout_secs, Some(90));

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
        .stdout(predicate::str::contains("Skipped"))
        .stdout(predicate::str::contains(
            "Results: 1 added, 0 replaced, 0 unchanged, 1 skipped.",
        ));
    let servers = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    assert_eq!(servers[0].command.as_deref(), None, "merge kept existing");

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
        .stdout(predicate::str::contains("Overwrote 1"))
        .stdout(predicate::str::contains(
            "Results: 0 added, 1 replaced, 1 unchanged, 0 skipped.",
        ));
    let servers = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    let fs = servers.iter().find(|entry| entry.name == "fs").unwrap();
    assert_eq!(fs.url.as_deref(), Some("https://new.example/mcp"));
    assert!(!fs.enabled);
    assert_eq!(fs.login_argv, vec!["client", "login"]);
    assert_eq!(fs.request_timeout_secs, Some(90));
    assert!(std::fs::read_to_string(sb.config_dir.join("mcp.toml"))
        .unwrap()
        .contains("# local settings"));

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
        .stdout(predicate::str::contains(
            "Results: 0 added, 0 replaced, 2 unchanged, 0 skipped.",
        ));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_force_updates_the_effective_config_owner() {
    // config.toml outranks ~/.newt/mcp.toml. --force must update that owner,
    // while non-mutating modes continue to refuse a shadowed default target.
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

    for extra in [&[][..], &["--merge"][..]] {
        let mut args = vec!["mcp", "import", claude.to_str().unwrap(), "--all"];
        args.extend_from_slice(extra);
        newt(&sb)
            .args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("outranks"))
            .stderr(predicate::str::contains("scrybe"));
    }
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
        .stdout(predicate::str::contains("Results: 0 added, 1 replaced"));

    let updated = std::fs::read_to_string(sb.config_dir.join("config.toml")).unwrap();
    assert!(updated.contains("command = \"claude-scrybe\""));
    // No shadowed entry is written to the lower-precedence breakout file.
    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "force should replace the existing owner, not create a duplicate"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_refuses_a_batch_with_multiple_owners_without_writing() {
    let sb = sandbox();
    let config_path = sb.config_dir.join("config.toml");
    let mcp_path = sb.config_dir.join("mcp.toml");
    let config_original = "[[mcp_servers]]\nname = \"config-owned\"\ncommand = \"old-config\"\n";
    let mcp_original = "[[mcp_servers]]\nname = \"mcp-owned\"\ncommand = \"old-mcp\"\n";
    std::fs::write(&config_path, config_original).unwrap();
    std::fs::write(&mcp_path, mcp_original).unwrap();
    let source = sb.cwd.join("c.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "config-owned": { "command": "new-config" },
              "mcp-owned": { "command": "new-mcp" }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--all",
            "--force",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("different owning files"));

    assert_eq!(
        std::fs::read_to_string(config_path).unwrap(),
        config_original
    );
    assert_eq!(std::fs::read_to_string(mcp_path).unwrap(), mcp_original);
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
