//! **One hermetic policy for tests that run the real `newt` binary**
//! (#1852, #2665).
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
//!
//! # The environment is cleared, not scrubbed (#2665)
//!
//! A scrub list is a second place the set of ambient inputs has to be
//! maintained, and it was always behind: the child still inherited the
//! `HERDR_*` triple when the tests ran inside a Herdr pane, so it connected to
//! the operator's live pane socket and sent `pane.release_agent`; it inherited
//! the proxy variables; and it reached the host's `~/.gitconfig` through
//! `git config --get`. So the policy now starts from `env_clear()` and admits
//! only [`INHERITED_ENV`]. Anything else a test needs, it pins after calling
//! the helper — the helper runs first, so the test's own `.env()` wins.
//!
//! Three more things the child may reach are owned by the fixture:
//!
//! * **Git identity** comes from configuration-from-environment
//!   (`GIT_CONFIG_COUNT`, git ≥ 2.31): `git config --get user.name` resolves
//!   to [`GIT_FIXTURE_NAME`] with no `~/.gitconfig` in the root and
//!   `GIT_CONFIG_NOSYSTEM` keeping `/etc/gitconfig` out.
//! * **Inference** goes to a loopback peer the test owns —
//!   [`inference_peer`] answers the startup probes of both wire shapes — and
//!   is configured explicitly, either in the fixture's config file or through
//!   `OLLAMA_HOST` for a config that names no backend (which `newt` honours
//!   since #2665).
//! * **Helper connections** (Herdr) are simply absent: no `HERDR_*` survives
//!   the clear. `hermetic_spawn.rs` proves it against a socket the test owns.
//! * **Temporary files** go into the root (`TMPDIR` / `TMP` / `TEMP` pinned
//!   there), and the root lives on RAM where the host has it — see
//!   [`isolated_root`] for the fsync measurement behind that.

#![allow(dead_code)] // each test binary uses a different subset of this module

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// Everything a spawned `newt` may inherit from the test process.
///
/// Deliberately NOT an exclusion list. Each entry is here because the child
/// cannot run correctly without it, and none of them steers which
/// configuration, backend, identity or helper a run resolves. `PATH` is how
/// `git` (identity, hardened helpers) and MCP fixture commands are found;
/// `LLVM_PROFILE_FILE` is how `just cov-ci` collects the spawned binary's
/// coverage — drop it and the whole integration tier silently vanishes from
/// the floor. The loader paths are what cargo set up for this test binary.
pub const INHERITED_ENV: &[&str] = &[
    "PATH",
    "RUST_BACKTRACE",
    "LLVM_PROFILE_FILE",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
];

/// The Windows loader and shell cannot run a child without these; the same
/// set `newt-inference`'s provider-plugin spawn passes through its own
/// `env_clear()`.
#[cfg(windows)]
const INHERITED_ENV_WINDOWS: &[&str] =
    &["SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC", "PATHEXT"];

/// Where the child's own temporary files go: the root. Unix reads `TMPDIR`;
/// Windows reads `TMP` then `TEMP`. Nothing the child writes lands outside
/// the fixture, and it shares the root's filesystem (see [`isolated_root`]).
const CHILD_TEMP_ENV: &[&str] = &["TMPDIR", "TMP", "TEMP"];

/// The git identity every spawned `newt` resolves — never the host's.
pub const GIT_FIXTURE_NAME: &str = "Newt Fixture";
pub const GIT_FIXTURE_EMAIL: &str = "fixture@newt.invalid";

/// Git reads these as if they were a config file (`git config` docs,
/// "GIT_CONFIG_COUNT"). Pure environment, so the root stays a clean slate
/// and `git_hardening`'s `~/.gitconfig` audit sees exactly nothing.
const GIT_FIXTURE_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_CONFIG_COUNT", "2"),
    ("GIT_CONFIG_KEY_0", "user.name"),
    ("GIT_CONFIG_VALUE_0", GIT_FIXTURE_NAME),
    ("GIT_CONFIG_KEY_1", "user.email"),
    ("GIT_CONFIG_VALUE_1", GIT_FIXTURE_EMAIL),
];

/// The one thing every spawn mechanism in these tests must be able to do.
///
/// Three command types appear across `newt-cli/tests` — `assert_cmd::Command`,
/// `std::process::Command`, and `tokio::process::Command`. Without this trait
/// the policy would be written three times, which is how two of the three
/// drifted in the first place.
pub trait CommandEnv {
    fn clear(&mut self);
    fn pin(&mut self, key: &str, value: &OsStr);
    fn pin_cwd(&mut self, dir: &Path);
}

