//! `newt web` — launch the newt-web HTMX cockpit.
//!
//! `newt-web` is DELIBERATELY not a workspace member (decision D1 in
//! `docs/decisions/newt_web_htmx.md`): its axum/sanitizer dependency tree
//! must never enter the agent workspace graph. So this command is a pure
//! process launcher — no dependency edge, just find-and-spawn:
//!
//! 1. `$NEWT_WEB_BIN` (explicit override, e.g. a dev build)
//! 2. `newt-web` beside the running `newt` binary (`just install-web` puts
//!    it there)
//! 3. `newt-web` on `PATH`
//!
//! Everything but the spawn is pure ([`candidate_paths`] / [`resolve`]) —
//! the `dgx_pull.rs` discipline.

use std::path::{Path, PathBuf};

/// Env override naming the newt-web binary to launch.
pub const WEB_BIN_ENV: &str = "NEWT_WEB_BIN";

/// Explicit candidate paths for the newt-web binary, in precedence order:
/// the env override, then a sibling of the running executable. (`PATH`
/// lookup is the spawn-time fallback, not a path candidate.) Pure.
pub fn candidate_paths(env_override: Option<&str>, current_exe: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = env_override.map(str::trim).filter(|p| !p.is_empty()) {
        out.push(PathBuf::from(p));
    }
    if let Some(dir) = current_exe.and_then(Path::parent) {
        out.push(dir.join(format!("newt-web{}", std::env::consts::EXE_SUFFIX)));
    }
    out
}

/// First explicit candidate that exists (`exists` injected so this stays
/// fs-free in unit tests); `None` = fall back to a bare `PATH` lookup. Pure.
pub fn resolve(
    env_override: Option<&str>,
    current_exe: Option<&Path>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidate_paths(env_override, current_exe)
        .into_iter()
        .find(|p| exists(p))
}

/// The actionable not-found message — one place, asserted by the CLI test.
pub fn not_found_message() -> String {
    format!(
        "newt-web is not installed (checked ${WEB_BIN_ENV}, a `newt-web` beside this \
         binary, and PATH). It is a separately built crate (decision D1 — its web \
         dependency tree stays out of the agent workspace): install it with \
         `just install-web`, or run it in place with \
         `cargo run --manifest-path newt-web/Cargo.toml`"
    )
}

/// Spawn newt-web with `args`, inheriting stdio, and propagate its exit.
pub fn run(args: &[String]) -> anyhow::Result<i32> {
    let env_override = std::env::var(WEB_BIN_ENV).ok();
    let current_exe = std::env::current_exe().ok();
    let program = resolve(
        env_override.as_deref(),
        current_exe.as_deref(),
        &|p: &Path| p.is_file(),
    )
    .unwrap_or_else(|| PathBuf::from("newt-web"));

    match std::process::Command::new(&program).args(args).status() {
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("{}", not_found_message())
        }
        Err(e) => Err(anyhow::anyhow!(
            "could not launch {}: {e}",
            program.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_outranks_the_sibling_candidate() {
        let exe = PathBuf::from("/opt/bin/newt");
        let paths = candidate_paths(Some("/custom/newt-web"), Some(&exe));
        assert_eq!(paths[0], PathBuf::from("/custom/newt-web"));
        assert_eq!(
            paths[1],
            PathBuf::from("/opt/bin").join(format!("newt-web{}", std::env::consts::EXE_SUFFIX))
        );
    }

    #[test]
    fn blank_override_and_missing_exe_yield_no_candidates() {
        assert!(candidate_paths(Some("   "), None).is_empty());
        assert!(candidate_paths(None, None).is_empty());
    }

    #[test]
    fn resolve_returns_the_first_existing_candidate_else_none() {
        let exe = PathBuf::from("/opt/bin/newt");
        let sibling =
            PathBuf::from("/opt/bin").join(format!("newt-web{}", std::env::consts::EXE_SUFFIX));
        // Override missing on disk → the sibling wins.
        let hit = resolve(Some("/custom/newt-web"), Some(&exe), &|p| p == sibling);
        assert_eq!(hit, Some(sibling));
        // Nothing exists → None (spawn falls back to PATH).
        assert_eq!(
            resolve(Some("/custom/newt-web"), Some(&exe), &|_| false),
            None
        );
    }

    #[test]
    fn the_not_found_message_names_every_escape_hatch() {
        let msg = not_found_message();
        assert!(msg.contains(WEB_BIN_ENV));
        assert!(msg.contains("just install-web"));
        assert!(msg.contains("--manifest-path newt-web/Cargo.toml"));
    }
}
