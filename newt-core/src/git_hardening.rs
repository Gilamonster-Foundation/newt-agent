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

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Filesystem grants for the session workspace's **own** git metadata (F32,
/// #2537): the extra read/write roots that let the model commit and move the
/// ref of the branch it has checked out, without needing to reach outside the
/// workspace fence for anything else.
///
/// Empty on any failure to resolve, on detached HEAD, and on the default
/// branch (`main`/`master`/the remote's `HEAD`) — those get read-only access
/// to the metadata, no write roots, so a caller that MEETs these into the
/// session `fs_write` scope grants nothing extra.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OwnGitGrant {
    pub read: Vec<String>,
    pub write: Vec<String>,
}

/// Compute [`OwnGitGrant`] for `workspace`'s checked-out repository.
///
/// Resolves the worktree gitdir and the common (administrative) dir with
/// `git rev-parse`, never by hand-parsing the `.git` gitlink file.
///
/// PR #2577 round 3: `write` is consumed ONLY by the confined-shell lane's
/// per-dispatch caveats widening for a `git …` command
/// (`newt_core::agentic::tools::shell::dispatch_caveats_for_git_shell`), never
/// folded into the session `fs_write` scope (`write_file`/`edit_file` on git
/// metadata stay refused there — see `caveats::apply_cli_fs_grants`). Real
/// `git add`/`git commit` need `MAKE_REG`/`REFER`/`REMOVE_FILE` rights to
/// create `index.lock` and rename it over `index`, plus insert objects —
/// DIRECTORY rights a file-level Landlock rule cannot carry (round 1's
/// per-file list was empirically provable to work for the tool's OWN
/// direct-filesystem writes, but not for a REAL confined shell `git add`
/// under the kernel fence). So `write` is exactly two DIRECTORIES: the
/// worktree gitdir itself, and the common dir's `objects/` subtree. `refs/`
/// and `config`/`hooks/` stay out of it — the default-branch guard in
/// `newt-git` (`refuse_if_default_branch`) and the shell `git commit`
/// redirect (`run_command_creates_shell_git_commit`) are what stop a ref
/// move, not this grant. A confined-shell `rm -rf <common>/objects` IS
/// possible from a directory-write grant on non-default branches — Shawn
/// accepted that trade-off (see `RESULT-dec2-own-gitdir.md`).
pub fn own_gitdir_grants(workspace: &Path) -> OwnGitGrant {
    // PR #2577 round 4, Blocker 1: this is the SESSION-START call
    // (`caveats::apply_cli_fs_grants`'s sole production caller runs before any
    // model action). Prime the identity cache HERE, from this resolve, so a
    // later per-dispatch re-check (`own_gitdir_shell_write_grant`) has a
    // trustworthy answer to compare against instead of re-trusting whatever a
    // (by-then possibly model-rewritten) `.git` gitlink / `commondir` says.
    let resolved = git_dirs(workspace);
    prime_identity_cache(workspace, resolved.clone());
    let Some((common_dir, absolute_git_dir)) = resolved else {
        return OwnGitGrant::default();
    };
    let read = vec![
        path_to_string(&absolute_git_dir),
        path_to_string(&common_dir),
    ];
    let read_only = OwnGitGrant {
        read: read.clone(),
        write: Vec::new(),
    };

    let Some(branch) = own_branch(workspace) else {
        return read_only; // detached HEAD
    };
    if is_default_branch(workspace, &branch) {
        return read_only;
    }

    let write = vec![
        path_to_string(&absolute_git_dir),
        path_to_string(&common_dir.join("objects")),
    ];
    OwnGitGrant { read, write }
}

/// `(common_dir, absolute_git_dir)`.
type GitDirPair = (PathBuf, PathBuf);

/// The identity pair `own_gitdir_grants` resolved the ONE time it ran at
/// session bootstrap, keyed by workspace. `None` for a workspace bootstrap
/// never resolved (or resolved to "not a repo") is a distinct cache state
/// from "not yet looked up" — both read back as `None` from
/// [`cached_identity`], and both correctly deny the per-dispatch grant.
fn identity_cache() -> &'static Mutex<HashMap<PathBuf, Option<GitDirPair>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<GitDirPair>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prime_identity_cache(workspace: &Path, resolved: Option<GitDirPair>) {
    identity_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(workspace.to_path_buf(), resolved);
}

fn cached_identity(workspace: &Path) -> Option<GitDirPair> {
    identity_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(workspace)
        .cloned()
        .flatten()
}

