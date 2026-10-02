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

/// The environment is cleared and only [`common::INHERITED_ENV`] survives, so
/// the question is no longer "is every ambient family scrubbed?" (a list that
/// was always behind — #2665 found `HERDR_*` and the proxies still leaking)
/// but "does the allowlist admit one?". It must not: nothing that steers
/// which configuration, backend, identity or helper a run resolves, and no
/// inference key. The built command's pins are checked the same way, because
/// a pin is the only way an ambient value can reach the child now.
#[test]
fn the_inherited_allowlist_admits_no_ambient_family() {
    let ambient = |key: &str| {
        [
            "NEWT_",
            "HERDR_",
            "OLLAMA_",
            "XDG_",
            "OPENAI_",
            "ANTHROPIC_",
        ]
        .iter()
        .any(|family| key.starts_with(family))
            || key.ends_with("_API_KEY")
            || key.ends_with("_PROXY")
            || key.ends_with("_proxy")
    };
    for key in common::INHERITED_ENV {
        assert!(
            !ambient(key),
            "{key} is on the inherited allowlist; the child must not see it"
        );
    }
    let cmd = common::newt();
    for key in env_deltas(&cmd).keys() {
        let key = key.to_string_lossy();
        assert!(!ambient(&key), "{key} is pinned into the isolated command");
    }
}

/// The git identity the child resolves is the fixture's, by configuration
/// the child cannot miss: `git config --get` reads `GIT_CONFIG_COUNT` the way
/// it reads a file, there is no `~/.gitconfig` in the root, and the system
/// file is switched off. `hermetic_spawn.rs` reads it back through the real
/// binary's `/byline`.
#[test]
fn the_git_identity_is_the_fixtures() {
    let cmd = common::newt();
    let deltas = env_deltas(&cmd);
    let pinned = |key: &str| {
        deltas
            .get(OsStr::new(key))
            .cloned()
            .flatten()
            .map(|v| v.to_string_lossy().into_owned())
    };
    assert_eq!(pinned("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
    assert_eq!(pinned("GIT_CONFIG_COUNT").as_deref(), Some("2"));
    assert_eq!(pinned("GIT_CONFIG_KEY_0").as_deref(), Some("user.name"));
    assert_eq!(
        pinned("GIT_CONFIG_VALUE_0").as_deref(),
        Some(common::GIT_FIXTURE_NAME)
    );
    assert_eq!(pinned("GIT_CONFIG_KEY_1").as_deref(), Some("user.email"));
    assert_eq!(
        pinned("GIT_CONFIG_VALUE_1").as_deref(),
        Some(common::GIT_FIXTURE_EMAIL)
    );
    assert!(
        !cmd.home().join(".gitconfig").exists(),
        "the identity is environment, not a file in the root"
    );
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

    // Axis 1 + 2: the file and the user config root. Nothing pins either —
    // and the clear means nothing inherits them.
    for key in ["NEWT_CONFIG", "NEWT_CONFIG_DIR"] {
        assert!(
            !deltas.contains_key(OsStr::new(key)),
            "{key} must not be pinned, or the run can be redirected"
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
/// - `identity_cli.rs` is pinned at **0** — its caller-owned home and nested
///   workspace use `common::newt_at`, so its construction cannot skip the
///   shared policy.
/// - `worker_cli.rs` and `stdout_purity.rs` keep small counts because they own
///   their spawns (a raw `std::process::Command`, a `tokio` one) and hand them
///   to `common::isolate`. The number is pinned so a NEW spawn site has to be
///   justified in review rather than appearing silently.
/// - #2665 brought every remaining binary under the policy. The non-zero
///   baselines left are the sites that take the binary's PATH rather than
///   spawn it (`doctor_cli.rs` writes it into an MCP config, `web_cli.rs`
///   copies it, `mcp_probe_cli.rs` probes it) or hand a raw spawn to
///   `common::isolate` (`mcp_cli/grant_net.rs`).
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
        ("headless_cli.rs", include_str!("headless_cli.rs"), 0),
        (
            "headless_cli/cognition.rs",
            include_str!("headless_cli/cognition.rs"),
            0,
        ),
        ("identity_cli.rs", include_str!("identity_cli.rs"), 0),
        ("worker_cli.rs", include_str!("worker_cli.rs"), 2),
        ("stdout_purity.rs", include_str!("stdout_purity.rs"), 0),
        ("doctor_cli.rs", include_str!("doctor_cli.rs"), 2),
        ("mcp_cli.rs", include_str!("mcp_cli.rs"), 0),
        (
            "mcp_cli/grant_net.rs",
            include_str!("mcp_cli/grant_net.rs"),
            1,
        ),
        ("mcp_probe_cli.rs", include_str!("mcp_probe_cli.rs"), 1),
        (
            "net_guard_selfexec_cli.rs",
            include_str!("net_guard_selfexec_cli.rs"),
            0,
        ),
        (
            "ocap_denials_cli.rs",
            include_str!("ocap_denials_cli.rs"),
            0,
        ),
        ("providers_cli.rs", include_str!("providers_cli.rs"), 0),
        ("setup_cli.rs", include_str!("setup_cli.rs"), 0),
        ("timer_cli.rs", include_str!("timer_cli.rs"), 0),
        ("tunings_cli.rs", include_str!("tunings_cli.rs"), 0),
        ("web_cli.rs", include_str!("web_cli.rs"), 1),
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
    assert!(!deltas.contains_key(OsStr::new("NEWT_CONFIG_DIR")));
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
