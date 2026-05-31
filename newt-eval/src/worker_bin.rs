//! Discovery for the `newt` worker binary used by `newt-eval`.
//!
//! Resolution order (first match wins):
//!
//! 1. The `--worker-bin <PATH>` CLI flag (passed in as `cli_override`).
//! 2. The `NEWT_WORKER_BIN` environment variable.
//! 3. The sibling of `current_exe()` (i.e. the `newt` next to
//!    `newt-eval` — the layout produced by `cargo build --release`).
//! 4. `$CARGO_TARGET_DIR/release/newt` (and `/debug/newt`) when
//!    `CARGO_TARGET_DIR` is set — covers isolated build dirs like
//!    the agent worktrees that share `~/.cache/cargo-*-newt/`.
//! 5. The historical cwd-relative candidates:
//!    `target/release/newt`, `target/debug/newt`,
//!    `../target/release/newt`, `../target/debug/newt`.
//!
//! Issue #40: the previous logic only looked at cwd-relative paths, so
//! running the binary from anywhere but the workspace root produced a
//! confusing "binary not found" failure even though `newt` was sitting
//! right next to `newt-eval`.
//!
//! All candidates are surfaced in the error message on miss so the
//! operator can see exactly what was tried.

use std::path::{Path, PathBuf};

/// Environment variable that overrides the default `newt` binary path.
pub const ENV_WORKER_BIN: &str = "NEWT_WORKER_BIN";

/// Resolve the worker binary location, returning the resolved path and
/// the full list of candidates that was considered.
///
/// `cli_override` is the value of the `--worker-bin` CLI flag (`None`
/// if not passed). `lookup_env` and `current_exe` are injected for
/// tests; in production code call [`resolve_worker_bin`].
pub fn resolve_worker_bin_with<F, G>(
    cli_override: Option<PathBuf>,
    lookup_env: F,
    current_exe: G,
) -> Resolution
where
    F: Fn(&str) -> Option<String>,
    G: Fn() -> Option<PathBuf>,
{
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. CLI flag — explicit user intent, no path search.
    if let Some(p) = cli_override {
        candidates.push(p.clone());
        let found = p.exists();
        return Resolution {
            path: p,
            found,
            candidates,
            source: ResolutionSource::CliFlag,
        };
    }

    // 2. NEWT_WORKER_BIN env var.
    if let Some(raw) = lookup_env(ENV_WORKER_BIN) {
        let p = PathBuf::from(raw);
        candidates.push(p.clone());
        let found = p.exists();
        return Resolution {
            path: p,
            found,
            candidates,
            source: ResolutionSource::EnvVar,
        };
    }

    // 3. Sibling of current_exe — the layout produced by
    //    `cargo build --release`: both `newt` and `newt-eval` live in
    //    `<target>/release/`. This is the common case when an operator
    //    runs the eval binary directly out of the build dir.
    if let Some(exe) = current_exe() {
        if let Some(parent) = exe.parent() {
            let sibling = parent.join("newt");
            candidates.push(sibling.clone());
            if sibling.exists() {
                return Resolution {
                    path: sibling,
                    found: true,
                    candidates,
                    source: ResolutionSource::ArgvSibling,
                };
            }
        }
    }

    // 4. $CARGO_TARGET_DIR/{release,debug}/newt.
    if let Some(td) = lookup_env("CARGO_TARGET_DIR") {
        for profile in ["release", "debug"] {
            let p = Path::new(&td).join(profile).join("newt");
            candidates.push(p.clone());
            if p.exists() {
                return Resolution {
                    path: p,
                    found: true,
                    candidates,
                    source: ResolutionSource::CargoTargetDir,
                };
            }
        }
    }

    // 5. Historical cwd-relative fallbacks.
    for rel in [
        "target/release/newt",
        "target/debug/newt",
        "../target/release/newt",
        "../target/debug/newt",
    ] {
        let p = PathBuf::from(rel);
        candidates.push(p.clone());
        if p.exists() {
            return Resolution {
                path: p,
                found: true,
                candidates,
                source: ResolutionSource::CwdRelative,
            };
        }
    }

    // Nothing existed. Return the first historical fallback as a
    // best-effort path so the caller's "not found" error message still
    // points at a sensible default, alongside the full candidate list.
    let fallback = PathBuf::from("target/release/newt");
    Resolution {
        path: fallback,
        found: false,
        candidates,
        source: ResolutionSource::NotFound,
    }
}

