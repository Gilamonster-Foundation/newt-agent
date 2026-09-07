//! Confused-deputy-safe `git` subprocess invocation (step-7.4).
//!
//! # Why this exists
//!
//! The harness runs `git` as a *subprocess* in several internal, non-model
//! paths — collecting turn-end evidence ([`crate::agentic`] `claim_check`),
//! building the workspace context banner, computing a diff for the ACP worker,
//! crew bookkeeping. Each of those runs `git` **in the user's workspace**, which
//! on the hostile-repository / hostile-model threat model is attacker-controlled.
//!
//! `git` is a confused-deputy engine: a repository's `.git/config` (or
//! `.gitattributes`) can point ordinary read commands at an arbitrary program —
//! `core.fsmonitor` (fires on `git status`), `core.hooksPath` / hooks,
//! `diff.external` and per-driver `textconv` (fire on `git diff`), `core.pager`,
//! `core.sshCommand`, `protocol.ext`. A raw `Command::new("git")` in the
//! workspace therefore executes attacker code **outside** the Landlock/OCAP
//! fence, inheriting newt's full environment (provider keys, `NEWT_AGENT_KEY`).
//! This was empirically confirmed: `git status` with a repo-local
//! `core.fsmonitor=<payload>` ran the payload out-of-fence.
//!
//! [`hardened_git`] neutralizes that surface for every harness `git` call:
//!
//! - **`-c` overrides** beat repo-local `.git/config`, so `core.fsmonitor=`,
//!   `core.hooksPath=/dev/null`, `core.pager=cat`, `core.sshCommand=false`,
//!   `diff.external=`, and `protocol.ext.allow=never` disarm those gadgets even
//!   when the attacker wrote them into the repo.
//! - **`env_clear` + a minimal allowlist** drops every ambient gadget variable
//!   (`GIT_EXTERNAL_DIFF`, `GIT_SSH*`, `GIT_PAGER`, `GIT_ASKPASS`, …) AND newt's
//!   own secrets/authority, so a gadget that somehow still fires gets neither a
//!   payload from the environment nor newt's credentials.
//! - **`GIT_CONFIG_GLOBAL=/dev/null` + `GIT_CONFIG_SYSTEM=/dev/null`** ignore the
//!   user/system git config entirely.
//!
//! `textconv` uses *named* drivers that `-c` cannot wildcard away, so a caller
//! that runs `git diff` / `git log -p` / `git show` should ALSO pass
//! `--no-textconv --no-ext-diff` in `args` (belt-and-suspenders on top of the
//! `diff.external=` override).

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Git config keys/values forced via `-c` so a hostile repo `.git/config` cannot
/// turn a harness `git` call into code execution. `-c` outranks repo-local
/// config, so these win even when the attacker set the opposite in `.git/config`.
const GIT_HARDENING_OVERRIDES: &[&str] = &[
    "core.fsmonitor=",          // no fsmonitor hook (fires on `git status`)
    "core.hooksPath=/dev/null", // no hooks (fire on commit/checkout)
    "core.pager=cat",           // no pager subprocess
    "core.sshCommand=false",    // no ssh gadget
    "core.askpass=",            // no askpass gadget
    "core.editor=false",        // no editor gadget
    "diff.external=",           // no external diff program
    "protocol.ext.allow=never", // no `ext::` transport
];

