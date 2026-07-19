//! Process-level coverage for `newt mcp probe`.
//!
//! The stdio target is the `newt` binary itself serving MCP (`newt mcp`) —
//! the same self-probe trick `doctor_cli.rs` uses — so the whole confined
//! spawn → initialize → tools/list → derive pipeline runs for real, with no
//! external server. `NEWT_CONFIG_DIR`, `HOME`, and the cwd are sandboxed to
//! tempdirs (the doctor_cli isolation pattern).

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

fn newt(sb: &Sandbox) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.env("NEWT_CONFIG_DIR", &sb.config_dir)
        .env("HOME", &sb.home)
        .env_remove("NEWT_CONFIG")
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        .current_dir(&sb.cwd);
    cmd
}

/// The newt binary path — probed as a stdio MCP server via `--arg mcp`.
fn newt_bin() -> String {
    assert_cmd::cargo::cargo_bin("newt").display().to_string()
}

#[test]
fn probe_derives_identity_tools_and_posture_from_a_live_server() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "probe", &newt_bin(), "--arg", "mcp", "--yes"])
        .assert()
        .success()
        // Name derived from the server's own serverInfo, not the binary path.
        .stdout(predicate::str::contains("newt-mcp-server"))
        .stdout(predicate::str::contains("[[mcp_servers]]"))
        .stdout(predicate::str::contains("tool(s)"))
        .stdout(predicate::str::contains("confinement"))
        .stdout(predicate::str::contains("net egress"));
}

