//! Process-level coverage for `newt mcp add|remove|list|install|import`.
//!
//! The config root is redirected via `NEWT_CONFIG_DIR`, and `HOME` + the
//! working directory point at tempdirs so the merged `list` view never reads
//! the developer's real `~/.claude.json` / `./.mcp.json` (the doctor_cli.rs
//! isolation pattern).
//!
//! `mcp import` cases execute the real CLI and filesystem. They ground the pure
//! parser, sanitizer, selection, and staged-write regressions in
//! `newt-core::mcp`, `newt-core::config`, and `newt-cli::mcp_cmd`; each is
//! ignored in per-PR CI and run serially by the weekly/release acceptance lane.
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:22 EDT | Date: 2026-08-12

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
    // The workspace MUST live UNDER the sandbox home (#1494). The project-config
    // walk-up (`find_project_config_from`) stops at `home_dir()`; if `cwd` and
    // `home` were siblings, the boundary is never an ancestor of `cwd`, so on
    // Windows — where the temp dir lives under `C:\Users\<user>` — the walk-up
    // sails up past the real home and writes fixtures into the developer's real
    // `~/.newt/config.toml`. Nesting `cwd` under `home` makes the boundary a true
    // ancestor, so the search is contained on every OS (production is already
    // safe: there the real home genuinely is an ancestor of `cwd`).
    let cwd = home.join("ws");
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
        // `home_dir()` reads HOME then USERPROFILE; set both so home resolution is
        // contained on Windows too, not just Unix (#1494).
        .env("USERPROFILE", &sb.home)
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
fn install_scrybe_resolves_the_binary_to_an_absolute_path() {
    // scrybe smart-install: with the binary present on PATH, it is registered
    // by ABSOLUTE path so the server survives later PATH changes.
    let sb = sandbox();
    let bin = sb.home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let scrybe_bin = bin.join("scrybe-mcp-server");
    std::fs::write(&scrybe_bin, "#!/bin/sh\n").unwrap();

    newt(&sb)
        .env("PATH", &bin)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Installed MCP server 'scrybe'"))
        .stdout(predicate::str::contains("Scrybe Markdown editor"))
        .stdout(predicate::str::contains("Resolved command to"))
        .stdout(predicate::str::contains("newt doctor"));

    let cfg = load_config(&sb.config_dir.join("config.toml"));
    assert_eq!(cfg.mcp_servers.len(), 1);
    let entry = &cfg.mcp_servers[0];
    assert_eq!(entry.name, "scrybe");
    assert_eq!(entry.command.as_deref(), Some(scrybe_bin.to_str().unwrap()));
    assert_eq!(entry.args, vec!["stdio"]);
    assert!(entry.enabled);
}

