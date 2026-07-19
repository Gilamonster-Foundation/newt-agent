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
        .stdout(predicate::str::contains("Registered MCP server"));
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
        .stdout(predicate::str::contains("newt mcp install self-probe"));
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

/// UAT tier (weekly / release): probe a real external MCP server named by
/// `NEWT_UAT_MCP_CMD` (e.g. `scrybe-mcp-server stdio`) — the
/// `live_confined_mcp.rs` pattern: no host path baked in, `#[ignore]` so the
/// per-PR run never spawns it.
#[test]
#[ignore = "UAT: needs a live MCP server via NEWT_UAT_MCP_CMD"]
fn probe_live_server_from_env() {
    let Ok(spec) = std::env::var("NEWT_UAT_MCP_CMD") else {
        eprintln!("NEWT_UAT_MCP_CMD unset; nothing to probe");
        return;
    };
    let mut parts = spec.split_whitespace();
    let command = parts.next().expect("NEWT_UAT_MCP_CMD is empty");
    let sb = sandbox();
    let mut cmd = newt(&sb);
    cmd.args(["mcp", "probe", command]);
    for arg in parts {
        cmd.args(["--arg", arg]);
    }
    cmd.arg("--yes")
        .assert()
        .success()
        .stdout(predicate::str::contains("[[mcp_servers]]"));
    let _ = Path::new(&spec); // spec is host-provided; nothing else assumed
}
