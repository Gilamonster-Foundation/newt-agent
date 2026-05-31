//! Smoke tests for the `newt-eval` binary's CLI surface.
//!
//! These tests don't spawn the worker — they just verify the
//! command-line plumbing (subcommands, flags, exit codes) is wired up.
//! The real end-to-end test is `mock_e2e.rs`.

use assert_cmd::Command;
use predicates::str::contains;

/// `list-cases` enumerates the bundled cases.
#[test]
fn list_cases_prints_bundled_cases() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .arg("list-cases")
        .assert()
        .success()
        .stdout(contains("001-rename-function"))
        .stdout(contains("005-extract-constant"));
}

/// `run --mode mock` is explicitly directed at the test harness — the
/// binary returns a clear error rather than silently doing nothing.
#[test]
fn run_mock_mode_directs_user_to_test_harness() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args(["run", "--mode", "mock"])
        .assert()
        .failure()
        .stderr(contains("cargo test"));
}

/// #29: the worker timeout is exposed as a flag + env var.
#[test]
fn run_help_shows_worker_timeout_flag() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(contains("--worker-timeout-ms"))
        .stdout(contains("NEWT_EVAL_WORKER_TIMEOUT_MS"));
}

/// #29: the flag parses and is accepted alongside the other run args
/// (clap would exit 2 with an "unexpected argument" error otherwise).
#[test]
fn run_accepts_worker_timeout_flag() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args(["run", "--mode", "mock", "--worker-timeout-ms", "180000"])
        .assert()
        .failure()
        .stderr(contains("cargo test"));
}

/// Without a worker binary, live mode fails fast with a helpful message.
#[test]
fn run_live_mode_reports_missing_worker() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args([
            "run",
            "--mode",
            "live",
            "--case",
            "001",
            "--worker-bin",
            "/definitely/not/here/newt",
        ])
        .assert()
        // Exit code 2 = "ran cleanly but at least one case failed", which
        // is what we get when the runner errors on each case.
        .code(2);
}
