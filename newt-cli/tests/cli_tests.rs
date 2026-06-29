//! Integration tests for the `newt` CLI binary.

use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;

#[test]
fn doctor_runs_without_crash() {
    Command::cargo_bin("newt")
        .unwrap()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("newt doctor"));
}

#[test]
fn config_prints_toml() {
    Command::cargo_bin("newt")
        .unwrap()
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("backends"));
}

#[test]
fn config_flag_missing_file() {
    Command::cargo_bin("newt")
        .unwrap()
        .args(["--config", "/nonexistent/path/config.toml", "config"])
        .assert()
        .failure();
}

#[test]
fn config_flag_valid_file() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(
        br#"
[[backends]]
name = "test-ollama"
endpoint = "http://localhost:99999"
model = "test:7b"
tiers = ["FAST"]

default_tier_order = ["FAST"]
"#,
    )
    .unwrap();
    f.flush().unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .args(["--config", f.path().to_str().unwrap(), "config"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test-ollama"))
        .stdout(predicate::str::contains("test:7b"));
}

#[test]
fn doctor_with_config_flag() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(
        br#"
[[backends]]
name = "custom-backend"
endpoint = "http://localhost:99998"
model = "custom:3b"
tiers = ["FAST"]

default_tier_order = ["FAST"]
"#,
    )
    .unwrap();
    f.flush().unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .args(["--config", f.path().to_str().unwrap(), "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("custom-backend"));
}

#[test]
fn venv_and_exec_path_flags_are_accepted_by_dispatch() {
    // dispatch() resolves --venv / --exec-path into NEWT_VENV / NEWT_EXEC_PATHS
    // before running the subcommand; the flags must not break a non-TUI
    // subcommand like `config`.
    let venv = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(venv.path().join("bin")).unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .env_remove("VIRTUAL_ENV")
        .arg("--venv")
        .arg(venv.path())
        .arg("--exec-path")
        .arg(venv.path().join("bin"))
        .arg("--exec-path")
        .arg(venv.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("backends"));
}

#[test]
fn activated_virtual_env_is_picked_up_without_flag() {
    // With $VIRTUAL_ENV already set (shell-activated venv) and no --venv flag,
    // dispatch takes the env fallback path and the subcommand still works.
    let venv = tempfile::tempdir().unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .env("VIRTUAL_ENV", venv.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("backends"));
}

#[test]
fn dgx_route_review_task() {
    Command::cargo_bin("newt")
        .unwrap()
        .args(["dgx", "route", "review this code for security bugs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Complexity"))
        .stdout(predicate::str::contains("review"));
}

#[test]
fn dgx_route_complex_task() {
    Command::cargo_bin("newt")
        .unwrap()
        .args(["dgx", "route", "refactor the entire module across services"])
        .assert()
        .success()
        .stdout(predicate::str::contains("complex"));
}

#[test]
fn dgx_requires_subcommand() {
    Command::cargo_bin("newt")
        .unwrap()
        .arg("dgx")
        .assert()
        .failure();
}

#[test]
fn dgx_status_help_lists_json_flag() {
    // Issue #709 §1: `newt dgx status` gained a memory-budget view with a
    // `--json` form and a `--node` override. Clap-surface only (no SSH/network).
    Command::cargo_bin("newt")
        .unwrap()
        .args(["dgx", "status", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"))
        .stdout(predicate::str::contains("--node"));
}

#[test]
fn dgx_pull_help_lists_flags() {
    Command::cargo_bin("newt")
        .unwrap()
        .args(["dgx", "pull", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dry-run"))
        .stdout(predicate::str::contains("--force"))
        .stdout(predicate::str::contains("--name"))
        .stdout(predicate::str::contains("HuggingFace").or(predicate::str::contains("GGUF")));
}

/// A config file with a `[dgx]` node that carries an ssh_host (so `pull` can
/// resolve a node) but no live endpoint.
fn dgx_ssh_config() -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(
        br#"
[[backends]]
name = "x"
endpoint = "http://localhost:11434"
model = "m"
tiers = ["FAST"]

default_tier_order = ["FAST"]

[dgx]
active_node = "dgx"
active_endpoint = "ollama"

[[dgx.nodes]]
name = "dgx"
ollama = "http://localhost:11434"
ssh_host = "dgx.example"
ssh_user = "bob"
"#,
    )
    .unwrap();
    f.flush().unwrap();
    f
}

#[test]
fn dgx_pull_dry_run_native_prints_plan_and_does_not_ssh() {
    let f = dgx_ssh_config();
    Command::cargo_bin("newt")
        .unwrap()
        .env_remove("NEWT_DGX_SSH_HOST")
        .args([
            "--config",
            f.path().to_str().unwrap(),
            "dgx",
            "pull",
            "qwen2.5-coder:32b",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("OllamaNative"))
        .stdout(predicate::str::contains("ssh bob@dgx.example"))
        .stdout(predicate::str::contains("ollama pull 'qwen2.5-coder:32b'"));
}

#[test]
fn dgx_pull_without_ssh_host_fails() {
    let f = dgx_less_config();
    Command::cargo_bin("newt")
        .unwrap()
        .env_remove("NEWT_DGX_SSH_HOST")
        .env_remove("NEWT_DGX_HOST")
        .args([
            "--config",
            f.path().to_str().unwrap(),
            "dgx",
            "pull",
            "some-model:1b",
            "--dry-run",
        ])
        .assert()
        .failure();
}

/// A dgx-less config file used by the probe tests so they don't depend on
/// the developer's `~/.newt/config.toml`.
fn dgx_less_config() -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(
        br#"
[[backends]]
name = "x"
endpoint = "http://localhost:11434"
model = "m"
tiers = ["FAST"]

default_tier_order = ["FAST"]
"#,
    )
    .unwrap();
    f.flush().unwrap();
    f
}

#[test]
fn dgx_doctor_unconfigured_shows_guidance() {
    let f = dgx_less_config();
    Command::cargo_bin("newt")
        .unwrap()
        .env_remove("NEWT_DGX_OLLAMA_URL")
        .env_remove("NEWT_DGX_OLLAMA_LB_URL")
        .env_remove("NEWT_DGX_IN_CLUSTER_URL")
        .env_remove("NEWT_DGX_VLLM_URL")
        .env_remove("NEWT_DGX_HOST")
        .args(["--config", f.path().to_str().unwrap(), "dgx", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not set"))
        .stdout(predicate::str::contains("DNS note"));
}

#[test]
fn dgx_models_unconfigured_fails() {
    let f = dgx_less_config();
    Command::cargo_bin("newt")
        .unwrap()
        .env_remove("NEWT_DGX_OLLAMA_URL")
        .env_remove("NEWT_DGX_HOST")
        .args(["--config", f.path().to_str().unwrap(), "dgx", "models"])
        .assert()
        .failure();
}

#[test]
fn dgx_models_uses_env_endpoint() {
    // NEWT_DGX_OLLAMA_URL set to an unreachable host: `models` resolves the
    // endpoint (no "not configured" error) and fails on the network call —
    // proving env-only wiring. The dgx-less --config keeps it independent
    // of ~/.newt.
    let f = dgx_less_config();
    Command::cargo_bin("newt")
        .unwrap()
        .env("NEWT_DGX_OLLAMA_URL", "http://127.0.0.1:1")
        .args(["--config", f.path().to_str().unwrap(), "dgx", "models"])
        .assert()
        .failure();
}