/// Per-dispatch write grant for the confined-shell `git` lane
/// (`dispatch_caveats_for_git_shell`, PR #2577 round 3/4). Re-runs `rev-parse`
/// on EVERY `git` dispatch, but the write grant is bound to the identity
/// [`own_gitdir_grants`] cached at session start, not to whatever the fresh
/// resolve says: the workspace's `.git` gitlink and the worktree gitdir's
/// `commondir` file are both ordinary, model-writable files, so trusting a
/// live re-resolve would let a rewritten pointer grant kernel write on an
/// ENTIRELY DIFFERENT repository's `hooks/`/`config`/`objects` (Blocker 1) —
/// hooks mean code execution the next time anyone runs git there. The fresh
/// resolve is still run, but ONLY to CONFIRM it still equals the cached
/// identity; any mismatch — or no cached identity at all (session bootstrap
/// never ran, or resolved to "not a repo") — grants nothing. The branch check
/// is re-read fresh every dispatch, same as before: that can only NARROW the
/// grant (a default branch → nothing), never widen it, so re-reading it is
/// safe in a way re-trusting the path resolve is not.
pub fn own_gitdir_shell_write_grant(workspace: &Path) -> Vec<String> {
    let Some((cached_common, cached_git_dir)) = cached_identity(workspace) else {
        return Vec::new();
    };
    let Some((fresh_common, fresh_git_dir)) = git_dirs(workspace) else {
        return Vec::new();
    };
    if fresh_common != cached_common || fresh_git_dir != cached_git_dir {
        return Vec::new(); // re-pointed since session start — grant nothing
    }
    let Some(branch) = own_branch(workspace) else {
        return Vec::new();
    };
    if is_default_branch(workspace, &branch) {
        return Vec::new();
    }
    vec![
        path_to_string(&cached_git_dir),
        path_to_string(&cached_common.join("objects")),
    ]
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `(git-common-dir, absolute-git-dir)`, both made absolute against
/// `workspace` (`--git-common-dir` prints relative for a normal checkout).
fn git_dirs(workspace: &Path) -> Option<(PathBuf, PathBuf)> {
    let output = hardened_git(
        workspace,
        &["rev-parse", "--git-common-dir", "--absolute-git-dir"],
    )
    .ok()?
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let common_dir = lines.next()?;
    let absolute_git_dir = PathBuf::from(lines.next()?);
    let common_dir = if Path::new(common_dir).is_absolute() {
        PathBuf::from(common_dir)
    } else {
        workspace.join(common_dir)
    };
    Some((common_dir, absolute_git_dir))
}

/// The checked-out branch name (`refs/heads/<name>` stripped), or `None` for
/// detached HEAD / an unresolvable symbolic ref.
fn own_branch(workspace: &Path) -> Option<String> {
    let output = hardened_git(workspace, &["symbolic-ref", "-q", "HEAD"])
        .ok()?
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .strip_prefix("refs/heads/")
        .map(str::to_owned)
}

/// Is `branch` the repo's default branch — `main`, `master`, or whatever the
/// remote's `HEAD` points at? The remote lookup is best-effort: an offline or
/// remote-less repo just falls back to the hardcoded names.
fn is_default_branch(workspace: &Path, branch: &str) -> bool {
    if branch == "main" || branch == "master" {
        return true;
    }
    let Some(output) = hardened_git(
        workspace,
        &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|mut c| c.output().ok()) else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .strip_prefix("refs/remotes/origin/")
        .is_some_and(|default| default == branch)
}

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

/// Construct automatic Git metadata only with unrestricted read authority.
///
/// Execution hardening does not confine config includes, object alternates,
/// linked administrative directories, or child symlinks. Until those reads
/// are capability-aware, optional metadata must be unavailable under a bounded
/// grant. This check precedes even executable lookup and command construction.
pub fn metadata_git(
    cwd: &Path,
    args: &[&str],
    read_scope: &crate::Scope<String>,
) -> io::Result<Command> {
    crate::agentic::check_git_read_scope("metadata", read_scope)
        .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
    hardened_git(cwd, args)
}

/// Build a **confused-deputy-safe** `git` [`Command`] running in `cwd` with
/// `args`. Every harness `git` subprocess that touches a (possibly hostile)
/// workspace must go through this instead of a raw `Command::new("git")`.
/// This is execution hardening, not filesystem read confinement. Automatic
/// optional metadata must use [`metadata_git`] with the turn's read scope.
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
    // F26 v1 tried `GIT_CEILING_DIRECTORIES=cwd's parent` here, but this
    // builder is shared by `metadata_git`, which also backs the TUI's
    // HEAD/dirty line, the ACP worker's diff, and crew — a ceiling here
    // would blind every one of them to `cd newt-core && newt` (a workspace
    // that IS a repo subdirectory, the common case, not the leak case).
    // The enclosing-repo leak this was guarding against is fixed instead
    // where it is actually observed: `claim_check::snapshot_workspace`
    // scopes and prefix-strips its own status, below.
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

/// F26 v2 regression (portable — not macOS-gated, unlike the module above):
/// `hardened_git`/`metadata_git` must still discover a repo when run from a
/// SUBDIRECTORY of it. A v1 fix set `GIT_CEILING_DIRECTORIES` to stop upward
/// discovery, which also blinded `metadata_git` — shared by the TUI's
/// HEAD/dirty line, the ACP worker's diff, and crew — to the ordinary case of
/// `cd newt-core && newt`. That approach was reverted; this test pins that a
/// subdirectory launch still finds its repo.
#[cfg(test)]
mod subdir_discovery_tests {
    use super::*;

    #[test]
    fn hardened_git_from_a_repo_subdirectory_still_finds_the_repo() {
        let root = tempfile::tempdir().expect("repo root");
        let init = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .expect("git")
                .status
                .success());
        };
        init(&["init", "-q"]);
        init(&["config", "user.email", "t@example.com"]);
        init(&["config", "user.name", "t"]);
        std::fs::write(root.path().join("f.txt"), "one\n").unwrap();
        init(&["add", "f.txt"]);
        init(&["commit", "-q", "-m", "init"]);

        let subdir = root.path().join("sub");
        std::fs::create_dir(&subdir).unwrap();

        let out = hardened_git(&subdir, &["rev-parse", "HEAD"])
            .unwrap()
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "rev-parse HEAD from a repo subdirectory must still succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Real `git`, real tempdirs: grounds [`own_gitdir_grants`] against actual
/// worktree layouts rather than a belief about what `git rev-parse` prints.
/// Per the workspace testing tiers this is an expensive/real-resource test,
/// not the mocked unit tier; it is small and self-contained enough to run
/// inline rather than being split into the weekly suite.
#[cfg(all(test, unix))]
mod own_gitdir_grant_tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        std::fs::write(dir.join("seed"), "x").unwrap();
        git(dir, &["add", "seed"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// Would have failed before this fix: no grants existed at all, so
    /// `permits_path` denied every file `git add`/`git commit` touches on a
    /// linked worktree's own branch (F32, #2537).
    #[test]
    fn linked_worktree_on_own_branch_grants_the_two_write_directories() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root.path().join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );

        let grant = own_gitdir_grants(&wt);
        assert!(!grant.write.is_empty(), "own branch must get write roots");

        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        let scope = crate::caveats::Scope::only(grant.write.clone());
        let path = |rel: &str| main.join(".git").join(rel).to_string_lossy().into_owned();
        // What `git add` needs (round 3: the write grant serves the confined
        // SHELL lane's `git add` only — `git commit` from the shell is refused
        // outright and redirected to the `git` tool, so refs never need a
        // filesystem write grant here at all).
        for touched in [
            path("worktrees/wt/index"),
            path("worktrees/wt/index.lock"),
            path("worktrees/wt/HEAD"),
            path("worktrees/wt/logs/HEAD"),
            path("worktrees/wt/COMMIT_EDITMSG"),
            path("objects/pack/multi-pack-index"),
        ] {
            assert!(
                crate::caveats::permits_path(&scope, &touched),
                "{touched} must be permitted"
            );
        }
        // Refs, config, and hooks stay out of the write grant — moving a ref
        // is what `refuse_if_default_branch` (the `git` tool) guards, not a
        // filesystem grant.
        for denied in [
            path("refs/heads/task"),
            path("refs/heads/main"),
            path("config"),
            path("hooks/pre-commit"),
        ] {
            assert!(
                !crate::caveats::permits_path(&scope, &denied),
                "{denied} must stay denied"
            );
        }

        // The grant is real enough for a real (unconfined, this is a plain
        // subprocess — not the kernel fence) `git add` + `git commit` to
        // succeed; the confined-shell version of this property is
        // `newt-core::agentic::tools::shell::git_shell_dispatch_tests`.
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);
    }

    #[test]
    fn normal_checkout_on_own_branch_grants_add_and_commit_paths() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        git(repo.path(), &["checkout", "-q", "-b", "task"]);

        let grant = own_gitdir_grants(repo.path());
        assert!(!grant.write.is_empty());

        std::fs::write(repo.path().join("f.txt"), "hi").unwrap();
        git(repo.path(), &["add", "f.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "task work"]);
    }

    /// Writing `refs/heads/main`, `config`, or `hooks/pre-commit` must never be
    /// in the grant, on either layout.
    #[test]
    fn default_branch_checkout_grants_no_write_roots() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        git(repo.path(), &["branch", "-m", "main"]);

        let grant = own_gitdir_grants(repo.path());
        assert!(
            grant.write.is_empty(),
            "checkout on the default branch must not get commit authority: {:?}",
            grant.write
        );
    }

    #[test]
    fn detached_head_grants_read_only() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let head = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        git(repo.path(), &["checkout", "-q", &head]);

        let grant = own_gitdir_grants(repo.path());
        assert!(
            grant.write.is_empty(),
            "detached HEAD must not get write roots"
        );
        assert!(!grant.read.is_empty(), "detached HEAD still gets read");
    }
}
