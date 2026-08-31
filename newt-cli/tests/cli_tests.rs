//! Integration tests for the `newt` CLI binary.

use predicates::prelude::*;
use std::io::Write;

// #1852: `assert_cmd::Command` is deliberately NOT imported. Every `newt` in
// this file is built by `common::newt()`, which pins the config-discovery
// axes. Reaching for the raw `cargo_bin` constructor here does not compile
// until someone re-adds that import, and re-adding it trips
// `config_isolation::newt_is_only_constructed_through_the_isolation_helper`.
mod common;

/// Ground-truth build provenance: the real binary must report the package
/// version and the commit checked out in the real repository. This guards the
/// shared build-info value against drifting back to Cargo's version alone.
#[test]
fn version_reports_package_and_source_commit() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let git = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(git.status.success(), "git rev-parse failed: {git:?}");
    let commit = String::from_utf8(git.stdout).unwrap();
    let commit = commit.trim();

    assert!(
        newt_core::build_info::VERSION_WITH_COMMIT.contains(commit),
        "build version {:?} does not identify checked-out commit {commit}",
        newt_core::build_info::VERSION_WITH_COMMIT
    );
    common::newt()
        .arg("--version")
        .assert()
        .success()
        .stdout(format!(
            "newt {}\n",
            newt_core::build_info::VERSION_WITH_COMMIT
        ));
}

#[test]
fn binary_help_surfaces_start_on_the_guarded_cli_stack() {
    // Regression for PR #746 / issue #747: on Windows, the expanded clap tree
    // overflowed the default ~1 MiB process main-thread stack before any
    // stdout/stderr. Exercise the real binary entrypoint, not `Cli::try_parse`,
    // so removing the 16 MiB wrapper fails as a process startup regression.
    for args in [
        &["--help"][..],
        &["dgx", "--help"][..],
        &["plan", "--help"][..],
    ] {
        common::newt()
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("newt"));
    }

    common::newt()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config-dir"));
}

/// Regression for #1513 (the #1491 re-scope): the `newt bench` subcommand was
/// removed — matrix orchestration lives in the `scripts/eval/` tooling, not the
/// CLI surface. `bench` must parse as an unrecognized subcommand and must not
/// appear in the top-level help. Fully mocked / no backend — PR tier.
#[test]
fn bench_subcommand_is_removed() {
    common::newt()
        .args(["bench"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));

    // The subcommand catalog lists each command name on its own indented line;
    // no such line may be `bench`. (The word "benchmark" legitimately appears
    // in `solve`'s description — the lane the eval tooling drives.)
    common::newt()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"(?m)^\s+bench\b").unwrap().not());
}

/// `newt help` renders the interactive command catalog WITHOUT starting a
/// session or connecting to a backend. This is the property that lets
/// `scripts/eval/grade-548.sh` run on a hosted CI runner (no live Ollama): the
/// binary is spawned with an unreachable backend and no config, yet must still
/// print help and exit 0. Fully mocked / hostile-env — PR tier.
#[test]
fn help_subcommand_renders_without_a_backend() {
    common::newt()
        .env("OLLAMA_HOST", "http://127.0.0.1:1") // unreachable on purpose
        .env("NO_COLOR", "1")
        .args(["help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Available commands:"))
        .stdout(predicate::str::contains("/dgx status"));

    common::newt()
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        .env("NO_COLOR", "1")
        .args(["help", "dgx"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/dgx help"));
}

/// ANTI-DRIFT (process tier): the `newt help` BINARY must emit bytes IDENTICAL
/// to the stable plain library renderer `newt_tui::render_help`. Pairs with the
/// newt-tui unit test `render_help_is_byte_identical_to_the_plain_help_corpus`,
/// which pins the startup-free CLI and interactive Markdown-off fallback to
/// the same help_lines / command_help_page corpora. `NO_COLOR` pins
/// `color_supported()` to `false` in both the binary and this expectation.
#[test]
fn help_subcommand_is_byte_identical_to_the_plain_renderer() {
    let cases: [(&[&str], Option<&str>); 3] = [
        (&["help"], None),
        (&["help", "dgx"], Some("dgx")),
        (&["help", "/exit"], Some("exit")), // leading slash tolerated
    ];
    for (args, topic) in cases {
        let out = common::newt()
            .env("OLLAMA_HOST", "http://127.0.0.1:1")
            .env("NO_COLOR", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "`newt {args:?}` exited non-zero");
        let expected = newt_tui::render_help(topic, false, false);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            expected,
            "`newt {args:?}` stdout drifted from newt_tui::render_help"
        );
    }
}

#[test]
fn doctor_runs_without_crash() {
    common::newt()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("newt doctor"));
}

