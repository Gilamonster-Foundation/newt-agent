use super::*;

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn merge_does_not_grant_network_authority_for_a_skipped_server() {
    let sb = sandbox();
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://trusted.example.test/mcp\"\n",
    )
    .unwrap();
    let source = sb.cwd.join("claude.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "review": { "type": "http", "url": "https://skipped.example.test/mcp" }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--merge",
            "--grant-net",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Skipped 1"))
        .stdout(predicate::str::contains("Granted exact MCP network host").not());

    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "a skipped import must not create the authoritative config"
    );
    let servers = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    assert_eq!(
        servers[0].url.as_deref(),
        Some("https://trusted.example.test/mcp")
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_grants_and_reports_the_entry_it_saved() {
    // #3062 review (F2, G4): a force import keeps a local `enabled = false`
    // the source omits, so the saved server stays disabled. Its host must not
    // be granted, and the next steps must not call it admitted.
    let sb = sandbox();
    let source = sb.cwd.join("c.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "remote": { "type": "http", "url": "https://mcp.example.test/mcp" },
              "docs": { "command": "/bin/true" }
        } }"#,
    )
    .unwrap();
    let config = sb.config_dir.join("config.toml");
    let config_text = "[[mcp_servers]]\nname = \"remote\"\ntype = \"http\"\nurl = \"https://mcp.example.test/mcp\"\nenabled = false # keep off until review\n";
    std::fs::write(&config, config_text).unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "remote",
            "--force",
            "--grant-net",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("remote — not admitted"))
        .stdout(predicate::str::contains("Granted").not());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), config_text);

    let mcp = sb.config_dir.join("mcp.toml");
    std::fs::write(
        &mcp,
        "[[mcp_servers]]\nname = \"docs\"\ncommand = \"/bin/true\"\nenabled = false # paused by operator\n",
    )
    .unwrap();
    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "docs",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("docs — not admitted"))
        .stdout(predicate::str::contains("docs — admitted").not());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn a_grant_withheld_from_a_disabled_server_is_named_with_its_host() {
    // #3062 review (R3-8): --grant-net grants no host to a server the merge
    // keeps disabled (F2), and said nothing of it, so the grant the operator
    // asked for went missing without a word. Each withheld host is named,
    // with how to grant it once the server is enabled.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    std::fs::write(
        &config,
        "[[mcp_servers]]\nname = \"remote\"\ntype = \"http\"\nurl = \"https://mcp.example.test/mcp\"\nenabled = false # keep off until review\n",
    )
    .unwrap();
    let source = sb.cwd.join("c.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "remote": { "type": "http", "url": "https://mcp.example.test/mcp" },
              "open": { "type": "http", "url": "https://open.example.test/mcp" }
        } }"#,
    )
    .unwrap();
    let import = |extra: &[&str]| {
        newt(&sb)
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--all",
                "--force",
            ])
            .args(extra)
            .assert()
            .success()
    };

    import(&["--grant-net"])
        .stdout(predicate::str::contains("host(s) in").and(predicate::str::contains(
            ": open.example.test\n",
        )))
        .stdout(predicate::str::contains(
            "Withheld the network grant for `remote`, which stays disabled: once it is enabled, grant the exact hostname mcp.example.test in trusted [tui.permissions] net",
        ))
        .stdout(predicate::str::contains("for `open`").not());
    // Nothing was asked for without --grant-net, so nothing is withheld.
    import(&[]).stdout(predicate::str::contains("Withheld").not());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn a_host_already_granted_is_not_named_as_withheld() {
    // #3062 review (R4-1): the withheld line named the host of a disabled
    // server even when the same import granted it to an enabled one, or the
    // target's net already held it, so it asked for a grant that is there.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    let remote = "[[mcp_servers]]\nname = \"remote\"\ntype = \"http\"\nurl = \"https://shared.example.test/a/mcp\"\nenabled = false # keep off until review\n";
    let source = sb.cwd.join("c.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "remote": { "type": "http", "url": "https://shared.example.test/a/mcp" },
              "open": { "type": "http", "url": "https://shared.example.test/b/mcp" }
        } }"#,
    )
    .unwrap();
    let import = |selection: &[&str]| {
        newt(&sb)
            .args(["mcp", "import", source.to_str().unwrap()])
            .args(selection)
            .args(["--force", "--grant-net"])
            .assert()
            .success()
    };

    // Granted in this run, to `open` on the same host.
    std::fs::write(&config, remote).unwrap();
    import(&["--all"])
        .stdout(predicate::str::contains(": shared.example.test\n"))
        .stdout(predicate::str::contains("Withheld").not());
    // Already in the target's net.
    let granted = format!("{remote}\n[tui.permissions]\nnet = [\"shared.example.test\"]\n");
    std::fs::write(&config, &granted).unwrap();
    import(&["--name", "remote"]).stdout(predicate::str::contains("Withheld").not());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), granted);
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn grant_net_merge_under_a_project_config_recognizes_the_user_owned_server() {
    let sb = sandbox();
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    std::fs::write(
        sb.cwd.join(".newt/config.toml"),
        "default_tier_order = [\"FAST\"]\n",
    )
    .unwrap();
    let config = sb.config_dir.join("config.toml");
    let config_text = "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://trusted.example.test/mcp\"\n";
    std::fs::write(&config, config_text).unwrap();
    let source = sb.cwd.join("source.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "review": { "type": "http", "url": "https://skipped.example.test/mcp" }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--merge",
            "--grant-net",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Skipped 1"));

    assert_eq!(std::fs::read_to_string(config).unwrap(), config_text);
    assert!(!sb.config_dir.join("mcp.toml").exists());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn grant_net_refuses_a_project_array_that_would_replace_the_connector() {
    for project_array in [
        "mcp_servers = []\n",
        "[[mcp_servers]]\nname = \"different\"\ncommand = \"project-mcp\"\n",
    ] {
        let sb = sandbox();
        std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
        std::fs::write(sb.cwd.join(".newt/config.toml"), project_array).unwrap();
        let source = sb.cwd.join("source.json");
        std::fs::write(
            &source,
            r#"{ "mcpServers": {
                  "review": { "type": "http", "url": "https://review.example.test/mcp" }
            } }"#,
        )
        .unwrap();

        newt(&sb)
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
                "--grant-net",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "replaces the base mcp_servers array",
            ));

        assert!(!sb.config_dir.join("config.toml").exists());
        assert!(!sb.config_dir.join("mcp.toml").exists());
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn grant_net_allows_project_append_and_keeps_the_connector_effective() {
    let sb = sandbox();
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    std::fs::write(
        sb.cwd.join(".newt/config.toml"),
        "[merge]\narrays = \"append\"\n\n[[mcp_servers]]\nname = \"different\"\ncommand = \"project-mcp\"\n",
    )
    .unwrap();
    let source = sb.cwd.join("source.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "review": { "type": "http", "url": "https://review.example.test/mcp" }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .success();

    let persisted = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(persisted.mcp_servers[0].name, "review");
    assert_eq!(
        persisted.tui.unwrap().permissions.net,
        ["review.example.test"]
    );
    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("review"))
        .stdout(predicate::str::contains("different"));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn appended_project_same_name_obeys_default_merge_and_force_semantics() {
    fn fixture() -> (Sandbox, std::path::PathBuf) {
        let sb = sandbox();
        std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
        std::fs::write(
            sb.cwd.join(".newt/config.toml"),
            "[merge]\narrays = \"append\"\n\n[[mcp_servers]]\nname = \"review\"\ncommand = \"project-mcp\"\n",
        )
        .unwrap();
        let source = sb.cwd.join("source.json");
        std::fs::write(
            &source,
            r#"{ "mcpServers": {
                  "review": { "type": "http", "url": "https://review.example.test/mcp" }
            } }"#,
        )
        .unwrap();
        (sb, source)
    }

    let (sb, source) = fixture();
    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
    assert!(!sb.config_dir.join("config.toml").exists());

    let (sb, source) = fixture();
    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
            "--merge",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Skipped 1"))
        .stdout(predicate::str::contains("Granted exact MCP network host").not());
    assert!(!sb.config_dir.join("config.toml").exists());

    let (sb, source) = fixture();
    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Replaced 1 lower-precedence server(s)",
        ))
        .stdout(predicate::str::contains(
            "Results: 0 added, 1 replaced, 0 unchanged, 0 skipped.",
        ));
    let config = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(config.mcp_servers[0].name, "review");
    assert_eq!(config.tui.unwrap().permissions.net, ["review.example.test"]);
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn grant_net_uses_one_authoritative_config_without_rewriting_other_breakout_entries() {
    let sb = sandbox();
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"existing\"\ncommand = \"existing-mcp\"\n",
    )
    .unwrap();
    let source = sb.cwd.join("source.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "review": { "type": "http", "url": "https://review.example.test/mcp" }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .success();

    let config = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(
        config
            .mcp_servers
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>(),
        vec!["review"]
    );
    assert_eq!(
        config.tui.unwrap().permissions.net,
        vec!["review.example.test"]
    );
    let breakout = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    assert_eq!(breakout[0].name, "existing");
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn grant_net_keeps_an_explicit_mcp_toml_as_the_authoritative_target() {
    let sb = sandbox();
    let explicit = sb.config_dir.join("mcp.toml");
    let ordinary = sb.config_dir.join("config.toml");
    let ordinary_text = "[[mcp_servers]]\nname = \"review\"\ncommand = \"ordinary-user-config\"\n";
    std::fs::write(&ordinary, ordinary_text).unwrap();
    let source = sb.cwd.join("source.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "review": { "type": "http", "url": "https://review.example.test/mcp" }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "--config",
            explicit.to_str().unwrap(),
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .success();

    let config = load_config(&explicit);
    assert_eq!(config.mcp_servers[0].name, "review");
    assert_eq!(
        config.tui.unwrap().permissions.net,
        vec!["review.example.test"]
    );
    assert_eq!(
        std::fs::read_to_string(ordinary).unwrap(),
        ordinary_text,
        "an explicit co-located target must not be redirected or rejected by the ordinary user config"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn explicit_grant_uses_the_explicit_base_merge_strategy() {
    for (explicit_strategy, user_strategy, succeeds) in
        [("append", "replace", true), ("replace", "append", false)]
    {
        let sb = sandbox();
        std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
        std::fs::write(sb.cwd.join(".newt/config.toml"), "mcp_servers = []\n").unwrap();
        std::fs::write(
            sb.config_dir.join("config.toml"),
            format!("[merge]\narrays = \"{user_strategy}\"\n"),
        )
        .unwrap();
        let explicit = sb.config_dir.join("explicit.toml");
        std::fs::write(
            &explicit,
            format!("[merge]\narrays = \"{explicit_strategy}\"\n"),
        )
        .unwrap();
        let source = sb.cwd.join("source.json");
        std::fs::write(
            &source,
            r#"{ "mcpServers": {
                  "review": { "type": "http", "url": "https://review.example.test/mcp" }
            } }"#,
        )
        .unwrap();

        let mut command = newt(&sb);
        command.args([
            "--config",
            explicit.to_str().unwrap(),
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
        ]);
        if succeeds {
            command.assert().success();
            assert_eq!(load_config(&explicit).mcp_servers[0].name, "review");
        } else {
            command.assert().failure().stderr(predicate::str::contains(
                "replaces the base mcp_servers array",
            ));
            assert!(load_config(&explicit).mcp_servers.is_empty());
        }
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_grant_net_refuses_a_cross_file_legacy_override() {
    // Adding `review` to config.toml would shadow the mcp.toml copy. When
    // config.toml already holds it, the update is in place instead (#3062
    // review, F4; see force_import_updates_the_owner_in_place_beside_a_shadowed_copy).
    let sb = sandbox();
    let breakout = sb.config_dir.join("mcp.toml");
    let breakout_text = "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://old.example.test/mcp\"\n";
    std::fs::write(&breakout, breakout_text).unwrap();
    let config = sb.config_dir.join("config.toml");
    let source = sb.cwd.join("source.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": {
              "review": { "type": "http", "url": "https://new.example.test/mcp" }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
            "--force",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cross-file overwrite"))
        .stderr(predicate::str::contains(format!(
            "`newt --config {} mcp remove review`",
            newt_core::mcp::shell_quote_arg(&breakout.to_string_lossy())
        )));

    assert_eq!(std::fs::read_to_string(breakout).unwrap(), breakout_text);
    assert!(!config.exists());
}

#[test]
#[ignore = "real killed-process acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn grant_net_kill_boundaries_recover_to_an_atomic_connector_and_grant() {
    fn wait_for(path: &Path, child: &mut std::process::Child, step: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !path.exists() && std::time::Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("import exited before {step} failpoint: {status}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !path.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for {step} failpoint");
        }
    }

    for (step, committed_before_kill) in [("staged", false), ("replaced", true)] {
        let sb = sandbox();
        let source = sb.cwd.join("source.json");
        std::fs::write(
            &source,
            r#"{ "mcpServers": {
                  "review": { "type": "http", "url": "https://review.example.test/mcp" }
            } }"#,
        )
        .unwrap();
        let ready = sb.cwd.join(format!("{step}-ready"));
        let newt_bin = assert_cmd::cargo::cargo_bin("newt");
        let mut child = std::process::Command::new(newt_bin);
        crate::common::isolate(&mut child, &sb.home);
        child
            .env("NEWT_CONFIG_DIR", &sb.config_dir)
            .current_dir(&sb.cwd)
            .env("NEWT_TEST_MCP_IMPORT_KILL_AFTER", step)
            .env("NEWT_TEST_MCP_IMPORT_READY", &ready)
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
                "--grant-net",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = child.spawn().unwrap();
        wait_for(&ready, &mut child, step);
        child.kill().unwrap();
        child.wait().unwrap();

        let config_path = sb.config_dir.join("config.toml");
        assert_eq!(config_path.exists(), committed_before_kill);
        if committed_before_kill {
            let config = load_config(&config_path);
            assert_eq!(config.mcp_servers[0].name, "review");
            assert_eq!(
                config.tui.unwrap().permissions.net,
                vec!["review.example.test"]
            );
        }

        newt(&sb)
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
                "--grant-net",
                "--force",
            ])
            .assert()
            .success();
        let config = load_config(&config_path);
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(
            config.tui.unwrap().permissions.net,
            vec!["review.example.test"]
        );
        assert!(std::fs::read_dir(&sb.config_dir)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")));
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn a_grant_import_does_not_revive_a_dropped_stub_over_the_live_copy() {
    // #3062 review (R2-7): config.toml holds a `docs` whose url is commented
    // out, so discovery drops it; mcp.toml holds the live `docs`, paused. A
    // --grant-net import writes config.toml, and F4 lets it update an entry
    // that file really holds. Merging into the stub instead would revive it
    // over the paused copy and grant its host, so the cross-file refusal
    // stands and neither file changes.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    let mcp = sb.config_dir.join("mcp.toml");
    let stub = "[[mcp_servers]]\nname = \"docs\"\ntype = \"http\"\n# url = \"https://docs.example.test/mcp\"\n";
    let live = "[[mcp_servers]]\nname = \"docs\"\ntype = \"http\"\nurl = \"https://docs.example.test/mcp\"\nenabled = false # paused until security review\n";
    std::fs::write(&config, stub).unwrap();
    std::fs::write(&mcp, live).unwrap();
    let source = sb.cwd.join("s.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "docs": { "type": "http", "url": "https://docs.example.test/mcp" } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args(["mcp", "import", source.to_str().unwrap(), "--name", "docs"])
        .args(["--force", "--grant-net"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing a cross-file overwrite"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), stub);
    assert_eq!(std::fs::read_to_string(&mcp).unwrap(), live);
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn the_cross_file_refusal_leads_with_the_route_that_keeps_the_copy() {
    // #3062 review (R3-6): a --grant-net import writes config.toml, so it
    // refuses a name the break-out mcp.toml holds. Its one remedy, removing
    // that copy, dropped `enabled = false` and the timeout: the re-run added
    // the server enabled and granted its host, or, with a dropped stub in
    // config.toml, revived the stub (R2-7). The refusal leads with the re-run
    // without --grant-net, which keeps the copy as it is, names the host to
    // grant and the stub, and says what the remove, last, loses.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    let mcp = sb.config_dir.join("mcp.toml");
    let live = "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://review.example.test/mcp\"\nenabled = false # paused: security review pending\nrequest_timeout_secs = 90\n";
    let stub = "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\n# url = \"https://review.example.test/mcp\"\n";
    let source = sb.cwd.join("s.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": { "type": "http", "url": "https://review.example.test/mcp" } } }"#,
    )
    .unwrap();
    let import = |extra: &[&str]| {
        newt(&sb)
            .args(["mcp", "import", source.to_str().unwrap()])
            .args(["--name", "review", "--force"])
            .args(extra)
            .assert()
    };

    for held in [None, Some(stub)] {
        std::fs::write(&mcp, live).unwrap();
        match held {
            Some(text) => std::fs::write(&config, text).unwrap(),
            None => assert!(!config.exists()),
        }
        let output = import(&["--grant-net"]).failure().get_output().clone();
        let refusal = String::from_utf8(output.stderr).unwrap();
        let rerun = refusal.find("Re-run without --grant-net").expect(&refusal);
        let remove = refusal.find("mcp remove review`").expect(&refusal);
        assert!(rerun < remove, "{refusal}");
        assert!(
            refusal[rerun..remove].contains("review.example.test in trusted [tui.permissions] net"),
            "{refusal}"
        );
        assert!(refusal[remove..].contains("`enabled = false`"), "{refusal}");
        assert_eq!(
            refusal.contains("only a `review` entry that discovery drops"),
            held.is_some(),
            "{refusal}"
        );
        assert_eq!(std::fs::read_to_string(&mcp).unwrap(), live);

        // Following it keeps the paused, tuned copy as it was.
        import(&[])
            .success()
            .stdout(predicate::str::contains("1 unchanged"));
        assert_eq!(std::fs::read_to_string(&mcp).unwrap(), live);
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn a_grant_import_is_not_blocked_by_an_entry_discovery_drops() {
    // #3062 review (R2-3): C2's rule, that an entry discovery drops counts
    // for nothing, also holds for the other files a --grant-net import
    // checks: an appended project and the break-out mcp.toml. A `docs` stub
    // with no url in either neither refuses nor skips the import, which
    // adds a working `docs` to config.toml and says so, and leaves the stub.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    let stub = "[[mcp_servers]]\nname = \"docs\"\ntype = \"http\"\n";
    let appended = format!("[merge]\narrays = \"append\"\n\n{stub}");
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    let source = sb.cwd.join("src.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "docs": { "type": "http", "url": "https://docs.example.test/mcp" } } }"#,
    )
    .unwrap();
    for (holder, text) in [
        (sb.cwd.join(".newt").join("config.toml"), appended.as_str()),
        (sb.config_dir.join("mcp.toml"), stub),
    ] {
        for mode in [&[][..], &["--merge"][..], &["--force"][..]] {
            std::fs::write(&config, "[tui.permissions]\nnet = []\n").unwrap();
            std::fs::write(&holder, text).unwrap();
            newt(&sb)
                .args(["mcp", "import", source.to_str().unwrap(), "--name", "docs"])
                .arg("--grant-net")
                .args(mode)
                .assert()
                .success()
                .stdout(predicate::str::contains(
                    "Results: 1 added, 0 replaced, 0 unchanged, 0 skipped.",
                ));
            assert_eq!(std::fs::read_to_string(&holder).unwrap(), text);
            let saved = load_config(&config);
            assert_eq!(
                saved.mcp_servers[0].url.as_deref(),
                Some("https://docs.example.test/mcp"),
                "{mode:?}"
            );
        }
        std::fs::remove_file(&holder).unwrap();
    }
}
