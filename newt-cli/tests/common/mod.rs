//! **One isolation policy for tests that run the real `newt` binary** (#1852).
//!
//! An integration test that spawns `newt` inherits the developer's
//! environment. `newt` then resolves its configuration from ambient state, so
//! the test's result depends on whatever is in that developer's `~/.newt` —
//! it passes in CI, where `$HOME` is empty, and fails on the machine of the
//! person least able to reproduce CI. This module is the one place a `newt`
//! command is constructed, so the isolation is a property of the constructor
//! rather than something each test has to remember.
//!
//! # Three axes, pinned together
//!
//! Config discovery has three independent inputs, and pinning a subset does
//! not isolate anything — it just changes which developer file leaks in:
//!
//! 1. **`$NEWT_CONFIG`** — an explicit config file.
//! 2. **`$NEWT_CONFIG_DIR`, else `$HOME/.newt`** — the user config root
//!    (`Config::user_config_dir`).
//! 3. **The working directory** — `Config::project_config_path` walks up from
//!    the cwd looking for `.newt/config.toml`.
//!
//! ## Why the cwd is load-bearing, and why pinning only `$HOME` made it worse
//!
//! The project walk stops when it reaches the home directory, so the global
//! `~/.newt` is never mistaken for a project override:
//!
//! ```ignore
//! if home == Some(current) { break; }         // config.rs, find_project_config_from
//! ```
//!
//! That guard is keyed on `$HOME`. Redirect `$HOME` to a tempdir and the walk
//! no longer recognises the real home as a stopping point — so it climbs
//! *past* `/home/<user>`, finds `/home/<user>/.newt/config.toml`, and adopts
//! it as a PROJECT config, sibling `backends/` drop-ins and all. Pinning
//! `$HOME` alone does not sandbox `newt`; it disables the one guard that was
//! keeping the real home out.
//!
//! This is not hypothetical: it is why `worker_cli.rs`'s tests failed while
//! their helper was already redirecting `$HOME` and scrubbing five variables.
//! Verified directly against the built binary — with `$HOME` pinned, running
//! from inside the repo reads the real `~/.newt` and fails; running the same
//! command from `/tmp` succeeds.
//!
//! So [`isolate`] pins the cwd too, into the same throwaway root. A test that
//! genuinely needs the repo as its workspace should pass the path explicitly
//! rather than rely on where the harness happened to be started.

#![allow(dead_code)] // each test binary uses a different subset of this module

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// Environment that can redirect config discovery or preselect a backend.
///
/// Scrubbed as a FAMILY. Removing `NEWT_CONFIG_DIR` while leaving
/// `NEWT_PROVIDER` set does not isolate the test, it just moves which piece of
/// the operator's session leaks in — `worker_cli.rs` carries a comment about
/// exactly that leak biting a developer (`NEWT_PROVIDER=ollama-cloud` from the
/// shell made the worker refuse to start).
///
/// Deliberately NOT every `NEWT_*` variable: this is the set that steers
/// *which configuration and which backend* a run resolves. A test that wants
/// to exercise one of these sets it back after construction — the helper
/// scrubs first, so the test's own `.env()` wins.
pub const AMBIENT_CONFIG_ENV: &[&str] = &[
    // Config location (axes 1 and 2 above).
    "NEWT_CONFIG",
    "NEWT_CONFIG_DIR",
    // Backend selection — the family #1850 was about. A developer with any of
    // these exported gets a different backend than CI does.
    "NEWT_PROVIDER",
    "NEWT_BACKEND",
    "NEWT_DGX_MODEL",
    "NEWT_TEAM",
    // The legacy DGX env shim, which SYNTHESIZES an endpoint when set and so
    // can conjure a backend that exists in no config file at all.
    "NEWT_DGX_HOST",
    "NEWT_DGX_SCHEME",
    "NEWT_DGX_OLLAMA_URL",
    "NEWT_DGX_OLLAMA_PORT",
    // Config overlays and profile selection.
    "NEWT_PROFILE",
    "NEWT_BUNDLE",
    "NEWT_LOADOUT",
    // Identity and session state a stray export would bind the run to.
    "NEWT_OPERATOR_KEY",
    "NEWT_RESUME",
    "NEWT_CONVERSATION_ID",
    "NEWT_CODER",
    // Non-`NEWT_` inputs the backend resolver reads directly. An exported
    // OPENAI_API_KEY reaching a test process is also a secret-handling
    // concern, not merely a flakiness one.
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_MODEL",
    "OLLAMA_HOST",
];