impl CommandEnv for assert_cmd::Command {
    fn clear(&mut self) {
        self.env_clear();
    }
    fn pin(&mut self, key: &str, value: &OsStr) {
        self.env(key, value);
    }
    fn pin_cwd(&mut self, dir: &Path) {
        self.current_dir(dir);
    }
}

impl CommandEnv for std::process::Command {
    fn clear(&mut self) {
        self.env_clear();
    }
    fn pin(&mut self, key: &str, value: &OsStr) {
        self.env(key, value);
    }
    fn pin_cwd(&mut self, dir: &Path) {
        self.current_dir(dir);
    }
}

impl CommandEnv for tokio::process::Command {
    fn clear(&mut self) {
        self.env_clear();
    }
    fn pin(&mut self, key: &str, value: &OsStr) {
        self.env(key, value);
    }
    fn pin_cwd(&mut self, dir: &Path) {
        self.current_dir(dir);
    }
}

/// The hermetic policy: a cleared environment plus [`INHERITED_ENV`], all
/// three config-discovery axes pinned at `root`, and the fixture git identity.
///
/// `root` must outlive the spawn — [`newt`] owns a [`TempDir`] for exactly
/// that reason.
pub fn isolate<C: CommandEnv>(cmd: &mut C, root: &Path) {
    cmd.clear();
    for key in inherited_env() {
        if let Some(value) = std::env::var_os(key) {
            cmd.pin(key, &value);
        }
    }
    // `home_dir()` reads `HOME` then `USERPROFILE`; pinning one and leaving
    // the other would isolate Unix and not Windows.
    cmd.pin("HOME", root.as_ref());
    cmd.pin("USERPROFILE", root.as_ref());
    // Axis 3. Without this, pinning HOME actively defeats the project walk's
    // stop-at-home guard — see the module docs.
    cmd.pin_cwd(root);
    for key in CHILD_TEMP_ENV {
        cmd.pin(key, root.as_ref());
    }
    for (key, value) in GIT_FIXTURE_ENV {
        cmd.pin(key, OsStr::new(value));
    }
}

fn inherited_env() -> impl Iterator<Item = &'static str> {
    #[cfg(windows)]
    let platform = INHERITED_ENV_WINDOWS;
    #[cfg(not(windows))]
    let platform: &[&str] = &[];
    INHERITED_ENV.iter().chain(platform).copied()
}

/// A loopback inference peer that answers `newt`'s startup probes for both
/// wire shapes — `/api/tags` + `/api/ps` + `/api/show` (Ollama) and
/// `/v1/models` (OpenAI-compatible) — serving exactly `model`, so a spawned
/// chat adopts it and reaches nothing else. Anything unmounted (a turn, a
/// pull) is a fast 404; a test that drives a turn mounts its own responder.
pub async fn inference_peer(model: &str) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let routes = [
        (
            "GET",
            "/api/tags",
            serde_json::json!({"models": [{"name": model}]}),
        ),
        ("GET", "/api/ps", serde_json::json!({"models": []})),
        ("POST", "/api/show", serde_json::json!({})),
        (
            "GET",
            "/v1/models",
            serde_json::json!({"object": "list", "data": [{"id": model, "object": "model"}]}),
        ),
    ];
    for (verb, route, body) in routes {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
    }
    server
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

/// The `newt` binary, isolated at a caller-owned root.
///
/// The caller must keep `root` alive until the child exits. A test which needs
/// a distinct workspace must make it a child of `root` before changing the
/// command's current directory, so config discovery stops at the fake home
/// rather than walking into the developer's real profile.
pub fn newt_at(root: &Path) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::cargo_bin("newt").expect("built newt binary");
    isolate(&mut cmd, root);
    cmd
}

/// The `newt` binary, isolated. **The default way these tests should build one.**
pub fn newt() -> Newt {
    let root = isolated_root();
    let cmd = newt_at(root.path());
    Newt { root, cmd }
}

/// An isolated root for a caller that owns its own spawn (a raw
/// `std::process::Command`, or a `tokio` one) — pair with [`isolate`] — and
/// for any fixture directory the child will write into.
///
/// On the fastest filesystem the host offers. The child takes its config
/// lock through `atomic_fs`, which fsyncs; measured on a developer box while
/// another lane's build was writing (`strace -T`), ONE `fsync` of
/// `.newt/config.toml.lock` on the shared ext4 `/tmp` took 15.8 s — most of a
/// 30 s budget spent waiting for someone else's disk I/O, and the shape of
/// every "times out locally, passes in CI" report on these suites. The root
/// holds kilobytes, so a RAM-backed `/dev/shm` carries it when present;
/// `tempfile`'s default (`TMPDIR`, else the platform temp dir) otherwise.
pub fn isolated_root() -> TempDir {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        tempfile::tempdir_in(shm)
    } else {
        tempfile::tempdir()
    }
    .expect("isolated test root")
}