#[test]
fn probe_save_writes_the_config_and_duplicate_suggests_a_rename() {
    let sb = sandbox();
    newt(&sb)
        .args([
            "mcp",
            "probe",
            &newt_bin(),
            "--arg",
            "mcp",
            "--save",
            "--yes",
        ])
        .assert()
        .success()
        // Status lines live on stderr; stdout is report-only.
        .stderr(predicate::str::contains("Registered MCP server"));
    let cfg = newt_core::Config::load(&sb.config_dir.join("config.toml")).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(cfg.mcp_servers[0].name, "newt-mcp-server");
    assert_eq!(cfg.mcp_servers[0].args, vec!["mcp"]);

    // The registered entry shows up in the management view.
    newt(&sb)
        .args(["mcp", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("newt-mcp-server"));

    // Saving the same derived name again errors, pointing at --name.
    newt(&sb)
        .args([
            "mcp",
            "probe",
            &newt_bin(),
            "--arg",
            "mcp",
            "--save",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"))
        .stderr(predicate::str::contains("--name"));
}

#[test]
fn probe_to_catalog_then_install_round_trips() {
    let sb = sandbox();
    newt(&sb)
        .args([
            "mcp",
            "probe",
            &newt_bin(),
            "--arg",
            "mcp",
            "--name",
            "self-probe",
            "--to-catalog",
            "--yes",
        ])
        .assert()
        .success()
        // Status lines live on stderr; stdout is report-only.
        .stderr(predicate::str::contains("newt mcp install self-probe"));
    let catalog_text = std::fs::read_to_string(sb.config_dir.join("mcp-catalog.toml")).unwrap();
    assert!(
        catalog_text.contains("name = \"self-probe\""),
        "{catalog_text}"
    );

    // The probed entry is now installable like any curated one.
    newt(&sb)
        .args(["mcp", "install", "self-probe"])
        .assert()
        .success();
    let cfg = newt_core::Config::load(&sb.config_dir.join("config.toml")).unwrap();
    assert_eq!(cfg.mcp_servers[0].name, "self-probe");
    assert_eq!(cfg.mcp_servers[0].args, vec!["mcp"]);
}

#[test]
fn probe_without_yes_fails_closed_off_a_terminal() {
    let sb = sandbox();
    // Piped stdin is not a TTY: executing a candidate needs the consent
    // gesture, so the probe must refuse rather than silently spawn.
    newt(&sb)
        .args(["mcp", "probe", &newt_bin(), "--arg", "mcp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--yes"));
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "nothing may be written"
    );
}

#[test]
fn probe_never_certifies_a_stdin_echoing_process() {
    let sb = sandbox();
    // `/bin/cat` echoes the initialize request back verbatim (matching id, no
    // error) — before handshake validation this "probed OK" with zero tools
    // and was saveable. It must be rejected as not-an-MCP-server.
    newt(&sb)
        .args(["mcp", "probe", "/bin/cat", "--save", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not an MCP server"));
    assert!(
        !sb.config_dir.join("config.toml").exists(),
        "a non-server must never be registered"
    );
}

#[test]
fn probe_failure_reports_every_candidate_tried() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "probe", "/usr/bin/true", "--arg", "nope", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no candidate spoke MCP"))
        .stderr(predicate::str::contains("/usr/bin/true nope"))
        .stderr(predicate::str::contains("--arg"));
}

#[test]
fn probe_json_emits_a_machine_readable_report() {
    let sb = sandbox();
    let assert = newt(&sb)
        .args([
            "mcp",
            "probe",
            &newt_bin(),
            "--arg",
            "mcp",
            "--json",
            "--yes",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout not JSON ({e}):\n{stdout}"));
    assert_eq!(v["name"], "newt-mcp-server");
    assert_eq!(v["transport"], "stdio");
    assert_eq!(v["args"][0], "mcp");
    assert!(
        !v["tools"].as_array().unwrap().is_empty(),
        "tools listed: {v}"
    );
    assert!(v["toml"].as_str().unwrap().contains("[[mcp_servers]]"));
}

#[tokio::test]
async fn probe_url_succeeds_against_a_streamable_http_server() {
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains(r#""method":"initialize""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "serverInfo": { "name": "wire-srv", "version": "9.9" },
                "instructions": "A wired test server."
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("notifications/initialized"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("tools/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 2,
            "result": { "tools": [ { "name": "ping", "description": "", "inputSchema": {} } ] }
        })))
        .mount(&server)
        .await;

    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "probe", &server.uri(), "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wire-srv"))
        .stdout(predicate::str::contains("A wired test server."))
        .stdout(predicate::str::contains("ping"))
        .stdout(predicate::str::contains("type = \"http\""));
}

#[tokio::test]
async fn probe_url_reports_auth_required_on_401() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("token required"))
        .mount(&server)
        .await;

    let sb = sandbox();
    // Reachable-but-locked is a FINDING, not a failure: derive the entry,
    // point at `newt auth`, exit 0.
    newt(&sb)
        .args(["mcp", "probe", &server.uri(), "--name", "locked", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("authentication required"))
        .stdout(predicate::str::contains("newt auth locked"));
}

#[test]
fn probe_json_with_save_keeps_stdout_a_single_json_value() {
    let sb = sandbox();
    let assert = newt(&sb)
        .args([
            "mcp",
            "probe",
            &newt_bin(),
            "--arg",
            "mcp",
            "--json",
            "--save",
            "--to-catalog",
            "--yes",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // Status lines ("Registered…", next steps, "Cataloged…") belong on
    // stderr — stdout must stay exactly one machine-parseable JSON value.
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout not a single JSON value ({e}):\n{stdout}"));
    assert_eq!(v["name"], "newt-mcp-server");
    assert!(sb.config_dir.join("config.toml").exists());
    assert!(sb.config_dir.join("mcp-catalog.toml").exists());
}

#[test]
fn unreadable_probe_rules_drop_in_fails_loudly() {
    let sb = sandbox();
    // Present-but-unreadable (one invalid-UTF-8 byte): silently skipping it
    // would probe with the WRONG rules — the #1291 read-safety contract.
    std::fs::write(sb.config_dir.join("mcp-probe-rules.toml"), b"caf\xE9").unwrap();
    newt(&sb)
        .args(["mcp", "probe", "/usr/bin/true", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mcp-probe-rules.toml"));
}

#[test]
fn malformed_probe_rules_drop_in_fails_loudly() {
    let sb = sandbox();
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    std::fs::write(
        sb.cwd.join(".newt").join("mcp-probe-rules.toml"),
        "not toml [",
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "probe", "/usr/bin/true", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mcp-probe-rules.toml"));
}

#[test]
fn unreadable_catalog_drop_in_fails_install_loudly() {
    let sb = sandbox();
    std::fs::write(sb.config_dir.join("mcp-catalog.toml"), b"caf\xE9").unwrap();
    // Silently skipping the unreadable overlay would install from a catalog
    // the operator believes they have overridden.
    newt(&sb)
        .args(["mcp", "install", "scrybe"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("mcp-catalog.toml"));
    assert!(!sb.config_dir.join("config.toml").exists());
}

#[test]
fn probe_rules_project_drop_in_beats_the_user_one() {
    let sb = sandbox();
    // User rules would never speak MCP; the project rules pin `mcp`. Success
    // proves the project file won the whole-file replacement.
    std::fs::write(
        sb.config_dir.join("mcp-probe-rules.toml"),
        "arg_candidates = [[\"definitely-wrong\"]]\n",
    )
    .unwrap();
    std::fs::create_dir_all(sb.cwd.join(".newt")).unwrap();
    std::fs::write(
        sb.cwd.join(".newt").join("mcp-probe-rules.toml"),
        "arg_candidates = [[\"mcp\"]]\n",
    )
    .unwrap();
    newt(&sb)
        .args(["mcp", "probe", &newt_bin(), "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("newt-mcp-server"));
}

#[test]
fn probe_refuses_non_loopback_plain_http_without_consent() {
    let sb = sandbox();
    newt(&sb)
        .args(["mcp", "probe", "http://mcp.example/x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--allow-http"));
}

#[test]
fn probe_of_an_unreachable_loopback_url_fails_with_the_dial_error() {
    let sb = sandbox();
    // Loopback http needs no consent flag; the dial itself just fails fast.
    newt(&sb)
        .args(["mcp", "probe", "http://127.0.0.1:1/mcp", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("127.0.0.1"));
}

/// UAT tier (weekly / release): probe a real external MCP server — the
/// `live_confined_mcp.rs` env convention: `NEWT_UAT_MCP_CMD` is the command
/// ONLY (a path may contain spaces), `NEWT_UAT_MCP_ARGS` is the space-split
/// argument list. No host path baked in; `#[ignore]` keeps the per-PR run
/// from spawning it.
#[test]
#[ignore = "UAT: needs a live MCP server via NEWT_UAT_MCP_CMD (+ NEWT_UAT_MCP_ARGS)"]
fn probe_live_server_from_env() {
    let Ok(command) = std::env::var("NEWT_UAT_MCP_CMD") else {
        eprintln!("NEWT_UAT_MCP_CMD unset; nothing to probe");
        return;
    };
    assert!(
        Path::new(&command).exists() || !command.contains('/'),
        "NEWT_UAT_MCP_CMD names a missing path: {command}"
    );
    let args = std::env::var("NEWT_UAT_MCP_ARGS").unwrap_or_default();
    let sb = sandbox();
    let mut cmd = newt(&sb);
    cmd.args(["mcp", "probe", &command]);
    for arg in args.split_whitespace() {
        cmd.args(["--arg", arg]);
    }
    cmd.arg("--yes")
        .assert()
        .success()
        .stdout(predicate::str::contains("[[mcp_servers]]"));
}
