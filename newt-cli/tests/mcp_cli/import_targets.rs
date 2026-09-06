use super::*;

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_writes_a_claude_json_into_the_broken_out_mcp_toml() {
    let sb = sandbox();
    let claude = sb.cwd.join("claude.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": {
              "fs": { "command": "npx", "args": ["-y", "@mcp/fs"], "env": { "ROOT": "${MCP_ROOT}" } },
              "gh": { "command": "gh-mcp", "env": { "GH_TOKEN": "${MY_GH_TOKEN}" } }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args(["mcp", "import", claude.to_str().unwrap(), "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported 2 MCP server(s)"))
        .stdout(predicate::str::contains("mcp.toml"));

    // import breaks config out to ~/.newt/mcp.toml (created), NOT config.toml.
    let mcp_toml = sb.config_dir.join("mcp.toml");
    assert!(mcp_toml.is_file(), "import created ~/.newt/mcp.toml");
    assert!(!sb.config_dir.join("config.toml").exists());
    let servers = newt_core::mcp::parse_newt_mcp_toml(&std::fs::read_to_string(&mcp_toml).unwrap());
    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["fs", "gh"]);
    assert_eq!(
        servers
            .iter()
            .find(|s| s.name == "fs")
            .unwrap()
            .env
            .get("ROOT")
            .and_then(newt_core::mcp::SecretValue::as_literal),
        Some("${MCP_ROOT}")
    );
    // Claude's `${VAR}` environment reference is preserved verbatim (newt
    // resolves it host-side at spawn); no resolved secret lands on disk.
    let gh = servers.iter().find(|s| s.name == "gh").unwrap();
    assert_eq!(
        gh.env
            .get("GH_TOKEN")
            .and_then(newt_core::mcp::SecretValue::as_literal),
        Some("${MY_GH_TOKEN}")
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_selective_claude_import_grants_only_the_exact_mcp_host() {
    let sb = sandbox();
    let claude = sb.home.join(".claude.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": {
              "review": {
                "type": "http",
                "url": "https://BROKER.Example.test:8443/mcp",
                "headers": {
                  "X-Token": "${REVIEW_TOKEN}"
                }
              },
              "unrelated": { "command": "unrelated-mcp" }
        } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            "--from-claude",
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported 1 MCP server(s)"))
        .stdout(predicate::str::contains("broker.example.test"));

    let config_path = sb.config_dir.join("config.toml");
    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "grant-net uses one authoritative config document"
    );
    let servers = load_config(&config_path).mcp_servers;
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "review");
    assert_eq!(
        servers[0].url.as_deref(),
        Some("https://broker.example.test:8443/mcp")
    );
    assert_eq!(
        servers[0]
            .headers
            .get("X-Token")
            .and_then(newt_core::mcp::SecretValue::as_literal),
        Some("${REVIEW_TOKEN}")
    );

    let config = load_config(&config_path);
    assert_eq!(
        config.tui.unwrap().permissions.net,
        vec!["broker.example.test"]
    );

    // Re-importing and re-granting updates the server but never widens or
    // duplicates the exact-host authority.
    newt(&sb)
        .args([
            "mcp",
            "import",
            "--from-claude",
            "--name",
            "review",
            "--grant-net",
            "--force",
        ])
        .assert()
        .success();
    let config = load_config(&config_path);
    assert_eq!(
        config.tui.unwrap().permissions.net,
        vec!["broker.example.test"]
    );
    assert!(
        std::fs::read_dir(&sb.config_dir)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")),
        "atomic import must not leave preparation files behind"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_project_scope_is_refused_without_mutating_borrowed_authority() {
    let sb = sandbox();
    let project_dir = sb.cwd.join(".newt");
    std::fs::create_dir_all(&project_dir).unwrap();
    let project_config = project_dir.join("config.toml");
    let original = "[[mcp_servers]]\nname = \"borrowed\"\ncommand = \"borrowed-mcp\"\n";
    std::fs::write(&project_config, original).unwrap();
    let source = sb.cwd.join("source.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": { "command": "review-mcp" } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
            "--project",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("project MCP authority"))
        .stderr(predicate::str::contains("borrowed and untrusted"));

    assert_eq!(std::fs::read_to_string(project_config).unwrap(), original);
    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "a project-scoped import must not be promoted to trusted user config"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_codex_import_preserves_only_environment_references() {
    let sb = sandbox();
    let codex_home = sb.home.join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(
        codex_home.join("config.toml"),
        r#"
[mcp_servers.review]
url = "https://review-broker.example.test/mcp"
bearer_token_env_var = "REVIEW_TOKEN"
env_http_headers = { X-Trace = "TRACE_TOKEN" }
"#,
    )
    .unwrap();

    newt(&sb)
        .env("CODEX_HOME", &codex_home)
        .args([
            "mcp",
            "import",
            "--from-codex",
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .success();

    let servers = load_config(&sb.config_dir.join("config.toml")).mcp_servers;
    let review = &servers[0];
    assert_eq!(
        review
            .headers
            .get("Authorization")
            .and_then(newt_core::mcp::SecretValue::as_literal),
        Some("Bearer ${env:REVIEW_TOKEN}")
    );
    assert!(matches!(
        review.headers.get("X-Trace"),
        Some(newt_core::mcp::SecretValue::Ref(reference))
            if reference.env.as_deref() == Some("TRACE_TOKEN")
    ));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_codex_import_hides_malformed_source_and_accounts_for_rejected_entries() {
    let sb = sandbox();
    let codex_home = sb.home.join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    let config = codex_home.join("config.toml");
    let secret = "malformed-source-secret-7284";
    std::fs::write(
        &config,
        format!(
            "[mcp_servers.review]\nurl = \"https://example.test/mcp\"\nhttp_headers = {{ Authorization = \"{secret}\""
        ),
    )
    .unwrap();
    newt(&sb)
        .env("CODEX_HOME", &codex_home)
        .args(["mcp", "import", "--from-codex", "--name", "review"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains("source is not valid Codex TOML"));
    assert!(!sb.config_dir.join("mcp.toml").exists());

    std::fs::write(
        &config,
        r#"
[mcp_servers.valid]
command = "valid-mcp"

[mcp_servers.rejected]
command = "rejected-mcp"
unsupported_policy = "must-not-be-erased"
"#,
    )
    .unwrap();
    newt(&sb)
        .env("CODEX_HOME", &codex_home)
        .args(["mcp", "import", "--from-codex", "--all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot import all"));
    assert!(!sb.config_dir.join("mcp.toml").exists());

    newt(&sb)
        .env("CODEX_HOME", &codex_home)
        .args(["mcp", "import", "--from-codex", "--name", "valid"])
        .assert()
        .success();
    let imported = std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap();
    assert!(imported.contains("valid"));
    assert!(!imported.contains("rejected"));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_checks_user_config_even_when_ambient_config_is_active() {
    let sb = sandbox();
    std::fs::write(
        sb.cwd.join("newt.toml"),
        "default_tier_order = [\"FAST\"]\n",
    )
    .unwrap();
    std::fs::write(
        sb.config_dir.join("config.toml"),
        "[[mcp_servers]]\nname = \"review\"\ncommand = \"already-owned\"\n",
    )
    .unwrap();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": { "command": "borrowed" } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("can outrank"))
        .stderr(predicate::str::contains("config.toml"));
    assert!(!sb.config_dir.join("mcp.toml").exists());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_refuses_an_ineffective_grant_under_ambient_config() {
    let sb = sandbox();
    let ambient = sb.cwd.join("newt.toml");
    let ambient_original = "default_tier_order = [\"FAST\"]\n";
    std::fs::write(&ambient, ambient_original).unwrap();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": {
            "type": "http", "url": "https://broker.example.test/mcp"
        } } }"#,
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
        .stderr(predicate::str::contains("ambient"))
        .stderr(predicate::str::contains("active base config"));
    assert_eq!(std::fs::read_to_string(ambient).unwrap(), ambient_original);
    assert!(!sb.config_dir.join("mcp.toml").exists());
    assert!(!sb.config_dir.join("config.toml").exists());
}

#[cfg(unix)]
#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_grant_net_commits_to_config_without_following_breakout_symlink() {
    use std::os::unix::fs::symlink;

    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": {
            "type": "http", "url": "https://broker.example.test/mcp"
        } } }"#,
    )
    .unwrap();

    let referent = sb.home.join("real-mcp.toml");
    let original = "# operator-owned referent\n";
    std::fs::write(&referent, original).unwrap();
    symlink(&referent, sb.config_dir.join("mcp.toml")).unwrap();

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

    assert_eq!(std::fs::read_to_string(&referent).unwrap(), original);
    assert!(std::fs::symlink_metadata(sb.config_dir.join("mcp.toml"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        load_config(&sb.config_dir.join("config.toml"))
            .tui
            .unwrap()
            .permissions
            .net,
        vec!["broker.example.test"]
    );
}

#[cfg(unix)]
#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_updates_once_resolved_authoritative_config_target() {
    use std::os::unix::fs::symlink;

    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": {
            "type": "http", "url": "https://broker.example.test/mcp"
        } } }"#,
    )
    .unwrap();
    let referent = sb.home.join("real-config.toml");
    let original = "default_tier_order = [\"FAST\"]\n# operator-owned permissions\n";
    std::fs::write(&referent, original).unwrap();
    symlink(&referent, sb.config_dir.join("config.toml")).unwrap();

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

    let updated = std::fs::read_to_string(&referent).unwrap();
    assert!(updated.contains(original));
    assert_eq!(
        load_config(&referent).tui.unwrap().permissions.net,
        vec!["broker.example.test"]
    );
    assert!(!sb.config_dir.join("mcp.toml").exists());
    assert!(std::fs::symlink_metadata(sb.config_dir.join("config.toml"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(windows)]
#[test]
#[ignore = "real Windows reparse-point acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_updates_once_resolved_windows_symlinked_config_target() {
    use std::os::windows::fs::symlink_file;

    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": {
            "type": "http", "url": "https://broker.example.test/mcp"
        } } }"#,
    )
    .unwrap();
    let referent = sb.home.join("real-mcp.toml");
    let original = "# operator-owned referent\n";
    std::fs::write(&referent, original).unwrap();
    symlink_file(&referent, sb.config_dir.join("mcp.toml")).unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review",
        ])
        .assert()
        .success();

    let updated = std::fs::read_to_string(&referent).unwrap();
    assert!(updated.contains(original));
    assert!(updated.contains("name = \"review\""));
    assert!(std::fs::symlink_metadata(sb.config_dir.join("mcp.toml"))
        .unwrap()
        .file_type()
        .is_symlink());
}