/// The one thing every spawn mechanism in these tests must be able to do.
///
/// Three command types appear across `newt-cli/tests` — `assert_cmd::Command`,
/// `std::process::Command`, and `tokio::process::Command`. Without this trait
/// the policy would be written three times, which is how two of the three
/// drifted in the first place.
pub trait CommandEnv {
    fn scrub(&mut self, key: &str);
    fn pin(&mut self, key: &str, value: &OsStr);
    fn pin_cwd(&mut self, dir: &Path);
}

impl CommandEnv for assert_cmd::Command {
    fn scrub(&mut self, key: &str) {
        self.env_remove(key);
    }
    fn pin(&mut self, key: &str, value: &OsStr) {
        self.env(key, value);
    }
    fn pin_cwd(&mut self, dir: &Path) {
        self.current_dir(dir);
    }
}

impl CommandEnv for std::process::Command {
    fn scrub(&mut self, key: &str) {
        self.env_remove(key);
    }
    fn pin(&mut self, key: &str, value: &OsStr) {
        self.env(key, value);
    }
    fn pin_cwd(&mut self, dir: &Path) {
        self.current_dir(dir);
    }
}

impl CommandEnv for tokio::process::Command {
    fn scrub(&mut self, key: &str) {
        self.env_remove(key);
    }
    fn pin(&mut self, key: &str, value: &OsStr) {
        self.env(key, value);
    }
    fn pin_cwd(&mut self, dir: &Path) {
        self.current_dir(dir);
    }
}

/// Pin all three config-discovery axes at `root`. The single policy.
///
/// `root` must outlive the spawn — [`newt`] owns a [`TempDir`] for exactly
/// that reason.
pub fn isolate<C: CommandEnv>(cmd: &mut C, root: &Path) {
    for key in AMBIENT_CONFIG_ENV {
        cmd.scrub(key);
    }
    // `home_dir()` reads `HOME` then `USERPROFILE`; pinning one and leaving
    // the other would isolate Unix and not Windows.
    cmd.pin("HOME", root.as_ref());
    cmd.pin("USERPROFILE", root.as_ref());
    // Axis 3. Without this, pinning HOME actively defeats the project walk's
    // stop-at-home guard — see the module docs.
    cmd.pin_cwd(root);
}

/// A `newt` command that cannot see the developer's configuration, and the
/// throwaway root it is pinned to.
///
/// Derefs to the command, so a call site reads the way it did before:
/// `Command::cargo_bin("newt").unwrap()` becomes `common::newt()`.
pub struct Newt {
    root: TempDir,
    cmd: assert_cmd::Command,
}

impl Newt {
    /// The isolated `$HOME` — also the cwd, and the parent of the config dir.
    pub fn home(&self) -> &Path {
        self.root.path()
    }

    /// The isolated `~/.newt`. Created on demand so a test can seed it.
    pub fn config_dir(&self) -> PathBuf {
        let dir = self.root.path().join(".newt");
        std::fs::create_dir_all(&dir).expect("create isolated config dir");
        dir
    }
}

impl std::ops::Deref for Newt {
    type Target = assert_cmd::Command;
    fn deref(&self) -> &Self::Target {
        &self.cmd
    }
}

impl std::ops::DerefMut for Newt {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cmd
    }
}

/// The `newt` binary, isolated. **The only way these tests should build one.**
pub fn newt() -> Newt {
    let root = tempfile::tempdir().expect("isolated test root");
    let mut cmd = assert_cmd::Command::cargo_bin("newt").expect("built newt binary");
    isolate(&mut cmd, root.path());
    Newt { root, cmd }
}

/// An isolated root for a caller that owns its own spawn (a raw
/// `std::process::Command`, or a `tokio` one). Pair with [`isolate`].
pub fn isolated_root() -> TempDir {
    tempfile::tempdir().expect("isolated test root")
}