#[test]
fn install_scrybe_without_the_binary_hints_pip() {
    // The bundled scrybe entry with NO binary anywhere is a hard error naming
    // the pip package — the "special relationship" that removes setup friction.
    let sb = sandbox();
    let empty = sb.home.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    newt(&sb)
        .env("PATH", &empty)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pip install scrybe.ai"));
    // Nothing was registered.
    assert!(!sb.config_dir.join("config.toml").exists());
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
    // Pin an empty PATH: a user/project drop-in that overrides scrybe keeps its
    // own bare command when the binary is absent (only the BUNDLED scrybe entry
    // hard-fails with a pip hint), so this exercises catalog layering cleanly.
    let empty = sb.home.join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    // User drop-in overrides the bundled scrybe entry.
    std::fs::write(
        sb.config_dir.join("mcp-catalog.toml"),
        "[[servers]]\nname = \"scrybe\"\ncommand = \"scrybe-user\"\nargs = [\"stdio\"]\n",
    )
    .unwrap();
    newt(&sb)
        .env("PATH", &empty)
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
        .env("PATH", &empty)
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
fn uat_import_rejects_literal_url_and_argument_credentials_without_leaking_them() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    let cases = [
        (
            "userinfo-secret-4319",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://user:userinfo-secret-4319@broker.example.test/mcp"
            } } }"#,
        ),
        (
            "query-secret-8274",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://broker.example.test/mcp?api_key=query-secret-8274"
            } } }"#,
        ),
        (
            "argument-secret-6621",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["--token", "argument-secret-6621"]
            } } }"#,
        ),
    ];

    for (secret, json) in cases {
        std::fs::write(&source, json).unwrap();
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
            .stdout(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains("move credentials"));
        assert!(
            !sb.config_dir.join("mcp.toml").exists(),
            "a rejected credential must never enter mcp.toml"
        );
        assert!(
            !sb.config_dir.join("config.toml").exists(),
            "a rejected credential must never create a network grant"
        );
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_claude_import_rejects_literal_env_and_headers_without_leaking_values() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    for (secret, json, borrowed_key) in [
        (
            "literal-env-value-7741",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "env": { "ACCESS_TOKEN": "literal-env-value-7741" }
            } } }"#,
            "review.env.ACCESS_TOKEN",
        ),
        (
            "literal-header-value-8832",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://broker.example.test/mcp",
                "headers": { "Authorization": "Bearer literal-header-value-8832" }
            } } }"#,
            "review.headers.Authorization",
        ),
    ] {
        std::fs::write(&source, json).unwrap();
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
            .stdout(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains("credential field(s)"))
            .stderr(predicate::str::contains(borrowed_key).not())
            .stderr(predicate::str::contains(secret).not());
        assert!(!sb.config_dir.join("mcp.toml").exists());
        assert!(!sb.config_dir.join("config.toml").exists());
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_encoded_and_alias_credentials_without_leaking_them() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    let cases = [
        (
            "client-secret-value-3197",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://broker.example.test/mcp?client%5Fsecret=client-secret-value-3197"
            } } }"#,
        ),
        (
            "signature-value-8264",
            r#"{ "mcpServers": { "review": {
                "type": "http",
                "url": "https://broker.example.test/mcp?X-Amz-Signature=signature-value-8264"
            } } }"#,
        ),
        (
            "access-token-value-5561",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["--access-token", "access-token-value-5561"]
            } } }"#,
        ),
        (
            "header-token-value-4428",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["-H", "Authorization: Bearer header-token-value-4428"]
            } } }"#,
        ),
        (
            "api-key-header-value-6173",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["-H", "X-API-Key: api-key-header-value-6173"]
            } } }"#,
        ),
        (
            "client-secret-header-value-2841",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["--header=X-Client-Secret: client-secret-header-value-2841"]
            } } }"#,
        ),
        (
            "auth-token-header-value-3902",
            r#"{ "mcpServers": { "review": {
                "command": "review-mcp",
                "args": ["-HX-Auth-Token: auth-token-header-value-3902"]
            } } }"#,
        ),
    ];

    for (secret, json) in cases {
        std::fs::write(&source, json).unwrap();
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
            .stdout(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains("credential"));
        assert!(!sb.config_dir.join("mcp.toml").exists());
        assert!(!sb.config_dir.join("config.toml").exists());
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_credential_placeholders_in_url_and_argv() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    for json in [
        r#"{ "mcpServers": { "review": {
            "type": "http",
            "url": "https://broker.example.test/mcp?token=${REVIEW_TOKEN}"
        } } }"#,
        r#"{ "mcpServers": { "review": {
            "command": "review-mcp",
            "args": ["--token", "${REVIEW_TOKEN}"]
        } } }"#,
        r#"{ "mcpServers": { "review": {
            "type": "http",
            "url": "https://broker.example.test/${MCP_PATH}"
        } } }"#,
        r#"{ "mcpServers": { "review": {
            "command": "review-mcp",
            "args": ["--ordinary=${ORDINARY_VALUE}"]
        } } }"#,
    ] {
        std::fs::write(&source, json).unwrap();
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
            .stderr(predicate::str::contains("environment reference"));
        assert!(!sb.config_dir.join("mcp.toml").exists());
    }
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_invalid_transport_urls_without_a_network_grant() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    for (url, expected, secret) in [
        ("ftp://example.test/mcp", "unsupported URL scheme", None),
        (
            "https://example.test/mcp#access_token=fragment-secret-8127",
            "URL fragment",
            Some("fragment-secret-8127"),
        ),
    ] {
        std::fs::write(
            &source,
            serde_json::json!({
                "mcpServers": {
                    "review": { "type": "http", "url": url }
                }
            })
            .to_string(),
        )
        .unwrap();
        let mut command = newt(&sb);
        let assertion = command
            .args([
                "mcp",
                "import",
                source.to_str().unwrap(),
                "--name",
                "review",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected));
        if let Some(secret) = secret {
            assertion
                .stdout(predicate::str::contains(secret).not())
                .stderr(predicate::str::contains(secret).not());
        }
        assert!(!sb.config_dir.join("mcp.toml").exists());
    }
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
fn uat_codex_import_rejects_literal_headers_without_leaking_values() {
    let sb = sandbox();
    let codex_home = sb.home.join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    let config = codex_home.join("config.toml");
    for (secret, body, borrowed_key) in [
        (
            "never-copy-this-header",
            r#"
[mcp_servers.review]
url = "https://review-broker.example.test/mcp"
bearer_token_env_var = "REVIEW_TOKEN"
http_headers = { X-Literal = "never-copy-this-header" }
env_http_headers = { X-Trace = "TRACE_TOKEN" }

[mcp_servers.unrelated]
command = "unrelated-mcp"
"#,
            "review.headers.X-Literal",
        ),
        (
            "never-copy-this-env",
            r#"
[mcp_servers.review]
command = "review-mcp"
env = { ACCESS_TOKEN = "never-copy-this-env" }
"#,
            "review.env.ACCESS_TOKEN",
        ),
    ] {
        std::fs::write(&config, body).unwrap();
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
            .failure()
            .stdout(predicate::str::contains(secret).not())
            .stderr(predicate::str::contains("credential field(s)"))
            .stderr(predicate::str::contains(borrowed_key).not())
            .stderr(predicate::str::contains(secret).not());
    }

    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "rejected import must not create MCP config"
    );
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "rejected import must not grant network authority"
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
fn uat_import_rejects_transport_owned_headers_without_writing() {
    let sb = sandbox();
    let claude = sb.cwd.join("borrowed.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": { "review": {
            "type": "http",
            "url": "https://review-broker.example.test/mcp",
            "headers": { "MCP-Session-Id": "${SESSION_ID}" }
        } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            claude.to_str().unwrap(),
            "--name",
            "review",
            "--grant-net",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "unsupported or ambiguous transport semantics",
        ));

    let codex_home = sb.home.join("codex-home-owned-header");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(
        codex_home.join("config.toml"),
        r#"
[mcp_servers.review]
url = "https://review-broker.example.test/mcp"
env_http_headers = { hOsT = "HOST_OVERRIDE" }
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
        .failure()
        .stderr(predicate::str::contains(
            "unsupported or ambiguous transport semantics",
        ));

    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "rejected transport-owned headers must not create MCP config"
    );
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "rejected transport-owned headers must not grant network authority"
    );
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
fn uat_claude_import_rejects_unknown_semantics_before_trust_promotion() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": {
            "command": "review-mcp",
            "unsupported_policy": "must-not-be-erased"
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
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unsupported fields"))
        .stderr(predicate::str::contains("must-not-be-erased").not());
    assert!(!sb.config_dir.join("mcp.toml").exists());
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_nonportable_server_names_before_writing() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    for invalid in ["../outside", "review__source", "review--source"] {
        let mut servers = serde_json::Map::new();
        servers.insert(
            invalid.to_string(),
            serde_json::json!({"command": "review-mcp"}),
        );
        std::fs::write(
            &source,
            serde_json::json!({"mcpServers": servers}).to_string(),
        )
        .unwrap();

        newt(&sb)
            .args(["mcp", "import", source.to_str().unwrap(), "--name", invalid])
            .assert()
            .failure()
            .stderr(predicate::str::contains("portable single-component"));
    }
    assert!(!sb.config_dir.join("mcp.toml").exists());
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

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_effective_namespace_collisions() {
    let sb = sandbox();
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"review-source\"\ncommand = \"owned\"\n",
    )
    .unwrap();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review_source": { "command": "borrowed" } } }"#,
    )
    .unwrap();

    newt(&sb)
        .args([
            "mcp",
            "import",
            source.to_str().unwrap(),
            "--name",
            "review_source",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("effective tool namespace"));
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn uat_import_rejects_every_url_query_without_echoing_it() {
    let sb = sandbox();
    let source = sb.cwd.join("borrowed.json");
    std::fs::write(
        &source,
        r#"{ "mcpServers": { "review": {
            "type": "http", "url": "https://broker.example.test/mcp?auth=do-not-echo"
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
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("URL query"))
        .stderr(predicate::str::contains("do-not-echo").not());
    assert!(!sb.config_dir.join("mcp.toml").exists());
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

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_dedup_errors_on_clash_unless_force_or_merge() {
    let sb = sandbox();
    let claude = sb.cwd.join("c.json");
    std::fs::write(
        &claude,
        r#"{ "mcpServers": { "fs": { "command": "v2-cmd" } } }"#,
    )
    .unwrap();
    // Pre-seed an mcp.toml with a clashing `fs`.
    std::fs::write(
        sb.config_dir.join("mcp.toml"),
        "[[mcp_servers]]\nname = \"fs\"\ncommand = \"v1-cmd\"\n",
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
    assert_eq!(
        servers[0].command.as_deref(),
        Some("v1-cmd"),
        "unchanged on error"
    );

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
        .stdout(predicate::str::contains("Skipped"));
    let servers = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    assert_eq!(
        servers[0].command.as_deref(),
        Some("v1-cmd"),
        "merge kept existing"
    );

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
        .stdout(predicate::str::contains("Overwrote"));
    let servers = newt_core::mcp::parse_newt_mcp_toml(
        &std::fs::read_to_string(sb.config_dir.join("mcp.toml")).unwrap(),
    );
    assert_eq!(
        servers[0].command.as_deref(),
        Some("v2-cmd"),
        "force overwrote"
    );
}

#[test]
#[ignore = "real subprocess/filesystem acceptance; run in mcp-import-real workflow"]
#[serial_test::serial(real_fs)]
fn import_refuses_a_name_already_in_config_toml_that_outranks_mcp_toml() {
    // FIX 3 (#1301): config.toml outranks ~/.newt/mcp.toml, so importing a name
    // already in config.toml would write a silently-shadowed entry. It must be a
    // loud error (not a misleading "Imported"), and even --force must not write.
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

    for extra in [&[][..], &["--force"][..], &["--merge"][..]] {
        let mut args = vec!["mcp", "import", claude.to_str().unwrap(), "--all"];
        args.extend_from_slice(extra);
        newt(&sb)
            .args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("outranks"))
            .stderr(predicate::str::contains("scrybe"));
    }
    // Nothing was written to mcp.toml (the ineffective entry never lands).
    assert!(
        !sb.config_dir.join("mcp.toml").exists(),
        "no shadowed entry should be written"
    );
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

// ---------------------------------------------------------------------------
// Composed import -> discovery -> live MCP -> private-URL recovery UAT
// ---------------------------------------------------------------------------

mod composed_private_mcp_uat {
    use super::*;
    use std::ffi::OsString;

    use newt_core::agentic::{LeasedMcpCall, McpTools, PromptIntake};
    use newt_core::{BackendKind, ChatCtx, CompactionTriggerPolicy, MemMessage, ToolEvent};
    use newt_mcp_client::McpToolset;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const REVIEW_TOOL: &str = "review_source__get_review";
    const MCP_RESULT: &str = "authenticated review 42 loaded from imported MCP";

    /// Restore process-global discovery inputs after the acceptance scenario.
    /// The test is ignored and serialized because it intentionally grounds the
    /// environment and real-filesystem seams used by production discovery.
    struct DiscoveryEnv {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl DiscoveryEnv {
        fn install(sb: &Sandbox) -> Self {
            let values = [
                ("NEWT_CONFIG_DIR", sb.config_dir.as_os_str()),
                ("HOME", sb.home.as_os_str()),
                ("USERPROFILE", sb.home.as_os_str()),
            ];
            let saved = values
                .iter()
                .map(|(key, value)| {
                    let previous = std::env::var_os(key);
                    std::env::set_var(key, value);
                    (*key, previous)
                })
                .collect();
            Self { saved }
        }
    }

    impl Drop for DiscoveryEnv {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn last_tool_result(body: &serde_json::Value) -> Option<&str> {
        body["messages"]
            .as_array()?
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")?
            .get("content")?
            .as_str()
    }

    fn ollama_tool_call(name: &str, arguments: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }]
            }
        }))
    }

    /// Adaptive simulated inference. Any missing recovery seam deliberately
    /// reproduces the field-observed shell fallback so the final assertions
    /// distinguish a genuinely composed route from a merely connected server.
    struct PrivateReviewModel {
        review_url: String,
    }

    impl Respond for PrivateReviewModel {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            match last_tool_result(&body) {
                None => ollama_tool_call(
                    "web_fetch",
                    serde_json::json!({"url": self.review_url}),
                ),
                Some(result) if result.contains(MCP_RESULT) => ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "message": {"content": "Review evidence loaded through the imported connector."}
                    })),
                Some(result)
                    if result.contains("Authenticated-source recovery")
                        && result.contains(REVIEW_TOOL) =>
                {
                    ollama_tool_call(
                        "tool_search",
                        serde_json::json!({"query": "authenticated code review"}),
                    )
                }
                Some(result) if result.contains("Tools matching") && result.contains(REVIEW_TOOL) => {
                    ollama_tool_call(
                        REVIEW_TOOL,
                        serde_json::json!({"url": self.review_url}),
                    )
                }
                Some(_) => ollama_tool_call(
                    "run_command",
                    serde_json::json!({"command": "curl -fsSL private-review-url"}),
                ),
            }
        }
    }

    /// The production client pool behind the core loop's dependency-cycle seam.
    struct LiveImportedMcp {
        toolset: McpToolset,
    }

    #[async_trait::async_trait]
    impl McpTools for LiveImportedMcp {
        fn handles(&self, name: &str) -> bool {
            self.toolset.handles(name)
        }

        fn tool_defs(&self) -> Vec<serde_json::Value> {
            self.toolset.tool_defs()
        }

        async fn call(&mut self, leased: &LeasedMcpCall<'_>) -> String {
            self.toolset.call(leased.tool(), leased.args()).await
        }
    }

    async fn mount_review_mcp(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/reviews/42"))
            .respond_with(ResponseTemplate::new(401))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"initialize\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("Mcp-Session-Id", "composed-session")
                    .set_body_json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-03-26",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "review-source", "version": "1"}
                        }
                    })),
            )
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                "\"method\":\"notifications/initialized\"",
            ))
            .and(header("mcp-session-id", "composed-session"))
            .respond_with(ResponseTemplate::new(202))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/list\""))
            .and(header("mcp-session-id", "composed-session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "tools": [{
                        "name": "get_review",
                        "description": "Get an authenticated code review from its URL.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"url": {"type": "string"}},
                            "required": ["url"]
                        }
                    }]
                }
            })))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/call\""))
            .and(body_string_contains("\"name\":\"get_review\""))
            .and(header("mcp-session-id", "composed-session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "content": [{"type": "text", "text": MCP_RESULT}]
                }
            })))
            .mount(server)
            .await;
    }

    /// Ground the entire field regression in one process-level scenario:
    /// Claude config adoption, persisted Newt discovery, streamable-HTTP MCP
    /// handshake/list/call, and the production agent loop's private-URL
    /// `web_fetch` -> `tool_search` -> namespaced connector recovery.
    #[ignore = "real subprocess/filesystem/socket UAT; run in mcp-import-real workflow"]
    #[serial_test::serial(real_fs)]
    #[tokio::test]
    async fn uat_imported_mcp_recovers_a_private_review_without_shell_or_operator_setup() {
        let sb = sandbox();
        let mcp_server = MockServer::start().await;
        mount_review_mcp(&mcp_server).await;
        let review_url = format!("{}/reviews/42", mcp_server.uri());
        let mcp_url = format!("{}/mcp", mcp_server.uri());

        std::fs::write(
            sb.home.join(".claude.json"),
            serde_json::json!({
                "mcpServers": {
                    "review-source": {
                        "type": "http",
                        "url": mcp_url
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        newt(&sb)
            .args([
                "mcp",
                "import",
                "--from-claude",
                "--name",
                "review-source",
                "--grant-net",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Imported 1 MCP server(s)"));

        let config_path = sb.config_dir.join("config.toml");
        let runtime_config = load_config(&config_path);
        let persisted = &runtime_config.mcp_servers;
        assert_eq!(persisted.len(), 1, "one connector persisted");
        assert_eq!(persisted[0].name, "review-source");
        assert_eq!(persisted[0].url.as_deref(), Some(mcp_url.as_str()));

        let discovered = newt_core::mcp::discover(persisted, None, Some(&sb.home), &sb.cwd);
        assert_eq!(discovered.len(), 1, "persisted connector wins discovery");
        assert_eq!(discovered[0].name, "review-source");
        assert_eq!(
            discovered[0].trust,
            newt_core::mcp::McpTrust::Trusted,
            "adoption promotes only the sanitized persisted copy to Newt trust"
        );
        assert_eq!(
            runtime_config
                .tui
                .as_ref()
                .expect("grant config")
                .permissions
                .net,
            vec!["127.0.0.1"],
            "grant-net is host-scoped"
        );

        // Exercise the same resolved config inputs and permission lowering as
        // production, then execute the real initialize + tools/list exchange.
        let _discovery_env = DiscoveryEnv::install(&sb);
        let workspace = sb.cwd.to_str().unwrap();
        let caveats = runtime_config
            .tui
            .as_ref()
            .expect("grant config")
            .permissions
            .to_caveats(workspace);
        let toolset =
            McpToolset::connect(workspace, &runtime_config.mcp_servers, true, &caveats).await;
        assert_eq!(toolset.summary(), vec![("review-source".to_string(), 1)]);
        assert!(
            toolset.tool_defs()[0].get("_meta").is_none(),
            "ordinary imported MCPs need no Newt-specific resource metadata"
        );
        assert!(
            toolset.handles(REVIEW_TOOL),
            "hyphenated imported server is exposed under its canonical namespace"
        );
        let mut mcp = LiveImportedMcp { toolset };

        let model = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(PrivateReviewModel {
                review_url: review_url.clone(),
            })
            .mount(&model)
            .await;

        let task = format!("Give me a review of {review_url}");
        let intake = PromptIntake::analyze(&task);
        let messages = vec![
            MemMessage::system("Use connected authenticated sources for private URLs."),
            MemMessage::user(task.clone()),
        ];
        let persona_tools = vec![
            "web_fetch".to_string(),
            "tool_search".to_string(),
            REVIEW_TOOL.to_string(),
        ];
        let mut events: Vec<ToolEvent> = Vec::new();

        let (reply, _, _, hallucinations) = newt_core::chat_complete(
            ChatCtx {
                url: &model.uri(),
                model: "composed-private-review-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: &task,
                workspace: sb.cwd.to_str().unwrap(),
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                disclosure: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: Some(&persona_tools),
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: 6,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: intake.disposition(),
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 30,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: Some(&mut events),
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: newt_core::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                live_tool_output: None,
                completed_spill_renderer: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
            },
            &mut mcp,
        )
        .await
        .expect("private review recovers through the imported live MCP");

        let executed: Vec<&str> = events.iter().map(|event| event.tool.as_str()).collect();
        assert_eq!(
            executed,
            vec!["web_fetch", "tool_search", REVIEW_TOOL],
            "raw fetch must recover through discovery and the imported connector: {events:?}"
        );
        assert!(
            !executed.contains(&"run_command") && !executed.contains(&"request_user_input"),
            "recovery must not fall back to shell or operator setup: {events:?}"
        );
        assert_eq!(hallucinations, 0, "all three called tools are real");
        assert!(
            reply.contains("imported connector"),
            "final answer: {reply}"
        );

        let model_wire = model
            .received_requests()
            .await
            .expect("model requests recorded")
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            model_wire.contains("error: web_fetch returned HTTP 401"),
            "the raw-fetch authentication failure must remain intact: {model_wire}"
        );
        assert!(
            model_wire.contains("Authenticated-source recovery"),
            "the connected MCP route must reach the next inference round: {model_wire}"
        );

        let mcp_requests = mcp_server
            .received_requests()
            .await
            .expect("MCP requests recorded");
        let methods: Vec<String> = mcp_requests
            .iter()
            .filter_map(|request| {
                serde_json::from_slice::<serde_json::Value>(&request.body)
                    .ok()?
                    .get("method")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize".to_string(),
                "notifications/initialized".to_string(),
                "tools/list".to_string(),
                "tools/call".to_string()
            ],
            "the production client must initialize, discover, and call the imported tool"
        );
        let call = mcp_requests
            .iter()
            .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
            .find(|request| request["method"] == "tools/call")
            .expect("one live tools/call request");
        assert_eq!(call["params"]["arguments"]["url"], review_url);
    }
}
