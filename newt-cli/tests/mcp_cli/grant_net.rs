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
        .stdout(predicate::str::contains("Overwrote 1"));
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
    for target_also_exists in [false, true] {
        let sb = sandbox();
        let breakout = sb.config_dir.join("mcp.toml");
        let breakout_text = "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://old.example.test/mcp\"\n";
        std::fs::write(&breakout, breakout_text).unwrap();
        let config = sb.config_dir.join("config.toml");
        let config_text = target_also_exists.then_some(
            "[[mcp_servers]]\nname = \"review\"\ntype = \"http\"\nurl = \"https://active.example.test/mcp\"\n",
        );
        if let Some(text) = config_text {
            std::fs::write(&config, text).unwrap();
        }
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
            .stderr(predicate::str::contains("Remove the existing server first"));

        assert_eq!(std::fs::read_to_string(breakout).unwrap(), breakout_text);
        match config_text {
            Some(text) => assert_eq!(std::fs::read_to_string(config).unwrap(), text),
            None => assert!(!config.exists()),
        }
    }
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
        child
            .env("NEWT_CONFIG_DIR", &sb.config_dir)
            .env("HOME", &sb.home)
            .env("USERPROFILE", &sb.home)
            .env_remove("NEWT_CONFIG")
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
