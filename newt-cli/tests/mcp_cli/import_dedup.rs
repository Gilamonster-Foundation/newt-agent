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
fn force_import_updates_an_owner_an_appended_project_does_not_outrank() {
    // #3062 review (F5): with `[merge] arrays = "append"` the base entries
    // come first, so config.toml's `alpha` is effective and the project's
    // same-name entry does not outrank it. The import updates config.toml.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    std::fs::write(
        &config,
        "[[mcp_servers]]\nname = \"alpha\"\ncommand = \"/bin/true\"\nrequest_timeout_secs = 90\n",
    )
    .unwrap();
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    let project = sb.cwd.join(".newt/config.toml");
    let project_text = "[merge]\narrays = \"append\"\n\n[[mcp_servers]]\nname = \"alpha\"\ncommand = \"/bin/false\"\n";
    std::fs::write(&project, project_text).unwrap();

    force_import(
        &sb,
        r#"{ "mcpServers": { "alpha": { "command": "/bin/true", "requestTimeoutSecs": 30 } } }"#,
    )
    .success()
    .stdout(predicate::str::contains(
        "Results: 0 added, 1 replaced, 0 unchanged, 0 skipped.",
    ));
    assert!(std::fs::read_to_string(&config)
        .unwrap()
        .contains("request_timeout_secs = 30"));
    assert_eq!(std::fs::read_to_string(&project).unwrap(), project_text);
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_updates_the_owner_in_place_beside_a_shadowed_copy() {
    // #3062 review (F4, G3): config.toml owns `alpha` and mcp.toml holds an
    // older, shadowed copy. The import updates the owner in place, since the
    // copy predates it, and names the command that removes that copy: the
    // default `newt mcp remove alpha` deletes the owner instead.
    let sb = sandbox();
    let config = sb.config_dir.join("config.toml");
    let mcp = sb.config_dir.join("mcp.toml");
    let shadowed = "[[mcp_servers]]\nname = \"alpha\"\ncommand = \"/bin/true\"\nargs = [\"--stale\"]\nenv = { API_TOKEN = { env = \"OLD_TOKEN\" } }\n";
    let source = sb.cwd.join("c.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "alpha": { "command": "/bin/true", "args": ["--live"], "requestTimeoutSecs": 30 } } }"#,
    )
    .unwrap();
    for extra in [&[][..], &["--grant-net"][..]] {
        std::fs::write(
            &config,
            "[[mcp_servers]]\nname = \"alpha\"\ncommand = \"/bin/true\"\nargs = [\"--live\"]\nlogin_argv = [\"/bin/true\", \"login\"] # operator login\n",
        )
        .unwrap();
        std::fs::write(&mcp, shadowed).unwrap();
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
            .stdout(predicate::str::contains("1 replaced"))
            .stdout(predicate::str::contains(format!(
                "`newt --config {} mcp remove alpha`",
                mcp.display()
            )));
        let updated = std::fs::read_to_string(&config).unwrap();
        assert!(
            updated.contains("login_argv = [\"/bin/true\", \"login\"] # operator login"),
            "{updated}"
        );
        assert!(updated.contains("request_timeout_secs = 30"), "{updated}");
        assert_eq!(std::fs::read_to_string(&mcp).unwrap(), shadowed);
    }
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

/// Writes `source` as the borrowed config and runs `newt mcp import <source>
/// --all --force`.
fn force_import(sb: &Sandbox, json: &str) -> assert_cmd::assert::Assert {
    let source = sb.cwd.join("c.json");
    std::fs::write(&source, json).unwrap();
    newt(sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--all",
            "--force",
        ])
        .assert()
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_applies_an_explicit_empty_source_value() {
    // #2915 review: `"args": []` is an explicit value, not an omitted field.
    // Only a field the source omits keeps its local value.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    std::fs::write(
        &mcp,
        "[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"\nargs = [\"--old-target\"]\nrequest_timeout_secs = 90\n",
    )
    .unwrap();

    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "command": "fs-mcp", "args": [] } } }"#,
    )
    .success()
    .stdout(predicate::str::contains(
        "Results: 0 added, 1 replaced, 0 unchanged, 0 skipped.",
    ));

    let servers = newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(&mcp).unwrap());
    assert!(servers[0].args.is_empty(), "explicit empty args applied");
    assert_eq!(
        servers[0].request_timeout_secs,
        Some(90),
        "omitted field kept"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_keeps_comments_and_counts_an_identical_entry_unchanged() {
    // #2915 review: a same-value import is a no-op, not a replacement that
    // erases the operator's comments; a changed value keeps them too.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    let original = "[[mcp_servers]]\nname = \"fs\" # identity note\ncommand = \"fs-mcp\" # operator note\nargs = [\"--a\"]\n";
    std::fs::write(&mcp, original).unwrap();

    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "command": "fs-mcp", "args": ["--a"] } } }"#,
    )
    .success()
    .stdout(predicate::str::contains(
        "Results: 0 added, 0 replaced, 1 unchanged, 0 skipped.",
    ));
    assert_eq!(std::fs::read_to_string(&mcp).unwrap(), original);

    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "command": "fs-mcp-v2", "args": ["--a"] } } }"#,
    )
    .success()
    .stdout(predicate::str::contains(
        "Results: 0 added, 1 replaced, 0 unchanged, 0 skipped.",
    ));
    let updated = std::fs::read_to_string(&mcp).unwrap();
    assert!(updated.contains("# identity note"), "{updated}");
    assert!(
        updated.contains("command = \"fs-mcp-v2\" # operator note"),
        "{updated}"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_keeps_the_file_style() {
    // #3062 review (F3): an identical entry counts as unchanged and leaves the
    // file byte for byte, whatever toml_edit would re-render; a real change
    // keeps the missing final newline, CRLF line endings and BOM.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    for original in [
        "[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"",
        "# servers\r\n[[mcp_servers]]\r\nname = \"fs\"\r\ncommand = \"fs-mcp\"\r\n\r\n[[mcp_servers]]\r\nname = \"other\"\r\ncommand = \"other-mcp\"\r\n",
        "\u{feff}[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"\n",
    ] {
        std::fs::write(&mcp, original).unwrap();
        force_import(&sb, r#"{ "mcpServers": { "fs": { "command": "fs-mcp" } } }"#)
            .success()
            .stdout(predicate::str::contains(
                "Results: 0 added, 0 replaced, 1 unchanged, 0 skipped.",
            ));
        assert_eq!(std::fs::read_to_string(&mcp).unwrap(), original);

        force_import(&sb, r#"{ "mcpServers": { "fs": { "command": "fs-v2" } } }"#)
            .success()
            .stdout(predicate::str::contains(
                "Results: 0 added, 1 replaced, 0 unchanged, 0 skipped.",
            ));
        assert_eq!(
            std::fs::read_to_string(&mcp).unwrap(),
            original.replacen("\"fs-mcp\"", "\"fs-v2\"", 1)
        );
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_refuses_to_carry_headers_to_a_changed_endpoint() {
    // #2915 review (P1): a new URL must not inherit the old destination's
    // Authorization header. Refuse without echoing the header or the URLs.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    let original = "[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://old.example.test/mcp\"\nheaders = { Authorization = \"Bearer ${REVIEW_FAKE_TOKEN}\" }\n";
    std::fs::write(&mcp, original).unwrap();

    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "type": "http", "url": "https://new.example.test/mcp" } } }"#,
    )
    .failure()
    .stderr(predicate::str::contains("refusing to send"))
    .stderr(predicate::str::contains("REVIEW_FAKE_TOKEN").not())
    .stderr(predicate::str::contains("example.test").not());
    assert_eq!(std::fs::read_to_string(&mcp).unwrap(), original);

    // Headers the source supplies itself are not a transfer.
    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "type": "http", "url": "https://new.example.test/mcp",
              "headers": { "Authorization": "Bearer ${NEW_FAKE_TOKEN}" } } } }"#,
    )
    .success()
    .stdout(predicate::str::contains("Endpoint changed for fs"));
    let servers = newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(&mcp).unwrap());
    assert_eq!(
        servers[0].headers.get("Authorization"),
        Some(&newt_core::mcp::SecretValue::literal(
            "Bearer ${NEW_FAKE_TOKEN}"
        ))
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_names_the_auth_consequence_of_a_resource_change() {
    // #2915: a saved login is bound to the exact resource, so `/mcp` → `/mcp/`
    // makes it ineligible. Say so instead of generic overwrite advice.
    let sb = sandbox();
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://api.example.test/mcp\"\n",
    )
    .unwrap();

    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "type": "http", "url": "https://api.example.test/mcp/" } } }"#,
    )
    .success()
    .stdout(predicate::str::contains(
        "Endpoint changed for fs: a saved login for the previous endpoint is not used",
    ));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_refuses_to_carry_oauth_settings_to_a_changed_endpoint() {
    // #3062 review (G5): a static OAuth client, pinned scopes and callback
    // port belong to the old endpoint's authorization server and must not
    // follow a moved URL the source names without them.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    let original = "[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://old.example.test/mcp\"\noauth_client_id = \"old-as-client\"\noauth_scopes = \"repo admin:org\"\noauth_callback_port = 8123\n";
    std::fs::write(&mcp, original).unwrap();

    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "type": "http", "url": "https://new.example.test/mcp" } } }"#,
    )
    .failure()
    .stderr(predicate::str::contains("3 local OAuth setting(s)"))
    .stderr(predicate::str::contains("old-as-client").not());
    assert_eq!(std::fs::read_to_string(&mcp).unwrap(), original);

    // An `oauth` object in the source decides all three.
    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "type": "http", "url": "https://new.example.test/mcp",
              "oauth": { "clientId": "new-client" } } } }"#,
    )
    .success()
    .stdout(predicate::str::contains("Endpoint changed for fs"));
    let servers = newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(&mcp).unwrap());
    assert_eq!(servers[0].oauth_client_id.as_deref(), Some("new-client"));
    assert_eq!(
        (
            servers[0].oauth_scopes.as_deref(),
            servers[0].oauth_callback_port
        ),
        (None, None)
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_keeps_a_bare_origin_url_as_written() {
    // #3062 review (F6): the import canonicalises `https://h` to `https://h/`,
    // the same request target, but a saved login is bound to the spelling the
    // operator configured. An identical source leaves that spelling alone.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    let original =
        "[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://api.example.test\"\n";
    std::fs::write(&mcp, original).unwrap();

    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "type": "http", "url": "https://api.example.test" } } }"#,
    )
    .success()
    .stdout(predicate::str::contains(
        "Results: 0 added, 0 replaced, 1 unchanged, 0 skipped.",
    ))
    .stdout(predicate::str::contains("Endpoint changed").not());
    assert_eq!(std::fs::read_to_string(&mcp).unwrap(), original);
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_refuses_to_carry_env_to_a_different_program() {
    // #2915: the stdio twin of the header transfer. A changed command, or the
    // same launcher running different args, must not inherit local `env`.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    let original = "[[mcp_servers]]\nname = \"fs\"\ncommand = \"npx\"\nargs = [\"old-mcp\"]\nenv = { API_TOKEN = { env = \"PRIVATE_MARKER_TOKEN\" } }\n";
    std::fs::write(&mcp, original).unwrap();

    for source in [
        r#"{ "mcpServers": { "fs": { "command": "new-mcp" } } }"#,
        r#"{ "mcpServers": { "fs": { "command": "npx", "args": ["new-mcp"] } } }"#,
    ] {
        force_import(&sb, source)
            .failure()
            .stderr(predicate::str::contains("refusing to send"))
            .stderr(predicate::str::contains("PRIVATE_MARKER_TOKEN").not())
            .stderr(predicate::str::contains("API_TOKEN").not());
        assert_eq!(std::fs::read_to_string(&mcp).unwrap(), original);
    }

    // An env the source supplies itself (here: none) is not a transfer.
    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "command": "npx", "args": ["new-mcp"], "env": {} } } }"#,
    )
    .success();
    let servers = newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(&mcp).unwrap());
    assert_eq!(servers[0].args, ["new-mcp"]);
    assert!(servers[0].env.is_empty());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_refuses_to_carry_args_to_a_different_program() {
    // #3062 review (F1): local argv, a credential flag included, must not
    // follow a changed command the source names without args.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    let original = "[[mcp_servers]]\nname = \"fs\"\ncommand = \"npx\"\nargs = [\"--api-key\", \"SYNTHETIC_SECRET_MARKER\"]\n";
    std::fs::write(&mcp, original).unwrap();

    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "command": "other-vendor-mcp" } } }"#,
    )
    .failure()
    .stderr(predicate::str::contains("2 local argument(s)"))
    .stderr(predicate::str::contains("SYNTHETIC_SECRET_MARKER").not());
    assert_eq!(std::fs::read_to_string(&mcp).unwrap(), original);

    // Args the source supplies itself are not a transfer.
    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "command": "other-vendor-mcp", "args": ["--serve"] } } }"#,
    )
    .success();
    let servers = newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(&mcp).unwrap());
    assert_eq!(servers[0].args, ["--serve"]);
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_names_the_local_settings_it_removed() {
    // #3062 review (F8): a source's `headers` replace the local map, and a
    // transport change drops the other transport's fields. Name what went,
    // never its value, instead of claiming every local setting was kept.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    let tenant = "[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://mcp.example.test/mcp\"\nheaders = { Authorization = \"Bearer ${SYNTHETIC_OLD_TOKEN}\", X-Tenant-Id = \"tenant-a\" }\n";
    let codex = sb.cwd.join("codex.toml");
    std::fs::write(
        &codex,
        "[mcp_servers.fs]\nurl = \"https://mcp.example.test/mcp\"\nbearer_token_env_var = \"NEW_TOKEN\"\n",
    )
    .unwrap();
    let codex_import = || {
        newt(&sb)
            .args(["mcp", "import", codex.to_str().unwrap(), "--all", "--force"])
            .assert()
    };
    let cases: [(&str, &dyn Fn() -> assert_cmd::assert::Assert, &str); 3] = [
        (
            tenant,
            &|| {
                force_import(
                    &sb,
                    r#"{ "mcpServers": { "fs": { "type": "http", "url": "https://mcp.example.test/mcp",
                          "headers": { "Authorization": "Bearer ${NEW_TOKEN}" } } } }"#,
                )
            },
            "fs: removed header X-Tenant-Id",
        ),
        (tenant, &codex_import, "fs: removed header X-Tenant-Id"),
        (
            "[[mcp_servers]]\nname = \"fs\"\ntype = \"http\"\nurl = \"https://old.example.test/mcp\"\nheaders = { Authorization = { env = \"SYNTHETIC_TOKEN_VAR\" } } # auth policy\n",
            &|| force_import(&sb, r#"{ "mcpServers": { "fs": { "command": "fs-mcp" } } }"#),
            "fs: removed url, header Authorization",
        ),
    ];
    for (local, import, removed) in cases {
        std::fs::write(&mcp, local).unwrap();
        import()
            .success()
            .stdout(predicate::str::contains("preserved, except:"))
            .stdout(predicate::str::contains(removed))
            .stdout(predicate::str::contains("tenant-a").not())
            .stdout(predicate::str::contains("SYNTHETIC").not())
            .stdout(predicate::str::contains("example.test").not());
    }

    // Nothing removed: the sentence stands as it was.
    std::fs::write(
        &mcp,
        "[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"\nenv = { LOG_LEVEL = \"debug\" }\n",
    )
    .unwrap();
    force_import(
        &sb,
        r#"{ "mcpServers": { "fs": { "command": "fs-mcp", "requestTimeoutSecs": 30 } } }"#,
    )
    .success()
    .stdout(predicate::str::contains(
        "local settings absent from the source were preserved.\n",
    ));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_keeps_comments_on_defaults_and_resets() {
    // #3062 review: resetting a value to its default keeps the comment on its
    // line, and an unchanged default-valued line survives a change elsewhere
    // in the same entry.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    let head = "[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"\n";
    let cases = [
        (
            "args = [\"--old-target\"] # argument policy\n",
            r#"{ "mcpServers": { "fs": { "command": "fs-mcp", "args": [] } } }"#,
            &["args = [] # argument policy"][..],
        ),
        (
            "enabled = false # operator policy\n",
            r#"{ "mcpServers": { "fs": { "command": "fs-mcp", "enabled": true } } }"#,
            &["enabled = true # operator policy"][..],
        ),
        (
            "enabled = true # enabled policy\ntype = \"stdio\" # transport policy\nrequest_timeout_secs = 90\n",
            r#"{ "mcpServers": { "fs": { "command": "fs-mcp", "enabled": true,
                  "requestTimeoutSecs": 30 } } }"#,
            &[
                "enabled = true # enabled policy",
                "type = \"stdio\" # transport policy",
                "request_timeout_secs = 30",
            ][..],
        ),
    ];
    for (local, source, expected) in cases {
        std::fs::write(&mcp, format!("{head}{local}")).unwrap();
        force_import(&sb, source)
            .success()
            .stdout(predicate::str::contains(
                "Results: 0 added, 1 replaced, 0 unchanged, 0 skipped.",
            ));
        let updated = std::fs::read_to_string(&mcp).unwrap();
        for line in expected {
            assert!(updated.contains(line), "missing `{line}` in:\n{updated}");
        }
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn force_import_updates_an_alias_spelled_field_in_place() {
    // #3062 review: `requestTimeoutSecs` is an accepted spelling of
    // `request_timeout_secs`. Importing a new value used to add the second
    // spelling, and the next `newt mcp list` rejected the file.
    let sb = sandbox();
    let mcp = sb.config_dir.join("mcp.toml");
    std::fs::write(
        &mcp,
        "[[mcp_servers]]\nname = \"fs\"\ncommand = \"fs-mcp\"\nrequestTimeoutSecs = 90 # timeout note\n",
    )
    .unwrap();
    let list = || newt(&sb).args(["mcp", "list"]).assert().success();
    list();

    let source = r#"{ "mcpServers": { "fs": { "command": "fs-mcp", "requestTimeoutSecs": 30 } } }"#;
    force_import(&sb, source)
        .success()
        .stdout(predicate::str::contains(
            "Results: 0 added, 1 replaced, 0 unchanged, 0 skipped.",
        ));
    let updated = std::fs::read_to_string(&mcp).unwrap();
    assert!(
        updated.contains("requestTimeoutSecs = 30 # timeout note"),
        "{updated}"
    );
    assert!(!updated.contains("request_timeout_secs"), "{updated}");
    list();

    force_import(&sb, source)
        .success()
        .stdout(predicate::str::contains(
            "Results: 0 added, 0 replaced, 1 unchanged, 0 skipped.",
        ));
    assert_eq!(std::fs::read_to_string(&mcp).unwrap(), updated);
}