/// #1951 regression: an ambiguous backend drop-in — the exact old newt-adopt
/// probe marker plus operator-looking evidence (`model`), which
/// `Config::resolve()` refuses to attribute — used to make `Config::resolve()?`
/// end `newt doctor` with that same failure before a single line of diagnosis
/// printed: the tool died with the exact error it exists to diagnose. Would
/// have failed (exit 1, no "Backend drop-ins:" section at all) before the
/// fix; now doctor reports the finding and keeps running everything that
/// does not need a resolved config.
#[test]
fn doctor_reports_unattributable_dropin_instead_of_dying() {
    let newt = common::newt();
    let dropin_dir = newt.config_dir().join("backends");
    std::fs::create_dir_all(&dropin_dir).unwrap();
    let dropin = dropin_dir.join("api-moonshot-ai-443.toml");
    std::fs::write(
        &dropin,
        "endpoint = \"https://api.moonshot.ai/v1\"\n\
         model = \"kimi-k2\"\n\
         [provenance]\n\
         source = \"newt adopt v0.7.3 (probed; delete this file to reset)\"\n",
    )
    .unwrap();

    let mut cmd = newt;
    cmd.arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("FINDING"))
        .stdout(predicate::str::contains("Backend drop-ins:"))
        .stdout(predicate::str::contains(dropin.display().to_string()))
        .stdout(predicate::str::contains("cannot be attributed"));
}

/// Anti-vacuous twin of [`doctor_reports_unattributable_dropin_instead_of_dying`]:
/// against a clean fixture (no drop-in at all) doctor reports no finding —
/// proving the assertion above is about THIS file, not something every
/// `doctor` run prints regardless of input.
#[test]
fn doctor_reports_no_finding_for_a_clean_dropin_directory() {
    common::newt()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("Backend drop-ins:"))
        .stdout(predicate::str::contains("(none)"))
        .stdout(predicate::str::contains("FINDING").not());
}

/// #1951 C0b regression: `--fix` on a piped/headless run (every `assert_cmd`
/// spawn — no pty) must REFUSE to guess which of the two remediations
/// applies, not silently pick one. The two repairs have opposite
/// consequences (keep the backend as configuration vs. discard it as
/// residue), so a non-interactive guess would be worse than the original
/// failure. The file must be byte-for-byte untouched.
#[test]
fn doctor_fix_refuses_to_guess_without_a_terminal() {
    let newt = common::newt();
    let dropin_dir = newt.config_dir().join("backends");
    std::fs::create_dir_all(&dropin_dir).unwrap();
    let dropin = dropin_dir.join("api-moonshot-ai-443.toml");
    let original = "endpoint = \"https://api.moonshot.ai/v1\"\n\
         model = \"kimi-k2\"\n\
         [provenance]\n\
         source = \"newt adopt v0.7.3 (probed; delete this file to reset)\"\n";
    std::fs::write(&dropin, original).unwrap();

    let mut cmd = newt;
    cmd.args(["doctor", "--fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains("refusing to guess"));

    assert_eq!(
        std::fs::read_to_string(&dropin).unwrap(),
        original,
        "a piped `--fix` must not modify the ambiguous drop-in — the choice needs a human"
    );
}

#[test]
fn config_prints_toml() {
    common::newt()
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("backends"));
}

#[test]
fn config_flag_missing_file() {
    common::newt()
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

    common::newt()
        .args(["--config", f.path().to_str().unwrap(), "config"])
        .assert()
        .success()
        .stdout(predicate::str::contains("test-ollama"))
        .stdout(predicate::str::contains("test:7b"));
}

#[test]
fn config_dir_flag_reads_config_from_isolated_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        r#"
[[backends]]
name = "config-dir-backend"
endpoint = "http://localhost:99997"
model = "config-dir:1b"
tiers = ["FAST"]

default_tier_order = ["FAST"]
"#,
    )
    .unwrap();

    common::newt()
        .env_remove("NEWT_CONFIG")
        .env_remove("NEWT_CONFIG_DIR")
        .arg("--config-dir")
        .arg(dir.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("config-dir-backend"))
        .stdout(predicate::str::contains("config-dir:1b"));
}

