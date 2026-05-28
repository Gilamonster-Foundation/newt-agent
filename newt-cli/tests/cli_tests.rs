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
