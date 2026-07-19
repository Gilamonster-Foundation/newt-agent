//! Process-level coverage for `newt mcp add|remove|list|install`.
//!
//! The config root is redirected via `NEWT_CONFIG_DIR`, and `HOME` + the
//! working directory point at tempdirs so the merged `list` view never reads
//! the developer's real `~/.claude.json` / `./.mcp.json` (the doctor_cli.rs
//! isolation pattern).

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

struct Sandbox {
    _root: tempfile::TempDir,
    config_dir: std::path::PathBuf,
    home: std::path::PathBuf,
    cwd: std::path::PathBuf,
}

fn sandbox() -> Sandbox {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("cfg");
    let home = root.path().join("home");
    let cwd = root.path().join("ws");
    for dir in [&config_dir, &home, &cwd] {
        std::fs::create_dir_all(dir).unwrap();
    }
    Sandbox {
        _root: root,
        config_dir,
        home,
        cwd,
    }
}

/// A `newt` invocation isolated from the developer's environment.
fn newt(sb: &Sandbox) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.env("NEWT_CONFIG_DIR", &sb.config_dir)
        .env("HOME", &sb.home)
        .env_remove("NEWT_CONFIG")
        .current_dir(&sb.cwd);
    cmd
}

fn load_config(path: &Path) -> newt_core::Config {
    newt_core::Config::load(path).unwrap()
}

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
        entry.env.get("SCRYBE_LOG").map(String::as_str),
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
fn install_scrybe_writes_the_catalog_registration() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Installed MCP server 'scrybe'"))
        .stdout(predicate::str::contains("Scrybe Markdown editor"))
        .stdout(predicate::str::contains("newt doctor"));

    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers.len(), 1);
    let entry = &cfg.mcp_servers[0];
    assert_eq!(entry.name, "scrybe");
    assert_eq!(entry.command.as_deref(), Some("scrybe-mcp-server"));
    assert_eq!(entry.args, vec!["stdio"]);
    assert!(entry.enabled);
}

#[test]
fn install_unknown_name_lists_the_available_catalog() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "install", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ghost"))
        .stderr(predicate::str::contains("available: scrybe"));
    assert!(!sb.config_dir.join("config.toml").exists());
}

#[test]
fn catalog_drop_ins_layer_user_over_bundled_and_project_over_user() {
    let sb = sandbox();
    // User drop-in overrides the bundled scrybe entry.
    std::fs::write(
        sb.config_dir.join("mcp-catalog.toml"),
        "[[servers]]\nname = \"scrybe\"\ncommand = \"scrybe-user\"\nargs = [\"stdio\"]\n",
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .success();
    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers[0].command.as_deref(), Some("scrybe-user"));
    newt(&sb)
        .args(["mcp", "remove", "scrybe"])
        .assert()
        .success();

    // Project drop-in overrides the user drop-in.
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    std::fs::write(
        sb.cwd.join(".newt").join("mcp-catalog.toml"),
        "[[servers]]\nname = \"scrybe\"\ncommand = \"scrybe-proj\"\nargs = [\"stdio\"]\n",
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .success();
    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers[0].command.as_deref(), Some("scrybe-proj"));
}

#[test]
fn malformed_catalog_drop_in_fails_install_loudly() {
    let sb = sandbox();
    std::fs::write(sb.config_dir.join("mcp-catalog.toml"), "not toml [").unwrap();
    newt(&sb)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mcp-catalog.toml"));
    assert!(!sb.config_dir.join("config.toml").exists());
}

#[test]
fn broken_catalog_drop_in_entry_fails_install_naming_the_file() {
    let sb = sandbox();
    // Parses fine, but a stdio server with no command can never connect.
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    std::fs::write(
        sb.cwd.join(".newt").join("mcp-catalog.toml"),
        "[[servers]]\nname = \"half\"\ndescription = \"broken on purpose\"\n",
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "install", "half"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("half"))
        .stderr(predicate::str::contains("mcp-catalog.toml"))
        .stderr(predicate::str::contains("command"));
    assert!(!sb.config_dir.join("config.toml").exists());
}

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
    let row = |name: &str| {
        stdout
            .lines()
            .find(|l| l.starts_with(name))
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
