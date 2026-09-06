use super::*;

#[test]
fn add_aborts_on_an_unreadable_config_without_truncating_it() {
    let sb = sandbox();
    let cfg_path = sb.config_dir.join("config.toml");
    // One invalid-UTF-8 byte (0xE9): read_to_string fails with InvalidData.
    // Treating that as "empty config" would rewrite — i.e. silently truncate —
    // the user's whole config to the single appended entry.
    let original: &[u8] = b"default_backend = \"caf\xE9\"\n";
    std::fs::write(&cfg_path, original).unwrap();

    newt(&sb)
        .args(["mcp", "add", "x", "--command", "y"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("config.toml"));
    assert_eq!(
        std::fs::read(&cfg_path).unwrap(),
        original,
        "an unreadable config must be left byte-for-byte untouched"
    );
}

#[test]
fn add_honors_the_global_config_flag_and_list_sees_it() {
    let sb = sandbox();
    let custom = sb.cwd.join("custom.toml");
    let custom_str = custom.to_str().unwrap().to_string();
    newt(&sb)
        .args(["--config", &custom_str, "mcp", "add", "x", "--command", "y"])
        .assert()
        .success()
        .stdout(predicate::str::contains("custom.toml"));
    // The same --config invocation of the reader sees the entry.
    newt(&sb)
        .args(["--config", &custom_str, "mcp", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("x"))
        .stdout(predicate::str::contains("newt config"));
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "--config must divert the write away from the user config"
    );
}

#[test]
fn add_honors_newt_config_env_as_the_write_target() {
    let sb = sandbox();
    let env_cfg = sb.cwd.join("env-config.toml");
    newt(&sb)
        .env("NEWT_CONFIG", &env_cfg)
        .args(["mcp", "add", "x", "--command", "y"])
        .assert()
        .success();
    let cfg = load_config(&env_cfg);
    assert_eq!(cfg.mcp_servers[0].name, "x");
    assert!(!sb.config_dir.join("config.toml").exists());
}

#[test]
fn add_prefers_cwd_newt_toml_when_present() {
    let sb = sandbox();
    // resolve()'s base search prefers ./newt.toml over the user config —
    // the write must land in the file the reader will actually consult.
    let newt_toml = sb.cwd.join("newt.toml");
    std::fs::write(&newt_toml, "# local config\n").unwrap();
    newt(&sb)
        .args(["mcp", "add", "x", "--command", "y"])
        .assert()
        .success();
    let text = std::fs::read_to_string(&newt_toml).unwrap();
    assert!(text.contains("# local config"), "comment kept: {text}");
    assert!(text.contains("name = \"x\""), "entry written: {text}");
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "user config must stay untouched when ./newt.toml is the reader's base"
    );
    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("x"));
}

#[test]
fn project_flag_edits_the_ancestor_project_config_from_a_subdir() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "add", "root-srv", "--command", "r", "--project"])
        .assert()
        .success();
    let root_cfg = sb.cwd.join(".newt").join("config.toml");
    assert!(root_cfg.is_file());

    // From a subdirectory, --project must edit the ANCESTOR project config —
    // forking a nested .newt/ would shadow the repo root's from that subtree.
    let sub = sb.cwd.join("crates").join("x");
    std::fs::create_dir_all(&sub).unwrap();
    let mut cmd = newt(&sb);
    cmd.current_dir(&sub)
        .args(["mcp", "add", "sub-srv", "--command", "s", "--project"])
        .assert()
        .success();
    assert!(
        !sub.join(".newt").exists(),
        "must not fork a nested project config"
    );
    let cfg = load_config(&root_cfg);
    let names: Vec<&str> = cfg.mcp_servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["root-srv", "sub-srv"]);

    // And remove --project from the subdir finds the same file.
    let mut cmd = newt(&sb);
    cmd.current_dir(&sub)
        .args(["mcp", "remove", "sub-srv", "--project"])
        .assert()
        .success();
    let cfg = load_config(&root_cfg);
    assert_eq!(cfg.mcp_servers.len(), 1);
}

