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
//!   and `protocol.ext.allow=never` disarm those gadgets even
//!   when the attacker wrote them into the repo.
//! - **`env_clear` + a minimal allowlist** drops every ambient gadget variable
//!   (`GIT_EXTERNAL_DIFF`, `GIT_SSH*`, `GIT_PAGER`, `GIT_ASKPASS`, …) AND newt's
//!   own secrets/authority, so a gadget that somehow still fires gets neither a
//!   payload from the environment nor newt's credentials.
//! - **`GIT_CONFIG_GLOBAL=/dev/null` + `GIT_CONFIG_SYSTEM=/dev/null`** ignore the
//!   user/system git config entirely.
//!
//! `textconv` uses *named* drivers that `-c` cannot wildcard away, so an internal
//! caller that runs `git diff` / `git log -p` / `git show` must pass
//! `--no-textconv --no-ext-diff` in `args`. The host-only `diff.external=`
//! override fails closed by selecting an invalid empty program if external
//! diff is accidentally enabled; it does not select Git's built-in diff.
//! Model commands already run under the command's filesystem/exec fence and
//! omit that override so ordinary native diffs retain their meaning.

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
pub(crate) fn git_dirs(workspace: &Path) -> Option<(PathBuf, PathBuf)> {
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
pub(crate) fn own_branch(workspace: &Path) -> Option<String> {
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
pub(crate) fn is_default_branch(workspace: &Path, branch: &str) -> bool {
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

/// The remote `origin`'s default branch name (`refs/remotes/origin/HEAD`),
/// or `None` when it cannot be resolved (offline/remote-less repo) — the
/// governed PR broker (issue-1188 amendment A4) falls back to `"main"` in
/// that case, same as [`is_default_branch`]'s hardcoded-name fallback.
#[must_use]
pub fn origin_default_branch_name(workspace: &Path) -> Option<String> {
    let output = hardened_git(
        workspace,
        &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
    )
    .ok()?
    .output()
    .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .strip_prefix("refs/remotes/origin/")
        .map(str::to_owned)
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
    "protocol.ext.allow=never", // no `ext::` transport
];

/// Host metadata callers must explicitly disable external diff in their argv.
/// This invalid command is a fail-closed backstop for those trusted callers,
/// not a usable default for a model's ordinary, already-confined Git command.
const GIT_HOST_ONLY_OVERRIDES: &[&str] = &["diff.external="];

/// The git settings a sandbox profile may carry, and the only ones copied from
/// the operator's own config. Fail-closed: a key reaches sandbox git only when
/// it matches here (exact key, or a `section.` prefix). Everything a config can
/// use to run a program or reach a credential is absent by construction:
/// `alias.*` (`!cmd`), `credential.*`, `include*`, `url.*.insteadOf`,
/// `core.sshCommand`, `gpg.*`, filter and diff drivers.
const COPYABLE_GIT_CONFIG: &[&str] = &[
    "init.defaultbranch",
    "core.autocrlf",
    "core.eol",
    "core.safecrlf",
    "core.quotepath",
    "core.whitespace",
    "pull.rebase",
    "pull.ff",
    "push.default",
    "fetch.prune",
    "merge.conflictstyle",
    "rebase.autosquash",
    "rebase.autostash",
    "rerere.enabled",
    "diff.algorithm",
    "diff.renames",
    "diff.colormoved",
    "branch.sort",
    "tag.sort",
    "commit.verbose",
    "help.autocorrect",
    "color.",
    "column.",
    "status.",
    "log.",
];

/// Is `key` on [`COPYABLE_GIT_CONFIG`]? Git section and variable names are
/// case-insensitive.
fn copyable(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    COPYABLE_GIT_CONFIG.iter().any(|allowed| {
        if allowed.ends_with('.') {
            key.starts_with(allowed) && !key[allowed.len()..].contains('.')
        } else {
            key == *allowed
        }
    })
}

/// The copyable settings in a `git config --list` listing (`key=value` per
/// line; a later line wins, as in git).
#[must_use]
pub fn copyable_git_config(listing: &str) -> std::collections::BTreeMap<String, String> {
    listing
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| copyable(key))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.to_owned()))
        .collect()
}

