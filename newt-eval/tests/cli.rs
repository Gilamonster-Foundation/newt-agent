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
///
/// Issue #41: a missing-worker run now produces `evaluator == "runner"`
/// FAIL rows in the scorecard, which exits 1 (not 2). The scorecard
/// itself doesn't get rendered because the resolver errors out before
/// any case runs once `--worker-bin` is given a path that doesn't
/// exist. We use `--legacy-exit-codes` together with a path *that*
/// `resolve_worker_bin` can resolve to exercise the runner-FAIL branch
/// in `run_live_mode_runner_failure_exits_one` below.
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
        // The resolver short-circuits before any case runs and the
        // process bails with anyhow → exit code 1 (FAILURE).
        .failure()
        .stderr(contains("not found"))
        .stderr(contains("/definitely/not/here/newt"));
}

/// #40: the help text mentions the new resolution order so an operator
/// who hits "binary not found" can see what to try.
#[test]
fn run_help_documents_worker_bin_resolution() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(contains("NEWT_WORKER_BIN"))
        .stdout(contains("--worker-bin"));
}

/// #41: the `--legacy-exit-codes` flag exists and is documented.
#[test]
fn run_help_documents_legacy_exit_codes_flag() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(contains("--legacy-exit-codes"));
}

/// #41: when the worker binary resolves but every case ends in a
/// runner FAIL (we point at a path that does exist but isn't a real
/// `newt` worker), the process exits 1.
#[test]
fn run_live_mode_runner_failure_exits_one() {
    // Use `/usr/bin/true` as a stand-in: it exists on both Linux and
    // macOS (unlike `/bin/true`, which is Linux-only), so the resolver
    // is happy, but it isn't an ACP-speaking worker — the runner will
    // record a "runner" FAIL row for every case it tries.
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args([
            "run",
            "--mode",
            "live",
            "--case",
            "001",
            "--worker-bin",
            "/usr/bin/true",
        ])
        .assert()
        // Issue #41: exit 1 on any runner FAIL.
        .code(1);
}

/// #41: with `--legacy-exit-codes`, the same situation reverts to the
/// pre-#41 exit code (2).
#[test]
fn run_live_mode_runner_failure_legacy_exit_code() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args([
            "run",
            "--mode",
            "live",
            "--case",
            "001",
            "--worker-bin",
            "/usr/bin/true",
            "--legacy-exit-codes",
        ])
        .assert()
        // Pre-#41 behavior: exit 2.
        .code(2);
}

/// `grade --help` documents the case + workspace flags.
#[test]
fn grade_help_shows_case_and_workspace_flags() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args(["grade", "--help"])
        .assert()
        .success()
        .stdout(contains("--case"))
        .stdout(contains("--workspace"));
}

/// `grade` against an unchanged fixture reconstructs an empty diff, so the
/// structural evaluators fail and the process exits 2 (the case-failed code).
#[test]
fn grade_unchanged_fixture_fails_and_exits_two() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args([
            "grade",
            "--case",
            "007-add-struct-method",
            "--workspace",
            "cases/007-add-struct-method/workspace",
        ])
        .assert()
        .code(2)
        .stdout(contains("diff_nonempty"));
}

/// `grade` reports a clear error when the named case does not exist.
#[test]
fn grade_unknown_case_errors() {
    Command::cargo_bin("newt-eval")
        .unwrap()
        .args(["grade", "--case", "no-such-case", "--workspace", "cases"])
        .assert()
        .failure()
        .stderr(contains("not found"));
}