#[test]
fn config_file_flag_overrides_config_dir_for_main_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        r#"
[[backends]]
name = "config-dir-backend"
endpoint = "http://localhost:99997"
model = "config-dir:1b"
tiers = ["FAST"]

default_tier_order = ["FAST"]
"#,
    )
    .unwrap();

    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(
        br#"
[[backends]]
name = "explicit-config-backend"
endpoint = "http://localhost:99996"
model = "explicit:1b"
tiers = ["FAST"]

default_tier_order = ["FAST"]
"#,
    )
    .unwrap();
    f.flush().unwrap();

    common::newt()
        .env_remove("NEWT_CONFIG")
        .env_remove("NEWT_CONFIG_DIR")
        .arg("--config-dir")
        .arg(dir.path())
        .arg("--config")
        .arg(f.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("explicit-config-backend"))
        .stdout(predicate::str::contains("explicit:1b"))
        .stdout(predicate::str::contains("config-dir-backend").not());
}

#[test]
fn config_dir_flag_reads_user_dropins_from_isolated_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("backends")).unwrap();
    std::fs::write(
        dir.path().join("backends").join("dropin.toml"),
        r#"
endpoint = "http://localhost:99995"
model = "dropin:1b"
tiers = ["FAST"]
"#,
    )
    .unwrap();

    common::newt()
        .env_remove("NEWT_CONFIG")
        .env_remove("NEWT_CONFIG_DIR")
        .arg("--config-dir")
        .arg(dir.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("dropin"))
        .stdout(predicate::str::contains("dropin:1b"));
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

    common::newt()
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

    common::newt()
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

    common::newt()
        .env("VIRTUAL_ENV", venv.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("backends"));
}

#[test]
fn dgx_route_review_task() {
    common::newt()
        .args(["dgx", "route", "review this code for security bugs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Complexity"))
        .stdout(predicate::str::contains("review"));
}

#[test]
fn dgx_route_complex_task() {
    common::newt()
        .args(["dgx", "route", "refactor the entire module across services"])
        .assert()
        .success()
        .stdout(predicate::str::contains("complex"));
}

#[test]
fn dgx_requires_subcommand() {
    common::newt().arg("dgx").assert().failure();
}

#[test]
fn dgx_status_help_lists_json_flag() {
    // Issue #709 §1: `newt dgx status` gained a memory-budget view with a
    // `--json` form and a `--node` override. Clap-surface only (no SSH/network).
    common::newt()
        .args(["dgx", "status", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"))
        .stdout(predicate::str::contains("--node"));
}

#[test]
fn dgx_pull_help_lists_flags() {
    common::newt()
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
    common::newt()
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
    common::newt()
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
    common::newt()
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
    common::newt()
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
    common::newt()
        .env("NEWT_DGX_OLLAMA_URL", "http://127.0.0.1:1")
        .args(["--config", f.path().to_str().unwrap(), "dgx", "models"])
        .assert()
        .failure();
}
