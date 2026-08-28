//! **#1852 regression: tests must not read the developer's `~/.newt`.**
//!
//! Ten tests across three files ran the real `newt` binary against ambient
//! config, so they passed in CI (empty `$HOME`) and failed on a developer box
//! whose `~/.newt/backends/` held a drop-in the current build refuses. Red
//! before this slice, on the real machine:
//!
//! ```text
//! cli_tests.rs:155   doctor_runs_without_crash
//! cli_tests.rs:165   config_prints_toml
//! cli_tests.rs:353   venv_and_exec_path_flags_are_accepted_by_dispatch
//! cli_tests.rs:368   activated_virtual_env_is_picked_up_without_flag
//! cli_tests.rs:378   dgx_route_review_task
//! cli_tests.rs:389   dgx_route_complex_task
//! worker_cli.rs:76   worker_generates_key_and_answers_initialize
//! worker_cli.rs:136  worker_ignores_invalid_metrics_port
//! worker_cli.rs:230  worker_metrics_server_serves_healthz_and_metrics
//! stdout_purity.rs:147 worker_stdout_is_pure_json_rpc
//! ```
//!
//! # These assertions never touch the real environment
//!
//! They inspect the **built command** — `get_envs()`, `get_current_dir()` —
//! rather than exporting a variable and watching what happens. That is
//! deliberate: a test that sets a process-global to observe an effect is the
//! #1850 defect wearing a different hat, and one that passes because a sibling
//! happens to hold some state is vacuous. Nothing here mutates anything
//! process-wide, so nothing here can race or be masked.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

mod common;

/// The built command's env deltas: `Some(v)` for a pin, `None` for a scrub.
fn env_deltas(cmd: &assert_cmd::Command) -> HashMap<OsString, Option<OsString>> {
    cmd.get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
        .collect()
}

/// Every variable in the family is explicitly REMOVED from the child.
///
/// Not "the ones that broke": pinning `NEWT_CONFIG_DIR` while leaving
/// `NEWT_PROVIDER` exported does not isolate a test, it changes which piece of
/// the operator's session leaks in.
#[test]
fn the_whole_ambient_config_family_is_scrubbed() {
    let cmd = common::newt();
    let deltas = env_deltas(&cmd);
    for key in common::AMBIENT_CONFIG_ENV {
        let entry = deltas.get(OsStr::new(key));
        assert!(
            matches!(entry, Some(None)),
            "{key} is not scrubbed from the isolated command (got {entry:?}); \
             a half-scrubbed family is not isolation"
        );
    }
}

/// All THREE config-discovery axes are pinned, not just the obvious two.
///
/// The cwd is the one that actually bit: `Config::project_config_path` walks
/// up from the working directory and stops only on reaching `$HOME`, so a test
/// that redirects `$HOME` to a tempdir *removes* that stopping point and the
/// walk climbs past the real `/home/<user>` into its `.newt/config.toml`.
/// Pinning `$HOME` alone made these tests read MORE developer state, not less.
#[test]
fn all_three_config_discovery_axes_are_pinned() {
    let cmd = common::newt();
    let root = cmd.home().to_path_buf();
    let deltas = env_deltas(&cmd);

    // Axis 1 + 2: the file and the user config root.
    for key in ["NEWT_CONFIG", "NEWT_CONFIG_DIR"] {
        assert_eq!(
            deltas.get(OsStr::new(key)),
            Some(&None),
            "{key} must be removed so the run cannot be redirected by the shell"
        );
    }
    // `home_dir()` reads HOME then USERPROFILE — pinning one leaves the other.
    for key in ["HOME", "USERPROFILE"] {
        assert_eq!(
            deltas.get(OsStr::new(key)).cloned().flatten().as_deref(),
            Some(root.as_os_str()),
            "{key} must be pinned into the throwaway root"
        );
    }
    // Axis 3, the one a hand-written `env_remove` list never covers.
    assert_eq!(
        cmd.get_current_dir(),
        Some(root.as_path()),
        "the working directory must be pinned, or the project-config walk \
         climbs out of the sandbox and into the real home"
    );
}

/// Non-vacuous companion: the pinned root really is a fresh, empty directory,
/// so "isolated" means "has no configuration" rather than "points somewhere
/// we did not check".
#[test]
fn the_isolated_root_starts_empty_and_is_unique_per_command() {
    let a = common::newt();
    let b = common::newt();
    assert_ne!(a.home(), b.home(), "each command gets its own root");
    for root in [a.home(), b.home()] {
        assert!(root.is_dir(), "root exists");
        assert_eq!(
            std::fs::read_dir(root).unwrap().count(),
            0,
            "an isolated root must start with no config at all"
        );
    }
    // `config_dir()` is the seam a test uses to plant one deliberately.
    let seeded = a.config_dir();
    assert!(
        seeded.starts_with(a.home()),
        "the config dir is inside the root"
    );
}

/// **The guard that makes the isolation hard to forget.**
///
/// A source tripwire in this repo's ratchet idiom: per-file counts of RAW
/// `newt`-binary construction, which may only go DOWN.
///
/// - `cli_tests.rs` is pinned at **0** — every command there is
///   `common::newt()`, and `assert_cmd::Command` is not even imported, so a
///   raw construction fails to compile before it reaches this test.
/// - `worker_cli.rs` and `stdout_purity.rs` keep small counts because they own
///   their spawns (a raw `std::process::Command`, a `tokio` one) and hand them
///   to `common::isolate`. The number is pinned so a NEW spawn site has to be
///   justified in review rather than appearing silently.
///
/// The needles are built with `concat!` so this file's own source — which
/// `include_str!` pulls in — cannot match them. Sources are embedded at
/// COMPILE time, so this does no filesystem I/O.
#[test]
fn newt_is_only_constructed_through_the_isolation_helper() {
    let needles = [
        concat!("Command::", "cargo_bin(\"newt\")"),
        concat!("cargo::", "cargo_bin(\"newt\")"),
    ];
    for (name, src, baseline) in [
        ("cli_tests.rs", include_str!("cli_tests.rs"), 0usize),
        ("worker_cli.rs", include_str!("worker_cli.rs"), 2),
        ("stdout_purity.rs", include_str!("stdout_purity.rs"), 0),
    ] {
        let found: usize = needles.iter().map(|n| src.matches(n).count()).sum();
        assert!(
            found <= baseline,
            "{name}: {found} raw `newt` construction(s), baseline {baseline} — \
             build it with `common::newt()`, or hand your own spawn to \
             `common::isolate` (#1852). This baseline ratchets DOWN only."
        );
    }
}

/// The shared policy is reachable from a raw `std::process::Command` too —
/// the shape `worker_cli.rs`'s metrics test needs. Asserted on the built
/// command, like everything else here.
#[test]
fn the_policy_applies_to_a_raw_std_command() {
    let root = common::isolated_root();
    let mut cmd = std::process::Command::new("/nonexistent/newt");
    common::isolate(&mut cmd, root.path());

    let deltas: HashMap<OsString, Option<OsString>> = cmd
        .get_envs()
        .map(|(k, v)| (k.to_owned(), v.map(OsStr::to_owned)))
        .collect();
    assert_eq!(deltas.get(OsStr::new("NEWT_CONFIG_DIR")), Some(&None));
    assert_eq!(
        deltas
            .get(OsStr::new("HOME"))
            .cloned()
            .flatten()
            .as_deref()
            .map(Path::new),
        Some(root.path())
    );
    assert_eq!(cmd.get_current_dir(), Some(root.path()));
}