/// Build a **confused-deputy-safe** `git` [`Command`] running in `cwd` with
/// `args`. Every harness `git` subprocess that touches a (possibly hostile)
/// workspace must go through this instead of a raw `Command::new("git")`.
///
/// On macOS, resolve the executable in the parent before scrubbing PATH.
/// A bare program plus a changed PATH makes Rust fall back from `posix_spawn`
/// to `fork`/`exec`, where parallel launches have crashed before exec with
/// libplatform's "os_once_t is corrupt". Missing Git fails before spawning;
/// the environment and config restrictions are unchanged.
///
/// # Errors
/// On macOS, an absent PATH, no executable candidate, or an unavailable working
/// directory returns an I/O error. Actual execution still checks OS permissions;
/// an ACL-denied candidate fails at spawn rather than selecting another Git.
pub fn hardened_git(cwd: &Path, args: &[&str]) -> io::Result<Command> {
    let path = std::env::var_os("PATH");
    let mut c = Command::new(git_program(cwd, path.as_deref())?);
    // A top-level option: never take the optional fsmonitor/index locks that can
    // trigger the fsmonitor hook as a side effect.
    c.arg("--no-optional-locks");
    for kv in GIT_HARDENING_OVERRIDES {
        c.arg("-c").arg(kv);
    }
    c.args(args).current_dir(cwd);

    // Start from an EMPTY environment: no ambient GIT_* gadget var, and none of
    // newt's secrets/authority, can reach git or a gadget that fires.
    c.env_clear();
    if let Some(path) = path {
        c.env("PATH", path);
    }
    // Keep HOME for git's own housekeeping, but the global config is redirected
    // to /dev/null below, so ~/.gitconfig / XDG git config are ignored anyway.
    if let Some(home) = std::env::var_os("HOME") {
        c.env("HOME", home);
    }
    c.env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat");
    Ok(c)
}

fn git_program(cwd: &Path, path: Option<&OsStr>) -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let cwd = std::path::absolute(cwd)?;
        // Relative/empty PATH entries retain their child-working-directory
        // semantics. Do not canonicalize symlinks or substitute a system Git
        // for the operator's selected executable. An absent PATH fails closed.
        path.into_iter()
            .flat_map(std::env::split_paths)
            .map(|entry| cwd.join(entry).join("git"))
            .find(|program| {
                program.metadata().is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "git executable not found in PATH")
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cwd, path);
        Ok(PathBuf::from("git"))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn git_program_is_resolved_before_the_environment_is_scrubbed() {
        let command = hardened_git(Path::new("."), &["--version"]).unwrap();
        assert!(
            Path::new(command.get_program()).is_absolute(),
            "a PATH-scrubbed bare program forces Rust's fork/pre-exec path"
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == "GIT_CONFIG_GLOBAL" && value == Some(std::ffi::OsStr::new("/dev/null"))
        }));
    }

    /// Real files ground the lookup predicate's ordering and execute-bit checks;
    /// the structural regression above alone cannot verify filesystem behavior.
    #[test]
    fn executable_lookup_preserves_path_order_and_child_relative_entries() {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = std::path::absolute(fixture.path()).unwrap();
        for (name, mode) in [
            ("not-executable", 0o600),
            ("first", 0o700),
            ("second", 0o700),
        ] {
            let dir = cwd.join(name);
            std::fs::create_dir(&dir).unwrap();
            let file = dir.join("git");
            std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let path = std::env::join_paths([
            Path::new("missing"),
            Path::new("not-executable"),
            Path::new("first"),
            Path::new("second"),
        ])
        .unwrap();
        assert_eq!(
            git_program(&cwd, Some(&path)).unwrap(),
            cwd.join("first/git")
        );
        let reversed = std::env::join_paths([cwd.join("second"), cwd.join("first")]).unwrap();
        assert_eq!(
            git_program(&cwd, Some(&reversed)).unwrap(),
            cwd.join("second/git")
        );
    }

    /// Real symlinks ground the predicate's promise to retain the selected path.
    #[test]
    fn empty_path_entry_searches_child_cwd_and_symlink_is_not_rewritten() {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = std::path::absolute(fixture.path()).unwrap();
        let target = cwd.join("selected-git");
        std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&target, cwd.join("git")).unwrap();
        assert_eq!(
            git_program(&cwd, Some(OsStr::new(""))).unwrap(),
            cwd.join("git")
        );
    }

    /// An empty real directory grounds the lookup's fail-closed missing-path case.
    #[test]
    fn absent_or_unusable_path_fails_in_the_parent_without_fallback() {
        let fixture = tempfile::tempdir().unwrap();
        for path in [None, Some(OsStr::new("missing"))] {
            assert_eq!(
                git_program(fixture.path(), path).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
        }
    }
}