/// The operator's own global git config as a `--list` listing, read in the
/// harness (never the sandbox) through [`hardened_git`] with an explicit
/// `--file`: `~/.gitconfig`, then `$XDG_CONFIG_HOME/git/config`. Missing files
/// contribute nothing.
///
/// # Errors
/// When no git executable can be found.
pub fn ambient_git_config_listing() -> io::Result<String> {
    let Some(home) = crate::config::home_dir() else {
        return Ok(String::new());
    };
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let mut listing = String::new();
    for file in [home.join(".gitconfig"), xdg.join("git").join("config")] {
        if !file.is_file() {
            continue;
        }
        let file = file.to_string_lossy().into_owned();
        let output = hardened_git(&home, &["config", "--file", &file, "--list"])?.output()?;
        if output.status.success() {
            listing.push_str(&String::from_utf8_lossy(&output.stdout));
        }
    }
    Ok(listing)
}

/// The environment for every `git` the model runs in the confined shell.
///
/// - No ambient user or system config. The fence cannot read `~/.gitconfig`,
///   and a live run's `git status` died on exactly that (`unable to access
///   '~/.gitconfig': Operation not permitted`). The operator's own settings
///   arrive only as the vetted `profile` snapshot.
/// - `profile`: the sandbox profile's plain settings, filtered again through
///   [`COPYABLE_GIT_CONFIG`] so a hand-edited or repository-supplied profile
///   cannot carry what the copy would have refused.
/// - `user.name`/`user.email` = `author`, so a commit made inside the sandbox
///   (`stash`, `merge`, `rebase`, `cherry-pick`) is never identity-less.
/// - [`GIT_HARDENING_OVERRIDES`] LAST, as git's environment config
///   (`GIT_CONFIG_COUNT`/`_KEY_n`/`_VALUE_n`, `-c` precedence, so it beats a
///   hostile `.git/config`). The model can still unset these in its own
///   command; the threat this answers is the repository, which cannot set the
///   environment.
/// - No forced `diff.external`: even an empty value selects an external diff
///   and breaks Git's built-in diff. Any repository-selected diff program runs
///   inside the same admitted native process fence.
#[must_use]
pub fn sandbox_git_env(
    author: (&str, &str),
    profile: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut config: Vec<(&str, &str)> = profile
        .iter()
        .filter(|(key, _)| copyable(key))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    config.extend([
        // git's XDG defaults (`~/.config/git/ignore`, `…/attributes`) are
        // ambient config too; unset, git warns that the fence refused them.
        ("core.excludesFile", "/dev/null"),
        ("core.attributesFile", "/dev/null"),
        ("user.name", author.0),
        ("user.email", author.1),
    ]);
    config.extend(
        GIT_HARDENING_OVERRIDES
            .iter()
            .map(|kv| kv.split_once('=').expect("each override is `key=value`")),
    );
    let mut env: Vec<(String, String)> = [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    env.push(("GIT_CONFIG_COUNT".to_owned(), config.len().to_string()));
    for (i, (key, value)) in config.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), key.to_owned()));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), value.to_owned()));
    }
    env
}

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
    for kv in GIT_HARDENING_OVERRIDES
        .iter()
        .chain(GIT_HOST_ONLY_OVERRIDES)
    {
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

/// `git config --local --list` (plus `--worktree`, when active) for
/// `workspace`'s repository, via [`hardened_git`] (so this read itself cannot
/// be redirected by the same hostile config it is inspecting). A repository
/// with no local config section at all is a clean, empty listing, not an
/// error.
///
/// With `extensions.worktreeConfig` enabled, Git additionally reads
/// `$GIT_DIR/config.worktree` as its OWN scope, distinct from `--local` — a
/// repo-local gadget key set only there would otherwise pass A1's scan
/// clean while still being read by the credentialed push (issue-1188 review
/// #2641, finding 3). `--worktree` is read only when that extension is
/// actually enabled (checked from the `--local` listing itself): asking for
/// it unconditionally errors on the vast majority of repositories that never
/// opted in, which is not a hostility signal.
///
/// # Errors
/// When git cannot be located/spawned, or a scope this reads exits with
/// anything other than "no matches" (`1`) — fails closed rather than
/// silently treating an unreadable/malformed config as clean.
/// `git remote get-url <remote>` for `workspace`, via [`hardened_git`].
/// Returns the URL git would resolve for a READ-ONLY purpose (deriving the
/// `owner/name` shown to the operator and passed to `gh --repo`). The
/// governed PUSH path does NOT use this — it reads
/// [`crate::git_staging::literal_remote_url`] instead, since staging's actual
/// dial never goes through a resolver that could apply `insteadOf` at all.
///
/// # Errors
/// A message naming the remote when git reports no such remote, or on I/O
/// failure launching git.
pub fn resolve_remote_url(workspace: &Path, remote: &str) -> Result<String, String> {
    let output = hardened_git(workspace, &["remote", "get-url", remote])
        .map_err(|e| e.to_string())?
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "no such remote '{remote}': {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Validate a resolved remote URL against issue-1188's amendment A2: only
/// `https://` and `ssh://` (including the scp-like `user@host:path` shorthand,
/// which IS an ssh URL to git) are pushable by the broker. `file://`, `ext::`,
/// bare local paths, and anything else are refused — `file://` can point
/// anywhere on the host filesystem and `ext::` runs an arbitrary program AS
/// the transport, exactly the class of gadget the rest of this module exists
/// to keep out of a host-side git invocation.
///
/// Returns the URL's host on success.
pub fn push_url_host(url: &str) -> Result<String, String> {
    let unsupported = || {
        format!(
        "unsupported remote URL scheme for a governed push: {url} (only https:// and ssh:// are pushable)"
    )
    };
    let valid_host = |host: &str| {
        !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    };

    if let Some(rest) = url.strip_prefix("https://") {
        // Reject embedded userinfo outright: `https://github.com@evil.example/…`
        // makes `github.com` LOOK like the host while Git actually connects to
        // `evil.example`, and a password/token embedded this way must never
        // reach a prompt/result/log either. A2 refuses the whole class.
        if rest.contains('@') {
            return Err(format!(
                "refused: an https push URL must not embed credentials or a username: {url}"
            ));
        }
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.split(':').next().unwrap_or(authority);
        return if valid_host(host) {
            Ok(host.to_ascii_lowercase())
        } else {
            Err(unsupported())
        };
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        // ssh://[user@]host[:port][/path] — an authority segment must exist
        // before the first `/`.
        let authority = rest.split('/').next().unwrap_or("");
        let host_part = authority.rsplit('@').next().unwrap_or(authority);
        let host = host_part.split(':').next().unwrap_or(host_part);
        return if valid_host(host) {
            Ok(host.to_ascii_lowercase())
        } else {
            Err(unsupported())
        };
    }
    // Any other explicit scheme (`file://`, `ext::`, `git://`, a credential
    // helper's own `scheme::` syntax) is refused outright, never treated as
    // scp-like shorthand.
    if url.contains("://") || url.contains("::") {
        return Err(unsupported());
    }
    // scp-like shorthand: `user@host:path`. Git recognizes this ONLY when no
    // `/` precedes the first `:` — `/tmp/repo@github.com:target` is a LOCAL
    // path to Git (the leading `/` rules out scp syntax) even though naive
    // string splitting on `@`/`:` would find `github.com` inside it.
    if let Some(colon) = url.find(':') {
        if url[..colon].contains('/') {
            return Err(unsupported()); // a local path, not a pushable URL
        }
        let authority = &url[..colon];
        let Some(host) = authority.rsplit_once('@').map(|(_, host)| host) else {
            return Err(unsupported()); // no `user@` — not scp syntax
        };
        return if valid_host(host) {
            Ok(host.to_ascii_lowercase())
        } else {
            Err(unsupported())
        };
    }
    Err(unsupported())
}

/// `(owner, name)` from a GitHub `https://github.com/…` or scp-like
/// `git@github.com:…` URL — amendment A4's `--repo owner/name` for `gh pr
/// create`. `None` for anything not shaped like a GitHub remote (the caller
/// refuses the PR broker in that case rather than guessing).
#[must_use]
pub fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let host = push_url_host(url).ok()?;
    if host != "github.com" {
        return None;
    }
    let path = if let Some(rest) = url.strip_prefix("https://") {
        rest.split_once('/')?.1
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        rest.split_once('/')?.1
    } else {
        url.split_once(':')?.1
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty()).then(|| (owner.to_string(), name.to_string()))
}

fn git_program(cwd: &Path, path: Option<&OsStr>) -> io::Result<PathBuf> {
    resolve_trusted_program(cwd, path, "git")
}

/// [`resolve_trusted_program`] for `git`, with no `cwd` bias — used by
/// [`crate::git_staging`], which never trusts a model-chosen `cwd` for
/// executable resolution at all (every trusted binary it spawns runs from a
/// harness-controlled staging directory).
///
/// # Errors
/// Same as [`resolve_trusted_program`].
pub fn trusted_git_program(path: Option<&OsStr>) -> io::Result<PathBuf> {
    resolve_trusted_program(Path::new("."), path, "git")
}

/// [`trusted_git_program`]'s sibling for `gh`.
///
/// # Errors
/// Same as [`resolve_trusted_program`].
pub fn trusted_gh_program(path: Option<&OsStr>) -> io::Result<PathBuf> {
    resolve_trusted_program(Path::new("."), path, "gh")
}

/// Resolve `name` from `path` the same trusted way on every Unix target, not
/// only macOS: only ABSOLUTE PATH entries are considered, so a relative or
/// empty entry can never resolve against the model-chosen `cwd` this process
/// is about to `chdir` into (`execvp`'s own relative-PATH-entry semantics are
/// exactly that "search relative to the current working directory" gadget —
/// issue-1188 review #2641 finding 5). This is stricter than plain `execvp`,
/// not merely a copy of it: previously only macOS resolved before scrubbing
/// PATH at all, and even that macOS lookup accepted relative entries.
///
/// Windows keeps the pre-existing bare-name behavior: `CreateProcess`'s
/// executable search does not repeat Unix's cwd-relative-PATH-entry gadget
/// the same way, and the Windows AppContainer refusal already covers the
/// dominant native-Git host-execution risk on that platform
/// (`windows_appcontainer_native_git_refusal`).
#[cfg(unix)]
fn resolve_trusted_program(cwd: &Path, path: Option<&OsStr>, name: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let _ = cwd;
    path.into_iter()
        .flat_map(std::env::split_paths)
        .filter(|entry| entry.is_absolute())
        .map(|entry| entry.join(name))
        .find(|program| {
            program.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{name} executable not found in an absolute PATH entry"),
            )
        })
}