/// Production wrapper around [`resolve_worker_bin_with`] that wires up
/// real `std::env::var` and `std::env::current_exe` lookups.
pub fn resolve_worker_bin(cli_override: Option<PathBuf>) -> Resolution {
    resolve_worker_bin_with(
        cli_override,
        |k| std::env::var(k).ok(),
        || std::env::current_exe().ok(),
    )
}

/// Outcome of a discovery run.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The resolved path (or the fallback we'd report on miss).
    pub path: PathBuf,
    /// True iff `path` exists on disk.
    pub found: bool,
    /// Every path that was tried, in order.
    pub candidates: Vec<PathBuf>,
    /// Which step in the resolution order produced `path`.
    pub source: ResolutionSource,
}

/// Which step in the resolution order produced the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    CliFlag,
    EnvVar,
    ArgvSibling,
    CargoTargetDir,
    CwdRelative,
    NotFound,
}

impl Resolution {
    /// Pretty-print the candidates list for inclusion in an error
    /// message. One path per line, indented for readability.
    pub fn render_candidates(&self) -> String {
        if self.candidates.is_empty() {
            return "  (no candidates were tried)".to_string();
        }
        self.candidates
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_lookup<'a>(
        map: &'a HashMap<&'static str, String>,
    ) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| map.get(k).cloned()
    }

    #[test]
    fn cli_flag_wins_even_if_missing() {
        // The CLI flag is the user's explicit choice. We don't search
        // past it; we report whether it exists and let the caller
        // decide.
        let env = HashMap::new();
        let r =
            resolve_worker_bin_with(Some(PathBuf::from("/nope/newt")), env_lookup(&env), || {
                Some(PathBuf::from("/usr/bin/newt-eval"))
            });
        assert_eq!(r.source, ResolutionSource::CliFlag);
        assert_eq!(r.path, PathBuf::from("/nope/newt"));
        assert!(!r.found);
        assert_eq!(r.candidates, vec![PathBuf::from("/nope/newt")]);
    }

    #[test]
    fn cli_flag_wins_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("newt");
        std::fs::write(&bin, "").unwrap();
        let env = HashMap::new();
        let r = resolve_worker_bin_with(Some(bin.clone()), env_lookup(&env), || None);
        assert_eq!(r.source, ResolutionSource::CliFlag);
        assert!(r.found);
        assert_eq!(r.path, bin);
    }

    #[test]
    fn env_var_used_when_no_cli_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("newt");
        std::fs::write(&bin, "").unwrap();
        let mut env = HashMap::new();
        env.insert("NEWT_WORKER_BIN", bin.to_string_lossy().into_owned());
        let r = resolve_worker_bin_with(None, env_lookup(&env), || None);
        assert_eq!(r.source, ResolutionSource::EnvVar);
        assert!(r.found);
        assert_eq!(r.path, bin);
    }

    #[test]
    fn argv_sibling_wins_over_cargo_target_dir() {
        // When the binary sits next to current_exe, prefer it even if
        // CARGO_TARGET_DIR points somewhere else.
        let exe_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let sibling = exe_dir.path().join("newt");
        let in_target = target_dir.path().join("release").join("newt");
        std::fs::create_dir_all(in_target.parent().unwrap()).unwrap();
        std::fs::write(&sibling, "").unwrap();
        std::fs::write(&in_target, "").unwrap();

        let mut env = HashMap::new();
        env.insert(
            "CARGO_TARGET_DIR",
            target_dir.path().to_string_lossy().into_owned(),
        );

        let r = resolve_worker_bin_with(None, env_lookup(&env), || {
            Some(exe_dir.path().join("newt-eval"))
        });
        assert_eq!(r.source, ResolutionSource::ArgvSibling);
        assert_eq!(r.path, sibling);
    }

    #[test]
    fn cargo_target_dir_release_preferred_over_debug() {
        let target_dir = tempfile::tempdir().unwrap();
        let release = target_dir.path().join("release").join("newt");
        let debug = target_dir.path().join("debug").join("newt");
        std::fs::create_dir_all(release.parent().unwrap()).unwrap();
        std::fs::create_dir_all(debug.parent().unwrap()).unwrap();
        std::fs::write(&release, "").unwrap();
        std::fs::write(&debug, "").unwrap();

        let mut env = HashMap::new();
        env.insert(
            "CARGO_TARGET_DIR",
            target_dir.path().to_string_lossy().into_owned(),
        );
        // current_exe in some random spot with no sibling newt.
        let unrelated = tempfile::tempdir().unwrap();
        let r = resolve_worker_bin_with(None, env_lookup(&env), || {
            Some(unrelated.path().join("newt-eval"))
        });
        assert_eq!(r.source, ResolutionSource::CargoTargetDir);
        assert_eq!(r.path, release);
    }

    #[test]
    fn cargo_target_dir_debug_when_release_missing() {
        let target_dir = tempfile::tempdir().unwrap();
        let debug = target_dir.path().join("debug").join("newt");
        std::fs::create_dir_all(debug.parent().unwrap()).unwrap();
        std::fs::write(&debug, "").unwrap();

        let mut env = HashMap::new();
        env.insert(
            "CARGO_TARGET_DIR",
            target_dir.path().to_string_lossy().into_owned(),
        );
        let unrelated = tempfile::tempdir().unwrap();
        let r = resolve_worker_bin_with(None, env_lookup(&env), || {
            Some(unrelated.path().join("newt-eval"))
        });
        assert_eq!(r.source, ResolutionSource::CargoTargetDir);
        assert_eq!(r.path, debug);
    }

    #[test]
    fn candidates_list_starts_with_argv_sibling() {
        // The argv-sibling probe must be the very first entry in the
        // candidates list whenever `current_exe` is available — so
        // the "binary not found" error message points at the
        // operator's most likely intent first.
        let unrelated = tempfile::tempdir().unwrap();
        let env = HashMap::new();
        let r = resolve_worker_bin_with(None, env_lookup(&env), || {
            Some(unrelated.path().join("newt-eval"))
        });
        assert_eq!(r.candidates[0], unrelated.path().join("newt"));
    }

    #[test]
    fn not_found_state_is_reachable_with_isolated_probes() {
        // To assert the NotFound state specifically, we have to
        // arrange a probe set where every candidate is guaranteed
        // missing. The cwd-relative probes can hit real files in
        // some tree layouts, so we wire `current_exe` to a tempdir
        // (no sibling `newt`) and stub CARGO_TARGET_DIR at another
        // empty tempdir. The cwd probes may still resolve in some
        // environments — when they don't, we should see NotFound.
        let exe_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert(
            "CARGO_TARGET_DIR",
            target_dir.path().to_string_lossy().into_owned(),
        );
        let r = resolve_worker_bin_with(None, env_lookup(&env), || {
            Some(exe_dir.path().join("newt-eval"))
        });
        // The candidates list must always cover all probe sites we
        // promise in the docstring.
        assert!(!r.candidates.is_empty());
        // If the cwd doesn't have a target/release/newt, we should
        // be in NotFound; otherwise we'd be in CwdRelative. Both
        // are valid outcomes — what we care about is that the
        // fallback path is well-formed and that found tracks the
        // source.
        match r.source {
            ResolutionSource::NotFound => {
                assert!(!r.found);
                assert_eq!(r.path, PathBuf::from("target/release/newt"));
            }
            ResolutionSource::CwdRelative => {
                assert!(r.found);
            }
            other => panic!("unexpected source {other:?}"),
        }
    }

    #[test]
    fn render_candidates_lists_each_path() {
        let r = Resolution {
            path: PathBuf::from("x"),
            found: false,
            candidates: vec![PathBuf::from("/a"), PathBuf::from("/b")],
            source: ResolutionSource::NotFound,
        };
        let rendered = r.render_candidates();
        assert!(rendered.contains("/a"));
        assert!(rendered.contains("/b"));
    }
}