#[test]
fn list_fails_loudly_on_a_broken_newt_config() {
    let sb = sandbox();
    std::fs::write(sb.config_dir.join("config.toml"), "not toml [").unwrap();
    // Swallowing the broken config and printing an empty view would
    // contradict the command's own show-and-flag contract.
    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("TOML"));
}

#[test]
fn list_attributes_claude_code_overlays_to_their_files() {
    let sb = sandbox();
    std::fs::write(
        sb.home.join(".claude.json"),
        r#"{ "mcpServers": { "user-srv": { "command": "u" } } }"#,
    )
    .unwrap();
    std::fs::write(
        sb.cwd.join(".mcp.json"),
        r#"{ "mcpServers": { "proj-srv": { "command": "p" } } }"#,
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "add", "mine", "--command", "m"])
        .assert()
        .success();

    let assert = newt(&sb).args(["mcp", "list"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // `newt mcp list` emits a GFM pipe table since #1916, so a row no longer
    // STARTS with the name. Matching the first CELL is also stricter than the
    // `starts_with` it replaces, which would have accepted `mine-2` as `mine`.
    let row = |name: &str| {
        stdout
            .lines()
            .find(|l| l.split('|').nth(1).is_some_and(|cell| cell.trim() == name))
            .unwrap_or_else(|| panic!("no row for {name} in:\n{stdout}"))
            .to_string()
    };
    assert!(row("mine").contains("newt config"), "{stdout}");
    assert!(row("user-srv").contains("claude-code (user)"), "{stdout}");
    assert!(
        row("proj-srv").contains("claude-code (project)"),
        "{stdout}"
    );
}

/// Bare `newt mcp` with PIPED stdin (not a TTY) must SERVE — the
/// backward-compatible path every `claude mcp add newt -- newt mcp`
/// config relies on. Feeding a single `initialize` and closing stdin
/// (EOF) must yield a JSON-RPC response and a clean exit — never a hang.
#[test]
fn bare_mcp_with_piped_stdin_serves_and_does_not_hang() {
    let sb = sandbox();
    let init = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n";
    newt(&sb)
        // Unreachable Ollama: with the verbatim contract, discover() does
        // not probe and `initialize` never touches it (stdout_purity.rs).
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        .arg("mcp")
        .write_stdin(init)
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicate::str::contains("\"jsonrpc\""));
}

#[test]
fn add_project_writes_the_project_config_not_the_user_config() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "add", "proj-fs", "--command", "mcp-fs", "--project"])
        .assert()
        .success()
        .stdout(predicate::str::contains(".newt"));

    let project_config = sb.cwd.join(".newt").join("config.toml");
    let cfg = load_config(&project_config);
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(cfg.mcp_servers[0].name, "proj-fs");
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "--project must not touch the user config"
    );
}

// ---------------------------------------------------------------------------
// ~/.newt/mcp.toml — broken-out source: write-target preference + list source
// ---------------------------------------------------------------------------

#[test]
fn add_prefers_an_existing_mcp_toml_over_config_toml() {
    let sb = sandbox();
    // Once the operator has broken config out, `add` lands in ~/.newt/mcp.toml.
    let mcp_toml = sb.config_dir.join("mcp.toml");
    std::fs::write(&mcp_toml, "# broken-out MCP config\n").unwrap();
    newt(&sb)
        .args(["mcp", "add", "fs", "--command", "mcp-fs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mcp.toml"));

    let text = std::fs::read_to_string(&mcp_toml).unwrap();
    assert!(
        text.contains("# broken-out MCP config"),
        "comment kept: {text}"
    );
    assert!(
        text.contains("name = \"fs\""),
        "entry written to mcp.toml: {text}"
    );
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "config.toml must stay untouched when mcp.toml exists"
    );

    // `list` attributes the row to the mcp.toml source.
    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fs"))
        .stdout(predicate::str::contains("newt mcp.toml"));
}

#[test]
fn add_uses_config_toml_when_no_mcp_toml_exists() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "add", "fs", "--command", "mcp-fs"])
        .assert()
        .success();
    // Default (no mcp.toml) keeps #1291 behavior: user config.toml.
    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers[0].name, "fs");
    assert!(!sb.config_dir.join("mcp.toml").exists());
}