#[cfg(not(unix))]
fn resolve_trusted_program(cwd: &Path, path: Option<&OsStr>, name: &str) -> io::Result<PathBuf> {
    let _ = (cwd, path);
    Ok(PathBuf::from(name))
}

#[cfg(all(test, unix))]
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
    fn executable_lookup_preserves_path_order_among_absolute_entries() {
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
            cwd.join("missing"),
            cwd.join("not-executable"),
            cwd.join("first"),
            cwd.join("second"),
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

    /// issue-1188 review #2641, finding 5: a relative or empty PATH entry
    /// must NEVER resolve against `cwd` — that is exactly the
    /// `execvp`-inherited gadget that let a repo-planted `git`/`gh` run with
    /// the host's credentials via a hostile (or merely inherited-relative)
    /// PATH. Would have failed before the fix: the prior macOS-only lookup
    /// joined a relative entry onto `cwd` and found `selected-git` here.
    #[test]
    fn relative_and_empty_path_entries_are_never_resolved_against_cwd() {
        let fixture = tempfile::tempdir().unwrap();
        let cwd = std::path::absolute(fixture.path()).unwrap();
        let planted = cwd.join("git");
        std::fs::write(&planted, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o700)).unwrap();
        for path in [OsStr::new(""), OsStr::new("."), OsStr::new("sub")] {
            assert_eq!(
                git_program(&cwd, Some(path)).unwrap_err().kind(),
                io::ErrorKind::NotFound,
                "a repo-writable relative/empty PATH entry must never be searched: {path:?}"
            );
        }
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
mod sandbox_git_env_tests {
    use super::*;

    fn config(env: &[(String, String)]) -> Vec<(String, String)> {
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        let count: usize = get("GIT_CONFIG_COUNT").unwrap().parse().unwrap();
        (0..count)
            .map(|i| {
                (
                    get(&format!("GIT_CONFIG_KEY_{i}")).unwrap(),
                    get(&format!("GIT_CONFIG_VALUE_{i}")).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn ambient_config_is_ignored() {
        let env = sandbox_git_env(("newt-agent", "a@b"), &Default::default());
        assert!(env.contains(&("GIT_CONFIG_GLOBAL".into(), "/dev/null".into())));
        assert!(env.contains(&("GIT_CONFIG_NOSYSTEM".into(), "1".into())));
    }

    #[test]
    fn every_hardening_override_rides_the_env_config() {
        let config = config(&sandbox_git_env(("newt-agent", "a@b"), &Default::default()));
        for kv in GIT_HARDENING_OVERRIDES {
            let (key, value) = kv.split_once('=').unwrap();
            assert!(
                config.contains(&(key.into(), value.into())),
                "{kv} missing: {config:?}"
            );
        }
    }

    #[test]
    fn invalid_external_diff_override_is_reserved_for_host_metadata() {
        let config = config(&sandbox_git_env(("newt-agent", "a@b"), &Default::default()));
        assert!(config.iter().all(|(key, _)| key != "diff.external"));
        let host =
            hardened_git(Path::new("."), &["diff", "--no-ext-diff", "--no-textconv"]).unwrap();
        assert!(host.get_args().any(|arg| arg == "diff.external="));
    }

    #[test]
    fn commits_are_authored_by_the_agent_identity() {
        let config = config(&sandbox_git_env(
            (
                crate::agent_identity::DEFAULT_AGENT_NAME,
                crate::agent_identity::DEFAULT_AGENT_EMAIL,
            ),
            &Default::default(),
        ));
        assert!(config.contains(&("user.name".into(), "newt-agent".into())));
        assert!(config.contains(&(
            "user.email".into(),
            "309460085+newt-agent@users.noreply.github.com".into()
        )));
    }
    #[test]
    fn a_copy_keeps_plain_settings_and_drops_every_gadget() {
        let listing = "user.name=Op\ninit.defaultBranch=main\nalias.x=!curl evil\n\
            credential.helper=osxkeychain\ncore.sshCommand=ssh -i k\ninclude.path=~/x\n\
            url.git@h:.insteadof=https://h/\ncolor.ui=auto\ncolor.diff.meta=blue\npull.rebase=true\n";
        let copied = copyable_git_config(listing);
        let keys: Vec<&str> = copied.keys().map(String::as_str).collect();
        assert_eq!(keys, ["color.ui", "init.defaultbranch", "pull.rebase"]);
    }

    #[test]
    fn a_profile_cannot_carry_what_the_copy_refuses() {
        let profile: std::collections::BTreeMap<String, String> = [
            ("alias.st".to_owned(), "!sh -c evil".to_owned()),
            ("init.defaultbranch".to_owned(), "trunk".to_owned()),
        ]
        .into();
        let config = config(&sandbox_git_env(("a", "a@b"), &profile));
        assert!(
            config.iter().all(|(key, _)| key != "alias.st"),
            "{config:?}"
        );
        assert!(config.contains(&("init.defaultbranch".into(), "trunk".into())));
    }

    #[test]
    fn the_hardening_floor_is_applied_after_the_profile() {
        let config = config(&sandbox_git_env(("a", "a@b"), &Default::default()));
        let last_user = config
            .iter()
            .rposition(|(k, _)| k.starts_with("user."))
            .unwrap();
        let first_floor = config
            .iter()
            .position(|(k, _)| k == "core.fsmonitor")
            .unwrap();
        assert!(first_floor > last_user, "{config:?}");
    }
}

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
        // Git resolves macOS's /var alias; compare the same physical paths.
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
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

/// Pure parsing/validation for the URL/owner-repo helpers the governed
/// brokers use, exercised without a real git process. The former
/// `hostile_push_config_key` denylist tests that lived here were deleted with
/// that function: the staging-repo broker (`crate::git_staging`) makes the
/// whole class of repo-local config gadget unrepresentable by never reading
/// the workspace's `.git/config` for the actual dial at all, rather than
/// enumerating known-hostile keys in it.
#[cfg(test)]
mod governed_push_config_tests {
    use super::*;

    #[test]
    fn https_ssh_and_scp_like_urls_resolve_the_right_host() {
        assert_eq!(
            push_url_host("https://github.com/o/r.git").as_deref(),
            Ok("github.com")
        );
        assert_eq!(
            push_url_host("ssh://git@github.com:22/o/r.git").as_deref(),
            Ok("github.com")
        );
        assert_eq!(
            push_url_host("git@github.com:o/r.git").as_deref(),
            Ok("github.com")
        );
    }

    /// Would have failed before A2: `file://` and `ext::` are not screened by
    /// any existing host allowlist check, so nothing else in the broker would
    /// have refused them.
    #[test]
    fn file_and_ext_transports_are_refused() {
        assert!(push_url_host("file:///home/op/other-repo").is_err());
        assert!(push_url_host("ext::sh -c evil").is_err());
        assert!(push_url_host("git://github.com/o/r.git").is_err());
    }

    /// Finding 1 (issue-1188 review #2641): embedded userinfo makes the
    /// checked host and the host Git actually dials DIFFERENT strings. Would
    /// have failed before the fix: naive `split(['/', '@'])` found
    /// `github.com` as the first token and returned it, while Git connects
    /// to `evil.example`.
    #[test]
    fn https_userinfo_host_confusion_is_refused() {
        assert!(push_url_host("https://github.com@evil.example/o/r.git").is_err());
        assert!(push_url_host("https://user:token@github.com/o/r.git").is_err());
    }

    /// Finding 1: a slash before the first `:` makes this a LOCAL path to
    /// Git, not scp-like shorthand — even though naive parsing would find an
    /// `@host:` pattern inside it. Would have failed before the fix.
    #[test]
    fn local_path_containing_at_and_colon_is_not_scp_syntax() {
        assert!(push_url_host("/tmp/repo@github.com:target").is_err());
        assert!(push_url_host("./relative/repo@host:target").is_err());
    }

    /// Finding 1: helper-like `scheme::` syntax must never fall through to
    /// the scp-shorthand branch.
    #[test]
    fn double_colon_helper_syntax_is_refused() {
        assert!(push_url_host("ext::sh -c 'evil'@host:path").is_err());
    }

    #[test]
    fn github_owner_repo_parses_https_and_scp_like() {
        assert_eq!(
            github_owner_repo("https://github.com/Org/Repo.git"),
            Some(("Org".to_string(), "Repo".to_string()))
        );
        assert_eq!(
            github_owner_repo("git@github.com:Org/Repo.git"),
            Some(("Org".to_string(), "Repo".to_string()))
        );
        assert_eq!(
            github_owner_repo("https://gitlab.example.com/Org/Repo.git"),
            None,
            "non-github hosts refuse the gh broker rather than guessing --repo"
        );
    }
}

/// Real git + a real tempdir repo: grounds [`resolve_remote_url`] against
/// actual `git remote` behavior, per the workspace's expensive/real-resource
/// testing tier (small and self-contained enough to run inline, same posture
/// as `own_gitdir_grant_tests` above).
#[cfg(all(test, unix))]
mod governed_push_process_tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
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

    #[test]
    fn resolve_remote_url_matches_what_git_would_actually_push_to() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        assert_eq!(
            resolve_remote_url(repo.path(), "origin").unwrap(),
            "https://github.com/o/r.git"
        );
    }

    #[test]
    fn resolve_remote_url_errors_on_missing_remote() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        assert!(resolve_remote_url(repo.path(), "origin").is_err());
    }
}
