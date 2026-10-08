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

mod branch;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod ref_namespace;
pub(crate) use branch::create_worktree_branch;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
/// under the kernel fence). So `write` is exactly TWO DIRECTORIES: the
/// worktree gitdir itself and the common dir's `objects/` subtree.
/// `config`/`hooks/` and EVERY refs/reflog namespace in the common dir
/// (`refs/heads`, `refs/tags`, `refs/remotes`, `logs/HEAD`, …) stay out of
/// it — including the checked-out branch's own `refs/heads/<branch>`.
///
/// #2682 (round 5, `8c537f17`) added `refs/heads/` and `logs/refs/heads/`
/// here so a confined `git commit` could advance the checked-out branch's
/// own ref — `git add` alone never touches a ref, so round 3's two-directory
/// grant was never exercised against a real `git commit`, and it failed:
/// `fatal: cannot lock ref 'HEAD': Unable to create
/// '<common>/refs/heads/<branch>.lock': Permission denied`, then (once
/// `refs/heads/` was added) `fatal: cannot update the ref
/// 'refs/heads/<branch>': unable to append to
/// '<common>/logs/refs/heads/<branch>': Permission denied`.
///
/// **Round 6 (#2686 review round 2) reverted that widening**: a directory-wide
/// Landlock grant on `refs/heads/`/`logs/refs/heads/` covers EVERY branch's
/// ref and reflog, not just the checked-out one, and nothing about a `git`
/// VERB guards it — `inspect_commands`'s unconditional refusal of `update-ref`/
/// `branch -f/-m/-c`/`symbolic-ref`/`push`
/// (`newt-core::agentic::tools::native_git`) only inspects `git` invocations,
/// so a compound confined-shell command whose SECOND segment is a plain shell
/// redirect (`git add . && echo payload > <common>/refs/heads/main`, not a
/// `git` verb at all) reached straight past it: measured, under round 5's
/// grant, writing into a SIBLING branch's ref/reflog AND into `main`'s own
/// ref/reflog both succeeded (`confined_shell_cannot_redirect_into_a_sibling_or_default_branch_ref_or_reflog`
/// in `newt-core::agentic::tools_tests::helper_git_shell_grant`, red before
/// this revert). `dispatch_caveats_for_git_shell`'s doc comment already names
/// an analogous `objects/`-directory trade-off as deliberately accepted (a
/// confined `rm -rf <common>/objects` becomes possible) — but THAT directory
/// holds no ref identity, while `refs/heads/` is exactly the authority
/// boundary between branches, so the same shape of exposure there is not an
/// accepted trade-off, it is the hole.
///
/// A commit now lands with HEAD DETACHED instead: the confined child never
/// touches the common dir's `refs/heads/`/`logs/refs/heads/` at all (a
/// detached HEAD's own ref IS the worktree-local `HEAD` file, already
/// inside the worktree-gitdir grant), and
/// [`crate::agentic::tools::native_git::needs_commit_broker`] firing routes
/// the dispatch through a HOST-SIDE, bounded `update-ref` compare-and-swap —
/// [`own_branch_for_commit_ref_move`], [`detach_own_head`],
/// [`advance_own_branch_ref`], [`reattach_own_head`] — that verifies the
/// resulting commit really is a fast-forward (or amendment) of the branch's
/// OWN previous tip before moving ONLY that one ref (reusing
/// `inspect_commands`'s guard, unmodified, against everything else).
///
/// **Round 7 (#2686 review round 3, P1)**: the host-side broker above was
/// itself unbound — `advance_own_branch_ref`/`reattach_own_head` re-derived
/// the git-dir/common-dir pair with a fresh `git rev-parse` against the
/// WORKSPACE, i.e. through the worktree's `.git` gitlink and the admin
/// dir's `commondir` file, both ordinary files inside this grant's own
/// write set. A confined child's second, non-`git` compound segment could
/// rewrite either AFTER a legitimate detached commit, redirecting the
/// publish/reattach onto a DIFFERENT repository (concretely: the primary
/// checkout's own `HEAD`/`refs/heads/<branch>`). [`BoundGitIdentity`] closes
/// this: the pair is resolved and held (as directory descriptors on Unix)
/// ONCE, by [`own_branch_for_commit_ref_move`], before the confined child
/// ever runs, and every later operation binds `GIT_DIR` to those held paths
/// — re-verified against the descriptors' `(dev, ino)` immediately before
/// use — instead of ever re-resolving through the
/// workspace again.
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

/// #2720: bind an ambient caller's repository only if no session has bound
/// this workspace yet. Never replace an existing identity (including a cached
/// refusal), since scoped siblings rely on it to detect rewritten Git pointers.
pub(crate) fn ambient_gitdir_write_grant(workspace: &Path) -> Vec<String> {
    let resolved = git_dirs(workspace);
    identity_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(workspace.to_path_buf())
        .or_insert(resolved);
    own_gitdir_shell_write_grant(workspace)
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
    let Some((common_dir, git_dir)) = verified_identity(workspace) else {
        return Vec::new();
    };
    let Some(branch) = own_branch(workspace) else {
        return Vec::new();
    };
    if is_default_branch(workspace, &branch) {
        return Vec::new();
    }
    vec![
        path_to_string(&git_dir),
        path_to_string(&common_dir.join("objects")),
    ]
}

/// `(common_dir, absolute_git_dir)` when the identity [`own_gitdir_grants`]
/// cached at session start still matches a FRESH resolve — the one point
/// `.git`/`commondir` are re-consulted at all, and only to confirm they
/// still agree with what session start trusted. A mismatch (re-pointed
/// since session start) or no cached identity (bootstrap never ran, or
/// resolved to "not a repo") both return `None`; the caller treats them
/// identically. Factored out of [`own_gitdir_shell_write_grant`] so every
/// caller that needs the identity PAIR (not just the write-grant strings —
/// [`own_branch_for_commit_ref_move`] does) shares one check.
fn verified_identity(workspace: &Path) -> Option<(PathBuf, PathBuf)> {
    let (cached_common, cached_git_dir) = cached_identity(workspace)?;
    let (fresh_common, fresh_git_dir) = git_dirs(workspace)?;
    if fresh_common != cached_common || fresh_git_dir != cached_git_dir {
        return None; // re-pointed since session start — trust nothing
    }
    Some((cached_common, cached_git_dir))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A worktree's git-dir/common-dir identity, bound ONCE — via held
/// directory descriptors on Unix — right before a commit-creating confined
/// child runs (#2686 review round 3, P1). The child's write grant includes
/// the worktree's own admin dir (for `HEAD`, `index`, …), which also holds
/// the `commondir` pointer file, and the workspace's `.git` gitlink is an
/// ordinary, model-writable file too — both reachable by a compound
/// command's non-`git` second segment. Every host-side operation
/// bracketing the child ([`detach_own_head`], [`advance_own_branch_ref`],
/// [`reattach_own_head`]) binds `GIT_DIR` to the paths resolved HERE —
/// never a fresh `git rev-parse` against the workspace, which would walk
/// back through whatever the child just rewrote — and [`Self::verify`]
/// re-checks their `(dev, ino)` against the held descriptor before each
/// use. Ref/object resolution always targets [`Self::common_dir`]
/// DIRECTLY, never the admin dir plus an env override: measured, git
/// ignores `GIT_COMMON_DIR` outright whenever `GIT_DIR` itself names a
/// directory carrying its own `commondir` file (a linked worktree's admin
/// dir always does), so that env var cannot stand in for a rewritten one.
/// The one read that is genuinely worktree-local — the detached `HEAD`
/// oid — is read as a raw file beneath the held descriptor instead
/// ([`Self::read_detached_head`]), never through git's own resolution.
pub struct BoundGitIdentity {
    /// Read only by the linux/macos-only native-write machinery below
    /// (`verify`, `hardened_git_common`), so gated the same way as that
    /// machinery rather than left as dead code on another platform.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    common_dir: PathBuf,
    git_dir: PathBuf,
    /// Native beneath-safe opens ([`crate::fs_cap::WorkspaceDir`]) exist only
    /// on Linux/macOS (`newt-core/src/lib.rs`'s `fs_cap` gate), so every
    /// field below is gated to match that exactly — not the broader
    /// `cfg(unix)` a round-4 draft used, which would also claim a
    /// non-Linux/macOS Unix (e.g. a BSD) that has no `fs_cap` module either
    /// (#2686 review round 5, P2: "gate the native implementation to
    /// Linux/macOS, where fs_cap exists").
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    common_root: agent_bridle_fdguard::GrantedRoot,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    git_root: agent_bridle_fdguard::GrantedRoot,
    /// The SAME two directory OBJECTS as `common_root`/`git_root` — never a
    /// second, independent open of the intended pathname (#2686 review
    /// round 5, P1: a round-4 draft opened these via
    /// [`crate::fs_cap::WorkspaceDir::open_root`] against the same pathname
    /// `common_root`/`git_root` had just been acquired from, which is two
    /// opens of one intended object bound only by "nothing hostile ran in
    /// between", not by a kernel guarantee). [`Self::bind`] derives these by
    /// `dup`ing `common_root`/`git_root`'s own fd
    /// ([`crate::fs_cap::WorkspaceDir::from_granted_root`]), so the object
    /// identity [`Self::verify`] checks IS the object every write below
    /// lands in, by construction — not merely by argument. Two capability
    /// TYPES over the same object, not duplicated authority: `GrantedRoot`
    /// exposes only `open_read`/`open_write`, with no `O_EXCL` create or
    /// `renameat` — both of which the native ref-update protocol (round 4,
    /// P1) needs and `WorkspaceDir` already has.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    common_workspace: crate::fs_cap::WorkspaceDir,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    git_workspace: crate::fs_cap::WorkspaceDir,
}

impl BoundGitIdentity {
    /// Bind `common_dir`/`git_dir` — already [`verified_identity`]-checked
    /// against the session-start cache by the caller — as held descriptors.
    /// Linux/macOS only (the one caller, [`own_branch_for_commit_ref_move`],
    /// never reaches this on any other platform — see its own `cfg(not(...))`
    /// arm, which returns `None` without calling this at all).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn bind(common_dir: PathBuf, git_dir: PathBuf) -> Result<Self, String> {
        Self::bind_seamed(common_dir, git_dir, &|| {})
    }

    /// [`Self::bind`] plus a test-only seam: `between_acquire_and_derive`
    /// fires, for each of the two directories, right after
    /// [`agent_bridle_fdguard::GrantedRoot::acquire`] has resolved the
    /// pathname and right before [`crate::fs_cap::WorkspaceDir::from_granted_root`]
    /// derives the write handle from that SAME held fd — proving there is no
    /// surviving window in which the two could end up bound to different
    /// objects, because the second one is no longer a pathname resolution at
    /// all (#2686 review round 5, P1's "deterministic seam test: replace and
    /// restore the pathname between the two former opens"). A no-op in
    /// production.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn bind_seamed(
        common_dir: PathBuf,
        git_dir: PathBuf,
        between_acquire_and_derive: &dyn Fn(),
    ) -> Result<Self, String> {
        let common_root = agent_bridle_fdguard::GrantedRoot::acquire(&common_dir).map_err(|e| {
            format!(
                "refused: cannot bind the common git dir '{}' ({e})",
                common_dir.display()
            )
        })?;
        let git_root = agent_bridle_fdguard::GrantedRoot::acquire(&git_dir).map_err(|e| {
            format!(
                "refused: cannot bind the worktree admin dir '{}' ({e})",
                git_dir.display()
            )
        })?;
        between_acquire_and_derive();
        // Derived by `dup`ing the ALREADY-HELD fd above — never a second
        // `open()` of `common_dir`/`git_dir`'s pathname. Whatever
        // `between_acquire_and_derive` just did to the pathname cannot
        // affect this: there is no pathname resolution left to redirect.
        let common_workspace = crate::fs_cap::WorkspaceDir::from_granted_root(&common_root)
            .map_err(|e| {
                format!(
                    "refused: cannot bind the common git dir '{}' ({e})",
                    common_dir.display()
                )
            })?;
        let git_workspace =
            crate::fs_cap::WorkspaceDir::from_granted_root(&git_root).map_err(|e| {
                format!(
                    "refused: cannot bind the worktree admin dir '{}' ({e})",
                    git_dir.display()
                )
            })?;
        Ok(Self {
            common_dir,
            git_dir,
            common_root,
            git_root,
            common_workspace,
            git_workspace,
        })
    }

    /// Re-check both held identities against a fresh `stat` of the SAME two
    /// paths this was bound to — never a fresh `git rev-parse` through the
    /// workspace. Fails closed: the directory at that exact path is no
    /// longer the object bound before the confined child ran.
    pub fn verify(&self) -> Result<(), String> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::unix::fs::MetadataExt;
            for (label, path, root) in [
                ("common git dir", &self.common_dir, &self.common_root),
                ("worktree admin dir", &self.git_dir, &self.git_root),
            ] {
                let now = std::fs::metadata(path).map_err(|e| {
                    format!(
                        "refused: the {label} '{}' can no longer be inspected ({e})",
                        path.display()
                    )
                })?;
                let held = root.identity();
                if (now.dev(), now.ino()) != (held.device, held.inode) {
                    return Err(format!(
                        "refused: the {label} '{}' no longer names the directory bound \
                         before the confined command ran",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }

    /// Read the worktree-local `HEAD` file directly — beneath the held
    /// descriptor on Unix — never through git's own gitdir machinery.
    /// Measured: for a linked worktree's admin dir, git reads that dir's
    /// own `commondir` FILE unconditionally to find refs/objects, and
    /// `GIT_COMMON_DIR` has NO effect in that case — it is silently
    /// ignored whenever `GIT_DIR` itself carries a `commondir` file, so it
    /// cannot override a rewritten one. Reading `HEAD` as a raw file sides
    /// steps that resolution entirely: a detached `HEAD` is nothing but a
    /// literal hex object id, and verifying the id is itself a reachable
    /// commit happens AFTER this read, against [`Self::common_dir`]
    /// directly ([`advance_own_branch_ref`]'s `commit_parents_of` call).
    ///
    /// `Ok(None)` for a symbolic ref (not detached) or an absent file.
    fn read_detached_head(&self) -> Result<Option<String>, String> {
        let text = {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                use std::io::Read;
                match self.git_root.open_read(Path::new("HEAD")) {
                    Ok(mut file) => {
                        let mut text = String::new();
                        file.read_to_string(&mut text)
                            .map_err(|e| format!("refused: cannot read HEAD ({e})"))?;
                        text
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(format!("refused: cannot read HEAD ({e})")),
                }
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                match std::fs::read_to_string(self.git_dir.join("HEAD")) {
                    Ok(text) => text,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(format!("refused: cannot read HEAD ({e})")),
                }
            }
        };
        let text = text.trim();
        if text.is_empty() || text.starts_with("ref:") {
            return Ok(None);
        }
        Ok(Some(text.to_string()))
    }
}

/// [`hardened_git`] pinned DIRECTLY to `identity`'s held COMMON dir — an
/// ordinary, non-worktree gitdir from git's own perspective, so there is no
/// `commondir` file for it to read at all and nothing for a confined
/// child's write grant on the ADMIN dir to redirect. Every REMAINING
/// git-spawn read (`rev-parse`, `cat-file`, the commit-subject `log`) routes
/// through this. The writes that used to spawn `git update-ref`/
/// `symbolic-ref` here — [`detach_own_head`], [`reattach_own_head`],
/// [`advance_own_branch_ref`] — no longer spawn git at all (#2686 review
/// round 4, P1): spawning git with `GIT_DIR` set to a PATHNAME only compares
/// the held directory object (via [`BoundGitIdentity::verify`]) without ever
/// USING it — the spawned process re-resolves that same pathname itself, a
/// check-to-use gap a directory swap between the two could still win. Those
/// three now write natively through [`BoundGitIdentity`]'s held
/// `WorkspaceDir` handles — `openat`/`renameat` relative to the ALREADY-OPEN
/// descriptor — so there is no second pathname resolution left to redirect.
/// Linux/macOS only — its only callers ([`rev_parse`], [`commit_parents_of`],
/// [`git_text`]) all serve the native ref-move machinery above, which is
/// itself gated the same way.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn hardened_git_common(identity: &BoundGitIdentity, args: &[&str]) -> io::Result<Command> {
    // Residual: these read queries reacquire common_dir by pathname; they do
    // not read through the held object-database descriptor. Identity checks do
    // not close that check-to-use window. Native ref/HEAD writes are separately
    // descriptor-bound. See docs/security/ocap-deviations.md.
    let mut cmd = hardened_git(&identity.common_dir, args)?;
    cmd.env("GIT_DIR", &identity.common_dir);
    Ok(cmd)
}

/// Bound state for one commit-creating `run_command` dispatch: the
/// branch/tip it will publish to, plus the identity every host-side
/// operation around the confined child binds to.
pub struct OwnBranchRefMove {
    pub branch: String,
    pub old_tip: String,
    pub identity: Arc<BoundGitIdentity>,
}

/// #2682 round 2 (#2686 review): is this workspace eligible for the
/// host-side commit-ref-move broker, and if so, what branch/tip does it
/// bind to? Reuses [`verified_identity`] (the SAME gate that used to decide
/// whether to widen the confined fence onto `refs/heads/`) rather than
/// duplicating it — it now decides whether to run the detach/advance/
/// reattach dance around an UNWIDENED dispatch instead, and (round 3, P1)
/// supplies the [`BoundGitIdentity`] that dance is bound to.
///
/// `None` also for a genuinely unborn branch (no prior commit on it) — there
/// is no tip to detach `HEAD` to, so this mechanism does not cover a
/// worktree's first-ever commit on brand-new, history-less branch. That is a
/// narrower surface than #2682's own fix (which granted the directory
/// unconditionally), accepted because every real worktree-commit scenario
/// starts from an existing commit (`git worktree add -b <branch>
/// <start-point>`); a genuinely unborn branch committing through this path
/// fails exactly as it did before #2682 (no ref-directory write at all). A
/// failure to bind the identity (fd exhaustion, a symlink on the way) is
/// the same narrowing, not a widening: the commit then fails exactly as it
/// did with no ref-directory write at all, never with a wider grant.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn own_branch_for_commit_ref_move(workspace: &Path) -> Option<OwnBranchRefMove> {
    let (common_dir, git_dir) = verified_identity(workspace)?;
    let branch = own_branch(workspace)?;
    if is_default_branch(workspace, &branch) {
        return None;
    }
    let identity = BoundGitIdentity::bind(common_dir, git_dir).ok()?;
    let old_tip = own_branch_tip(&identity, &branch)?;
    Some(OwnBranchRefMove {
        branch,
        old_tip,
        identity: Arc::new(identity),
    })
}

/// This platform has no [`crate::fs_cap::WorkspaceDir`] (native beneath-safe
/// writes are Linux/macOS only, matching `fs_cap`'s own gate) — `None`,
/// exactly the same narrowing this function already documents for an
/// unborn branch or a failed bind (#2686 review round 5, P2: "give other
/// platforms an explicit fail-closed unsupported path"). A commit-creating
/// dispatch then runs with `HEAD` attached and fails exactly as it did
/// before this mechanism existed — no ref-directory write grant — never
/// with a wider one; the retired pathname-`GIT_DIR` git spawn is not
/// restored.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn own_branch_for_commit_ref_move(_workspace: &Path) -> Option<OwnBranchRefMove> {
    None
}

/// `refs/heads/<branch>`'s current oid, or `None` for an unborn branch (no
/// commits on it yet — `rev-parse --verify` exits non-zero rather than
/// erroring, so this is the ordinary "nothing there yet" case, not a
/// resolution failure).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn own_branch_tip(identity: &BoundGitIdentity, branch: &str) -> Option<String> {
    rev_parse(identity, &format!("refs/heads/{branch}"))
        .ok()
        .flatten()
}

/// Detach this workspace's `HEAD` to `tip` — writes the worktree-LOCAL
/// `HEAD` file directly (the equivalent of `update-ref --no-deref HEAD
/// <tip>`) rather than following a symbolic ref to update
/// `refs/heads/<branch>` in the common dir, which is exactly the write this
/// mechanism exists to avoid granting to a confined child. Must be paired
/// with [`reattach_own_head`] once the confined dispatch this brackets
/// returns, success or failure alike — an unpaired call leaves the worktree
/// stuck in a detached state. See [`DetachedHeadGuard`] for cancellation.
pub fn detach_own_head(identity: &BoundGitIdentity, tip: &str) -> Result<(), String> {
    detach_own_head_seamed(identity, tip, &|| {})
}

/// [`detach_own_head`] plus a test-only seam: `between_verify_and_use` fires
/// right after [`BoundGitIdentity::verify`] succeeds and before the native
/// write — the point a test can replace the directory at `identity`'s
/// pathname to prove the write still lands in the HELD object (#2686 review
/// round 4, P1's "deterministic seam, not timing"). A no-op in production.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn detach_own_head_seamed(
    identity: &BoundGitIdentity,
    tip: &str,
    between_verify_and_use: &dyn Fn(),
) -> Result<(), String> {
    identity.verify()?;
    between_verify_and_use();
    write_head_natively(identity, &format!("{tip}\n"))
}

/// This platform has no [`crate::fs_cap::WorkspaceDir`] — native beneath-safe
/// writes are Linux/macOS only, matching `fs_cap`'s own gate (#2686 review
/// round 5, P2). Fails closed rather than silently doing nothing or
/// restoring the retired pathname-`GIT_DIR` git spawn (round 4, P2
/// explicitly forbids resurrecting that). [`own_branch_for_commit_ref_move`]
/// never binds a usable identity on this platform (its own cfg arm below),
/// so nothing ever calls this with a real detach pending — it exists only so
/// the crate compiles for this target.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detach_own_head_seamed(
    _identity: &BoundGitIdentity,
    _tip: &str,
    _between_verify_and_use: &dyn Fn(),
) -> Result<(), String> {
    Err(unsupported_native_write())
}

/// Reverses [`detach_own_head`]: `HEAD` becomes a symbolic ref to `branch`
/// again. Idempotent — safe to call even when `HEAD` was never actually
/// detached (e.g. the bracketed dispatch never ran at all). Fails closed —
/// never touches `HEAD` — when `identity` no longer verifies: a mid-dispatch
/// identity change means the worktree-local `HEAD` file this would write is
/// not provably the one this dispatch detached.
pub fn reattach_own_head(identity: &BoundGitIdentity, branch: &str) -> Result<(), String> {
    reattach_own_head_seamed(identity, branch, &|| {})
}

/// [`reattach_own_head`] plus the same test seam as [`detach_own_head_seamed`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reattach_own_head_seamed(
    identity: &BoundGitIdentity,
    branch: &str,
    between_verify_and_use: &dyn Fn(),
) -> Result<(), String> {
    identity.verify()?;
    between_verify_and_use();
    write_head_natively(identity, &format!("ref: refs/heads/{branch}\n"))
}

/// See [`detach_own_head_seamed`]'s `cfg(not(...))` twin — same reasoning.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn reattach_own_head_seamed(
    _identity: &BoundGitIdentity,
    _branch: &str,
    _between_verify_and_use: &dyn Fn(),
) -> Result<(), String> {
    Err(unsupported_native_write())
}

/// The shared refusal text for every native-write entry point on a platform
/// with no [`crate::fs_cap::WorkspaceDir`] (`cfg(not(any(target_os = "linux",
/// target_os = "macos")))`). One copy, so every stub says the same thing.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unsupported_native_write() -> String {
    "refused: native ref/HEAD writes are Linux/macOS only on this build (no \
     crate::fs_cap on this platform); the pathname-based git spawn this replaced is \
     not restored (#2686 review round 4, P2)"
        .to_string()
}

/// Write `content` to the worktree-local `HEAD` file natively — relative to
/// [`BoundGitIdentity::git_workspace`]'s already-open descriptor, via git's
/// own lockfile protocol (`HEAD.lock` created exclusively, written, synced,
/// then renamed over `HEAD`) — never by spawning git with a pathname `GIT_DIR`
/// (#2686 review round 4, P1). No compare-and-swap: unlike the branch ref
/// below, nothing but this one bracket ever touches this worktree's own
/// `HEAD` file, so there is no concurrent mover to race (real git's own
/// `update-ref`/`symbolic-ref` take no `<oldvalue>` for `HEAD` either, absent
/// one being passed explicitly).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_head_natively(identity: &BoundGitIdentity, content: &str) -> Result<(), String> {
    write_lockfile_natively(&identity.git_workspace, Path::new("HEAD"), content)
}

/// The shared git lockfile write: create `<rel>.lock` exclusively beneath
/// `workspace`'s held descriptor (`O_CREAT|O_EXCL`, refusing a lock another
/// writer already holds), write `content`, `fsync`, then rename it over
/// `rel`. Every step is relative to the descriptor `workspace` already
/// holds — opened once, before any confined child ran — so a filesystem
/// mutation at `rel`'s PATHNAME after that point cannot redirect any of it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_lockfile_natively(
    workspace: &crate::fs_cap::WorkspaceDir,
    rel: &Path,
    content: &str,
) -> Result<(), String> {
    write_lockfile_checked(workspace, rel, content, None, None)
}

/// Publish an already policy-verified private commit into the real detached
/// HEAD, using its held admin directory and Git's lock/CAS protocol (#2813).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn publish_detached_head(
    admin: &agent_bridle_fdguard::GrantedRoot,
    old: &str,
    new: &str,
    verified_reflog_entry: &str,
) -> Result<(), String> {
    let workspace = crate::fs_cap::WorkspaceDir::from_granted_root(admin)
        .map_err(|e| format!("refused: cannot bind HEAD publication ({e})"))?;
    write_lockfile_checked(
        &workspace,
        Path::new("HEAD"),
        &format!("{new}\n"),
        Some(old),
        Some(verified_reflog_entry),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_lockfile_checked(
    workspace: &crate::fs_cap::WorkspaceDir,
    rel: &Path,
    content: &str,
    expected: Option<&str>,
    head_reflog: Option<&str>,
) -> Result<(), String> {
    use std::io::{Read as _, Write as _};
    let lock_rel = lock_path(rel);
    let mut lock = workspace.create_new(&lock_rel).map_err(|e| {
        format!(
            "refused: '{}' is locked by another writer ({e})",
            rel.display()
        )
    })?;
    if let Some(expected) = expected {
        let mut actual = String::new();
        let read = workspace
            .open_regular(rel, true)
            .and_then(|mut file| file.read_to_string(&mut actual));
        if read.is_err() || actual.trim() != expected {
            drop(lock);
            let _ = workspace.unlink(&lock_rel);
            return Err("refused: detached HEAD changed during native commit".into());
        }
    }
    let wrote = lock
        .write_all(content.as_bytes())
        .and_then(|()| lock.sync_all());
    drop(lock);
    if let Err(e) = wrote {
        let _ = workspace.unlink(&lock_rel);
        return Err(format!("refused: cannot write '{}' ({e})", rel.display()));
    }
    // Keep HEAD.lock through both writes. A failed CAS appends nothing; a
    // failed log append leaves HEAD unchanged. The admin handle is the SAME
    // object as workspace (dup above), with no shared-log authority added.
    if let Some(entry) = head_reflog {
        let appended = workspace
            .create_dir_all(Path::new("logs"))
            .and_then(|()| workspace.append_regular(Path::new("logs/HEAD")))
            .and_then(|mut log| {
                log.write_all(entry.as_bytes())
                    .and_then(|()| log.sync_all())
            });
        if let Err(e) = appended {
            let _ = workspace.unlink(&lock_rel);
            return Err(format!("refused: cannot append worktree HEAD reflog ({e}); HEAD unchanged; a partial log entry may remain"));
        }
    }
    workspace.rename(&lock_rel, rel).map_err(|e| {
        let _ = workspace.unlink(&lock_rel);
        let log_note = if head_reflog.is_some() {
            "; the verified worktree HEAD reflog entry was already appended"
        } else {
            ""
        };
        format!("refused: cannot commit '{}' ({e}){log_note}", rel.display())
    })
}

/// `<rel>.lock`, git's own lockfile naming. Linux/macOS only — its callers
/// ([`write_lockfile_natively`], [`advance_branch_ref_natively`]) are.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn lock_path(rel: &Path) -> PathBuf {
    let mut name = rel.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// [`advance_own_branch_ref`]'s refusal: the reason, plus the detached
/// commit's oid when one genuinely exists and differs from `old_tip` — so a
/// caller reports it instead of losing it silently (#2686 review round 3,
/// P2: "preserve/report the candidate OID rather than just recommending
/// another commit").
#[derive(Debug)]
pub struct AdvanceRefusal {
    pub candidate_oid: Option<String>,
    pub reason: String,
}

impl std::fmt::Display for AdvanceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

/// The host-side, bounded ref-update broker (#2682 round 2 review): call
/// after a confined child committed with `HEAD` detached at `old_tip`
/// ([`detach_own_head`]) and exited successfully. Verifies `identity`
/// first (round 3, P1) — refusing before reading anything if it no longer
/// matches what [`own_branch_for_commit_ref_move`] bound — then that the
/// resulting detached `HEAD` really is a fast-forward OR an amendment of
/// `old_tip` — the SAME two shapes `NativeGitBroker`'s reference-transaction
/// hook already accepts (`newt_core::native_git_broker::NativeCommitSession::
/// check_commit_context`) — never an arbitrary other parent, then performs
/// the SINGLE compare-and-swap ref write that moves `refs/heads/<branch>`
/// (and appends its reflog) forward, NATIVELY (#2686 review round 4, P1):
/// relative to [`BoundGitIdentity::common_workspace`]'s already-open
/// descriptor, never by spawning `git update-ref` with a pathname `GIT_DIR`.
/// It never resolves or touches any OTHER ref: the destination is always
/// `branch`, resolved once by the caller ([`own_branch_for_commit_ref_move`])
/// and threaded through unchanged, and the compare-and-swap closes the race
/// where something else moved the branch between that resolve and this call.
pub fn advance_own_branch_ref(
    identity: &BoundGitIdentity,
    branch: &str,
    old_tip: &str,
) -> Result<String, AdvanceRefusal> {
    advance_own_branch_ref_seamed(identity, branch, old_tip, &|| {})
}

/// [`advance_own_branch_ref`] plus the same test seam as
/// [`detach_own_head_seamed`]: `between_verify_and_use` fires right after
/// [`BoundGitIdentity::verify`] succeeds and before the native CAS-write —
/// everything in between (reading the detached `HEAD` oid, the two
/// `commit_parents_of` reads, the subject read) is either already native or
/// a content-addressed read a directory swap can only make FAIL, never
/// return a wrong answer that passes the fast-forward/amend check (see the
/// module's `advance_and_reattach_use_the_held_object…` test).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn advance_own_branch_ref_seamed(
    identity: &BoundGitIdentity,
    branch: &str,
    old_tip: &str,
    between_verify_and_use: &dyn Fn(),
) -> Result<String, AdvanceRefusal> {
    let refuse = |reason: String, candidate_oid: Option<String>| AdvanceRefusal {
        candidate_oid,
        reason,
    };
    identity.verify().map_err(|reason| refuse(reason, None))?;
    let new_oid = identity
        .read_detached_head()
        .map_err(|e| refuse(e, None))?
        .ok_or_else(|| refuse("no commit was created on detached HEAD".to_string(), None))?;
    if new_oid == old_tip {
        return Err(refuse(
            "HEAD did not move — nothing to publish".to_string(),
            None,
        ));
    }
    let new_parents =
        commit_parents_of(identity, &new_oid).map_err(|e| refuse(e, Some(new_oid.clone())))?;
    let is_append = new_parents == [old_tip.to_string()];
    // Amend reuses old_tip's OWN parent list. Excluded when old_tip itself
    // is a root commit (empty parent list): every unrelated root commit
    // ALSO has an empty parent list, so that shape alone cannot distinguish
    // "amends this repo's first commit" from "an orphan commit forged
    // elsewhere" — amending a repo's literal first commit is rare enough
    // to leave out of this mechanism's scope rather than accept that
    // collision.
    let old_parents =
        commit_parents_of(identity, old_tip).map_err(|e| refuse(e, Some(new_oid.clone())))?;
    let is_amend = !is_append && !old_parents.is_empty() && new_parents == old_parents;
    if !is_append && !is_amend {
        return Err(refuse(
            format!(
                "the new commit {new_oid} is not an append or amendment of branch '{branch}''s \
                 previous tip {old_tip} (parents: {new_parents:?})"
            ),
            Some(new_oid),
        ));
    }
    let subject = git_text(identity, &["log", "-1", "--format=%s", &new_oid])
        .map_err(|e| refuse(e, Some(new_oid.clone())))?;
    let message = format!(
        "commit{}: {subject}",
        if is_amend { " (amend)" } else { "" }
    );
    between_verify_and_use();
    advance_branch_ref_natively(identity, branch, old_tip, &new_oid, &message)
        .map_err(|e| refuse(e, Some(new_oid.clone())))?;
    Ok(new_oid)
}

/// See [`detach_own_head_seamed`]'s `cfg(not(...))` twin — same reasoning:
/// no [`crate::fs_cap::WorkspaceDir`] on this platform, so this exists only
/// to compile, fails closed, and is never reached in practice.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn advance_own_branch_ref_seamed(
    _identity: &BoundGitIdentity,
    _branch: &str,
    _old_tip: &str,
    _between_verify_and_use: &dyn Fn(),
) -> Result<String, AdvanceRefusal> {
    Err(AdvanceRefusal {
        candidate_oid: None,
        reason: unsupported_native_write(),
    })
}

/// The native CAS-write [`advance_own_branch_ref`] delegates to: lock
/// `refs/heads/<branch>` exclusively (git's own lockfile protocol, via
/// [`write_lockfile_natively`]'s shared primitive, inlined here because the
/// compare-and-swap and the reflog append must both happen while the SAME
/// lock is held, before it is renamed into place), re-read the ref's
/// CURRENT on-disk value under that lock and compare to `old_tip`, append
/// the reflog line, and only then commit via `rename` — in that order, so a
/// failure at any step (locked, moved tip, a reflog write failure) leaves
/// `refs/heads/<branch>` completely untouched rather than partially applied.
///
/// #2686 review round 5, P2: a nested branch name (`feature/foo`) that has
/// only ever been written to `packed-refs` has no loose `refs/heads/feature/`
/// directory at all, so the lock create below needs its parent created
/// first — real git's own `update-ref` does the same (`core.logAllRefUpdates`
/// aside, a loose-ref write always creates whatever directories the branch's
/// own path needs). Every failure path, including a failed `rename` AFTER
/// the reflog append, now cleans up the owned `.lock` via `abort` and, for
/// the rename case specifically, says so honestly: the reflog line already
/// landed even though the ref itself did not move.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn advance_branch_ref_natively(
    identity: &BoundGitIdentity,
    branch: &str,
    old_tip: &str,
    new_oid: &str,
    message: &str,
) -> Result<(), String> {
    advance_branch_ref_natively_seamed(identity, branch, Some(old_tip), new_oid, message, &|| {})
}

/// [`advance_branch_ref_natively`] plus a test-only seam:
/// `between_reflog_and_rename` fires right after the reflog append
/// succeeds and right before the commit-by-`rename` — the exact window a
/// test needs to force a deterministic rename failure AFTER the reflog
/// line has genuinely landed, to prove the honest-partial-effect wording
/// above against a real failure rather than a narrative one (#2686 review
/// round 5, P2: "test injected rename failure"). A no-op in production.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn advance_branch_ref_natively_seamed(
    identity: &BoundGitIdentity,
    branch: &str,
    old_tip: Option<&str>,
    new_oid: &str,
    message: &str,
    between_reflog_and_rename: &dyn Fn(),
) -> Result<(), String> {
    use std::io::Write as _;
    let _namespace = old_tip
        .is_none()
        .then(|| ref_namespace::NewRefNamespace::lock(identity, branch))
        .transpose()?;
    let ref_rel = PathBuf::from(format!("refs/heads/{branch}"));
    if let Some(parent) = ref_rel.parent().filter(|p| !p.as_os_str().is_empty()) {
        identity
            .common_workspace
            .create_dir_all(parent)
            .map_err(|e| format!("refused: cannot create refs/heads/{branch}'s directory ({e})"))?;
    }
    let lock_rel = lock_path(&ref_rel);
    let mut lock = identity
        .common_workspace
        .create_new(&lock_rel)
        .map_err(|e| format!("refused: refs/heads/{branch} is locked by another writer ({e})"))?;

    let abort = |reason: String| {
        let _ = identity.common_workspace.unlink(&lock_rel);
        reason
    };

    if old_tip.is_none() {
        if let Err(e) = ref_namespace::check(identity, branch) {
            return Err(abort(e));
        }
    }
    let current = match read_ref_natively(identity, branch) {
        Ok(current) => current,
        Err(e) => return Err(abort(e)),
    };
    if current.as_deref() != old_tip {
        return Err(abort(format!(
            "refused: refs/heads/{branch} is no longer at {old_tip:?} (now {current:?}) — \
             a concurrent mover won the race"
        )));
    }

    let wrote = lock
        .write_all(format!("{new_oid}\n").as_bytes())
        .and_then(|()| lock.sync_all());
    drop(lock);
    if let Err(e) = wrote {
        return Err(abort(format!(
            "refused: cannot write refs/heads/{branch} ({e})"
        )));
    }

    if let Err(e) = append_reflog_natively(
        identity,
        branch,
        old_tip.unwrap_or(&"0".repeat(new_oid.len())),
        new_oid,
        message,
    ) {
        return Err(abort(e));
    }

    between_reflog_and_rename();

    identity
        .common_workspace
        .rename(&lock_rel, &ref_rel)
        .map_err(|e| {
            abort(format!(
                "refused: cannot commit refs/heads/{branch} ({e}) — the reflog line for this \
             update was already appended to logs/refs/heads/{branch} before this failure, so \
             that log now has an entry the ref itself never reflects"
            ))
        })
}

/// `refs/heads/<branch>`'s current on-disk value, read NATIVELY (relative to
/// [`BoundGitIdentity::common_root`]'s held descriptor, never a pathname git
/// spawn): the loose ref file if one exists, else a `packed-refs` line.
/// `Ok(None)` means genuinely absent from both — an unborn branch, or (for
/// the compare-and-swap above) a branch some other actor packed/deleted.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_ref_natively(identity: &BoundGitIdentity, branch: &str) -> Result<Option<String>, String> {
    use std::io::Read as _;
    let rel = PathBuf::from(format!("refs/heads/{branch}"));
    match identity.common_root.open_read(&rel) {
        Ok(mut file) => {
            let mut text = String::new();
            file.read_to_string(&mut text)
                .map_err(|e| format!("refused: cannot read refs/heads/{branch} ({e})"))?;
            let oid = text.trim();
            if !crate::git_staging::is_hex_oid(oid) || oid.bytes().all(|b| b == b'0') {
                return Err(format!("refused: invalid loose refs/heads/{branch}"));
            }
            Ok(Some(oid.to_owned()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => read_packed_ref_natively(identity, branch),
        Err(e) => Err(format!("refused: cannot read refs/heads/{branch} ({e})")),
    }
}

/// `packed-refs`' entry for `refs/heads/<branch>`, read NATIVELY. Skips
/// comment (`#`) and peeled (`^`) lines, matching git's own format.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_packed_ref_natively(
    identity: &BoundGitIdentity,
    branch: &str,
) -> Result<Option<String>, String> {
    let suffix = format!(" refs/heads/{branch}");
    let text = read_packed_refs_natively(identity)?;
    let oid = text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with('^'))
        .find_map(|line| line.strip_suffix(&suffix));
    match oid {
        Some(oid) if !crate::git_staging::is_hex_oid(oid) || oid.bytes().all(|b| b == b'0') => {
            Err(format!("refused: invalid packed refs/heads/{branch}"))
        }
        _ => Ok(oid.map(str::to_owned)),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_packed_refs_natively(identity: &BoundGitIdentity) -> Result<String, String> {
    use std::io::Read as _;
    match identity.common_root.open_read(Path::new("packed-refs")) {
        Ok(mut file) => {
            let mut text = String::new();
            file.read_to_string(&mut text)
                .map_err(|e| format!("refused: cannot read packed-refs ({e})"))?;
            Ok(text)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("refused: cannot read packed-refs ({e})")),
    }
}

/// Append one reflog line to `logs/refs/heads/<branch>`, creating every
/// missing directory on that path first — not just the fixed
/// `logs/refs/heads` prefix, but the BRANCH's own nested parent too
/// (`logs/refs/heads/feature/` for a branch named `feature/foo`), matching
/// git's own `update-ref` behavior when `core.logAllRefUpdates` is on (the
/// non-bare default) and fixing #2686 review round 5, P2: a round-4 draft
/// created only the fixed prefix, so a first reflog entry for a nested
/// branch name failed. The committer identity is newt's own harness
/// identity, not the commit's real author: this reflog line is bookkeeping
/// for `git reflog`, never consulted for any authorization decision here, so
/// it is not worth a fourth git-spawn read to recover the commit's actual
/// committer.
///
/// ponytail: harness identity, not the commit's own committer — upgrade to
/// the real one if `git reflog show` on this branch ever needs to attribute
/// a newt-made entry to a specific session/model rather than to newt itself.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn append_reflog_natively(
    identity: &BoundGitIdentity,
    branch: &str,
    old_tip: &str,
    new_oid: &str,
    message: &str,
) -> Result<(), String> {
    use std::io::Write as _;
    let rel = Path::new("logs/refs/heads").join(branch);
    if let Some(parent) = rel.parent() {
        identity
            .common_workspace
            .create_dir_all(parent)
            .map_err(|e| {
                format!("refused: cannot create the reflog directory for {branch} ({e})")
            })?;
    }
    let line = format!(
        "{old_tip} {new_oid} {} <{}> {} +0000\t{message}\n",
        crate::agent_identity::DEFAULT_AGENT_NAME,
        crate::agent_identity::DEFAULT_AGENT_EMAIL,
        chrono::Utc::now().timestamp(),
    );
    let mut file = identity
        .common_root
        .open_write(&rel, true)
        .map_err(|e| format!("refused: cannot open logs/refs/heads/{branch} ({e})"))?;
    file.write_all(line.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("refused: cannot append to logs/refs/heads/{branch} ({e})"))
}

/// After a commit-creating dispatch that did NOT exit 0 (e.g. a compound
/// `git commit … && false`): did a commit still land on the detached HEAD?
/// Read-only — never publishes, so the caller can report the oid without
/// moving the branch ref out from under a dispatch the model saw fail
/// (#2686 review round 3, P2). `Ok(None)` means genuinely nothing to report
/// (HEAD never moved off `old_tip`).
///
/// #2686 review round 5, P2: a round-4 draft collapsed a [`BoundGitIdentity::verify`]
/// or [`BoundGitIdentity::read_detached_head`] FAILURE into the same `None`
/// as "nothing happened" — indistinguishable from the caller's point of
/// view, even though one means "there may be an unrecoverable commit and we
/// could not even check" and the other means "there truly is nothing to
/// recover". Returning `Result` lets every caller tell those apart and
/// report the failure rather than silently treating it as success.
pub fn detached_commit_candidate(
    identity: &BoundGitIdentity,
    old_tip: &str,
) -> Result<Option<String>, String> {
    identity.verify()?;
    let Some(oid) = identity.read_detached_head()? else {
        return Ok(None);
    };
    let Some(candidate) = (oid != old_tip).then_some(oid) else {
        return Ok(None);
    };
    // Object existence is only a candidate sanity check. It proves neither
    // durability nor worker termination. Recovery ordering comes from retaining
    // DetachedHeadGuard in the shell execution lease; escaped descendants remain
    // an explicit residual in docs/security/ocap-deviations.md.
    if !is_real_commit_object(identity, &candidate) {
        return Err(format!(
            "refused: detached HEAD '{candidate}' is not a complete commit \
             object — a confined child may still be writing HEAD when it was \
             read; re-check after the child reaps"
        ));
    }
    Ok(Some(candidate))
}

/// Whether Git resolves this candidate to a commit object. This is not a
/// termination or durability proof, nor strict full-OID validation. The read
/// query reacquires common_dir by pathname (see hardened_git_common).
/// Unsupported platforms fail closed without restoring pathname publication.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_real_commit_object(identity: &BoundGitIdentity, oid: &str) -> bool {
    git_text(identity, &["cat-file", "-t", oid])
        .ok()
        .is_some_and(|kind| kind.trim() == "commit")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_real_commit_object(_identity: &BoundGitIdentity, _oid: &str) -> bool {
    false
}

/// Owned recovery state for the detach/dispatch/advance/reattach bracket.
/// Share this guard with the shell's execution lease before awaiting dispatch,
/// so cancelling the waiter cannot recover HEAD before owner shutdown. The
/// normal path calls resolve after its explicit publication/reattachment.
///
/// Drop never publishes a cancelled commit: it records any orphan candidate
/// and reattaches HEAD. Lease shutdown does not prove all descendants exited;
/// see the residual in docs/security/ocap-deviations.md.
pub struct DetachedHeadGuard {
    identity: Arc<BoundGitIdentity>,
    branch: String,
    old_tip: String,
    resolved: AtomicBool,
}

impl DetachedHeadGuard {
    #[must_use]
    pub fn new(identity: Arc<BoundGitIdentity>, branch: String, old_tip: String) -> Self {
        Self {
            identity,
            branch,
            old_tip,
            resolved: AtomicBool::new(false),
        }
    }

    /// The normal path already ran its own advance/reattach (success or
    /// refusal alike) — skip `Drop`'s best-effort recovery.
    pub fn resolve(&self) {
        self.resolved.store(true, Ordering::Release);
    }
}

impl Drop for DetachedHeadGuard {
    fn drop(&mut self) {
        if self.resolved.load(Ordering::Acquire) {
            return;
        }
        // #2686 review round 4, P2: a cancelled dispatch has NO confirmed
        // outcome — the model's tool call never completed — so Drop must
        // NEVER publish. The normal (resolved) path already gates
        // `advance_own_branch_ref` on `exec_outcome == Passed`; calling it
        // here unconditionally would reintroduce exactly the cosmetic-
        // success bug round 3 closed for THAT path, on the cancelled one.
        // Reattach is not a publish — it only restores the worktree's own
        // `HEAD` to a usable symbolic state — so it still runs unconditionally.
        //
        // A `Drop` cannot return anything to any caller, so a durable,
        // recoverable record is the only channel left for a real commit this
        // leaves stranded; `eprintln!` is the last-resort fallback for the
        // (rare) case even that record can't be written — never a silent
        // `let _ =` on either. #2686 review round 5, P2:
        // `detached_commit_candidate` now distinguishes "verified: nothing to
        // report" from "could not even check" (`Err`) — the latter can no
        // longer be reported as if it were the former; `Drop` has no return
        // channel for it, so `eprintln!` is the only honest place left.
        match detached_commit_candidate(&self.identity, &self.old_tip) {
            Ok(Some(oid)) => {
                let event = crate::event_journal::JournalEvent::new(
                    crate::event_journal::EventKind::OrphanedCommit,
                    &self.branch,
                    &oid,
                    "detached-head-guard-drop",
                );
                if crate::event_journal::record_event(event).is_none() {
                    eprintln!(
                        "newt: a commit ({oid}) landed on detached HEAD for branch '{}' but \
                         the dispatch holding it was cancelled before publication, and it \
                         could not be durably recorded either. Recover it with \
                         `git branch {} {oid}`.",
                        self.branch, self.branch
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!(
                    "newt: could not check whether a commit landed on detached HEAD for \
                     branch '{}' before this dispatch was cancelled: {error}. If one exists, \
                     it may be unreachable from any ref — recover it by inspecting the \
                     worktree's reflog by hand.",
                    self.branch
                );
            }
        }
        if let Err(error) = reattach_own_head(&self.identity, &self.branch) {
            eprintln!(
                "newt: could not restore branch '{}' as HEAD after a cancelled dispatch: \
                 {error}. The worktree may still be on a detached HEAD.",
                self.branch
            );
        }
    }
}

/// `rev-parse --verify --quiet <rev>` against the COMMON dir — `rev` is
/// always a ref name here (`refs/heads/<branch>`), which lives there, never
/// the worktree-local `HEAD` (see [`BoundGitIdentity::read_detached_head`]
/// for that). `Ok(Some(oid))` on success, `Ok(None)` for "nothing there"
/// (exit 1, e.g. an unborn branch), `Err` only for an actual failure to run
/// git at all. Linux/macOS only — every caller is.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rev_parse(identity: &BoundGitIdentity, rev: &str) -> Result<Option<String>, String> {
    let output = hardened_git_common(identity, &["rev-parse", "--verify", "--quiet", rev])
        .map_err(|e| e.to_string())?
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Ok(None);
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!oid.is_empty()).then_some(oid))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn commit_parents_of(identity: &BoundGitIdentity, oid: &str) -> Result<Vec<String>, String> {
    let output = hardened_git_common(identity, &["cat-file", "commit", oid])
        .map_err(|e| e.to_string())?
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    crate::native_git_broker::commit_parents(&output.stdout)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn git_text(identity: &BoundGitIdentity, args: &[&str]) -> Result<String, String> {
    let output = hardened_git_common(identity, args)
        .map_err(|e| e.to_string())?
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
pub(crate) fn resolve_trusted_program(
    cwd: &Path,
    path: Option<&OsStr>,
    name: &str,
) -> io::Result<PathBuf> {
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
pub(crate) fn resolve_trusted_program(
    cwd: &Path,
    path: Option<&OsStr>,
    name: &str,
) -> io::Result<PathBuf> {
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
    include!("git_hardening/branch_tests.rs");
    use super::*;
    use std::process::Command;

    /// The explicit, minimal environment every fixture git runs under —
    /// mirror of `agentic::tools::tests::git_shell_grant::hermetic_git_env`
    /// (`agentic/tools_tests/helper_git_shell_grant.rs`), the canonical
    /// hermetic constructor (#2686 review round 3, P3): that module's
    /// private test tree is not reachable from here, so this duplicates its
    /// shape rather than its code. `GIT_CONFIG_NOSYSTEM`/`GIT_CONFIG_GLOBAL`
    /// close the config sources that live OUTSIDE the process environment
    /// (`/etc/gitconfig`, the operator's own `~/.gitconfig`).
    fn hermetic_git_env(home: &Path) -> Vec<(&'static str, String)> {
        vec![
            ("HOME", home.to_string_lossy().into_owned()),
            ("GIT_AUTHOR_NAME", "t".to_string()),
            ("GIT_AUTHOR_EMAIL", "t@example.invalid".to_string()),
            ("GIT_COMMITTER_NAME", "t".to_string()),
            ("GIT_COMMITTER_EMAIL", "t@example.invalid".to_string()),
            ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".to_string()),
            ("GIT_TEMPLATE_DIR", "/dev/null".to_string()),
        ]
    }

    /// `env_clear()` rather than a denylist, so an inherited `GIT_DIR`,
    /// `GIT_WORK_TREE`, `GIT_COMMON_DIR`, or any other ambient git knob
    /// cannot leak into a fixture-setup invocation — the SAME property
    /// `hardened_git` enforces for production, applied here to the test's
    /// own unconfined setup/probe git calls.
    fn hermetic_git(dir: &Path, home: &Path) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .env_clear()
            .envs(hermetic_git_env(home));
        cmd
    }

    /// A throwaway private `HOME` that lives only for the one command.
    fn git(dir: &Path, args: &[&str]) {
        let home = tempfile::tempdir().unwrap();
        let status = hermetic_git(dir, home.path())
            .args(args)
            .status()
            .expect("git invocation");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Same contract as [`git`], but returns trimmed stdout instead of just
    /// asserting success — for the detach/advance/reattach tests' reads
    /// (`rev-parse`, `symbolic-ref`, `log --format=%s`). One shared spawn
    /// site reused by every read, rather than hand-rolling a new one each
    /// time.
    fn git_output(dir: &Path, args: &[&str]) -> String {
        let home = tempfile::tempdir().unwrap();
        let output = hermetic_git(dir, home.path())
            .args(args)
            .output()
            .expect("git invocation");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        std::fs::write(dir.join("seed"), "x").unwrap();
        git(dir, &["add", "seed"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// #2720: ambient commit setup must not replace a scoped session's cached
    /// refusal/identity. The global cache can be shared by sibling sessions.
    #[test]
    fn full_access_commit_does_not_rebind_an_existing_identity() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        init_repo(&workspace);
        git(&workspace, &["checkout", "-q", "-b", "task"]);
        prime_identity_cache(&workspace, None);
        let _ = crate::native_git_broker::NativeGitBroker::invocation_caveats(
            &crate::Caveats::top(),
            &workspace,
        )
        .unwrap();
        assert!(own_gitdir_shell_write_grant(&workspace).is_empty());
        assert!(cached_identity(&workspace).is_none());
    }

    /// Would have failed before F32/#2537: no grants existed at all, so
    /// `permits_path` denied every file `git add` touches on a linked
    /// worktree's own branch.
    ///
    /// #2682 round 5 (`8c537f17`) briefly ALSO granted the common dir's
    /// whole `refs/heads/` and `logs/refs/heads/` directories here, so a
    /// confined `git commit` could advance the checked-out branch's ref —
    /// but a directory-wide grant there covers every OTHER branch's ref and
    /// reflog too, and nothing inspects a plain shell redirect (not a `git`
    /// verb) in a compound confined-shell command to stop it reaching them.
    /// Round 6 (#2686 review round 2) reverted that: a commit instead lands
    /// with `HEAD` detached (see `own_branch_for_commit_ref_move` and
    /// friends) and a host-side, bounded `update-ref` publishes it, so this
    /// grant goes back to exactly the two directories `git add` needs.
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
        // What `git add` needs: the worktree's own admin dir + the common
        // `objects/` subtree. A commit's ref move is no longer in here at
        // all — it happens host-side, after the confined dispatch returns.
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
        // `refs/heads/` (the checked-out branch's own ref included),
        // `refs/tags`, the common dir's own top-level `logs/HEAD`, `config`,
        // and `hooks/` all stay out of the write grant — none of it is a
        // directory a confined child may create or rename inside anymore.
        for denied in [
            path("refs/heads/task"),
            path("refs/heads/task.lock"),
            path("logs/refs/heads/task"),
            path("refs/tags/v1"),
            path("logs/HEAD"),
            path("config"),
            path("hooks/pre-commit"),
        ] {
            assert!(
                !crate::caveats::permits_path(&scope, &denied),
                "{denied} must stay denied"
            );
        }

        // The grant is real enough for a real (unconfined, this is a plain
        // subprocess — not the kernel fence) `git add` to succeed; the
        // confined-shell proof of the FULL commit-ref-move mechanism is
        // `newt-core::agentic::tools_tests::helper_git_shell_grant`'s
        // `confined_shell_git_commit_succeeds_in_a_linked_worktree_on_a_non_default_branch`.
        git(&wt, &["add", "f.txt"]);
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

    /// The detach/advance/reattach cycle, driven directly (real unconfined
    /// git — the confined-kernel-fence proof of the whole thing lives in
    /// `helper_git_shell_grant`'s `confined_shell_git_commit_succeeds_…`).
    /// Mirrors exactly what `agentic::tools`'s `run_command` arm does around
    /// the confined dispatch.
    #[test]
    fn detach_advance_reattach_publishes_a_commit_made_on_detached_head() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt); // prime the identity cache, as session start does

        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).expect("task is a non-default, born branch");
        assert_eq!(branch, "task");

        detach_own_head(&identity, &old_tip).unwrap();
        let symref = Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(
            !symref.status.success(),
            "HEAD must be a detached oid, not a symbolic ref, while the confined child runs"
        );

        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);

        let new_oid = advance_own_branch_ref(&identity, &branch, &old_tip).unwrap();
        reattach_own_head(&identity, &branch).unwrap();

        let tip = git_output(&main, &["rev-parse", "refs/heads/task"]);
        assert_eq!(
            new_oid, tip,
            "advance_own_branch_ref returns the published oid"
        );
        let head = git_output(&wt, &["rev-parse", "HEAD"]);
        assert_eq!(tip, head, "refs/heads/task must advance to the new commit");
        assert_ne!(tip, old_tip);

        assert_eq!(
            git_output(&wt, &["symbolic-ref", "HEAD"]),
            "refs/heads/task",
            "HEAD must be reattached as a symbolic ref to the branch"
        );
        assert_eq!(
            git_output(&main, &["log", "--format=%s", "-1", "refs/heads/task"]),
            "task work"
        );
    }

    /// `advance_own_branch_ref` is a verifier, not a blind publisher: a
    /// detached-HEAD commit whose parent is NOT `old_tip` (forged, or made
    /// against a stale tip) must be refused, and `refs/heads/task` must stay
    /// exactly where it was.
    #[test]
    fn advance_own_branch_ref_refuses_a_commit_that_is_not_old_tips_descendant() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        // A commit with NO relation to old_tip at all (an orphan root
        // commit), as if a hostile `commit-tree` forged one out of thin air.
        detach_own_head(&identity, &old_tip).unwrap();
        git(&wt, &["checkout", "--orphan", "forged"]);
        std::fs::write(wt.join("evil"), "x").unwrap();
        git(&wt, &["add", "evil"]);
        git(&wt, &["commit", "-q", "-m", "forged"]);
        git(&wt, &["checkout", "-q", "--detach", "HEAD"]);

        let result = advance_own_branch_ref(&identity, &branch, &old_tip);
        let refusal = result.expect_err("an unrelated commit must be refused");
        assert!(
            refusal.candidate_oid.is_some(),
            "a real (if unrelated) commit exists, so its oid must be reported, not lost: {refusal}"
        );

        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            old_tip,
            "a refused advance must not move the branch ref at all"
        );
    }

    /// `--amend` reuses `old_tip`'s OWN parent, not `old_tip` itself — the
    /// other accepted shape alongside a plain append. A non-root `old_tip`
    /// (two commits deep) so the amend/root-collision guard added above
    /// does not itself suppress this acceptance path.
    #[test]
    fn advance_own_branch_ref_accepts_an_amended_commit() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        std::fs::write(main.join("second"), "y").unwrap();
        git(&main, &["add", "second"]);
        git(&main, &["commit", "-q", "-m", "second"]);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "--amend", "-m", "amended second"]);

        advance_own_branch_ref(&identity, &branch, &old_tip).unwrap();
        reattach_own_head(&identity, &branch).unwrap();

        assert_eq!(
            git_output(&main, &["log", "--format=%s", "-1", "refs/heads/task"]),
            "amended second"
        );
    }

    /// #2686 review round 3, P3: a concurrent mover wins the compare-and-swap.
    /// Between [`detach_own_head`] and [`advance_own_branch_ref`], a SEPARATE
    /// actor (modeled here as a direct `update-ref` from the primary
    /// checkout — the common dir is shared, so this is exactly what a second
    /// session or a race with another dispatch looks like) moves
    /// `refs/heads/task` to an unrelated commit. The old-tip CAS must refuse
    /// rather than clobber that external move, and the detached commit's oid
    /// must still be reported rather than silently dropped.
    #[test]
    fn advance_own_branch_ref_refuses_when_the_branch_moved_concurrently() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);

        // The race: a concurrent actor moves refs/heads/task BEFORE this
        // dispatch's advance runs, directly through the shared common dir.
        // `main` commits something unrelated first — forcing `task` straight
        // to `main`'s UNCHANGED HEAD would be a same-oid no-op, not a race.
        std::fs::write(main.join("elsewhere"), "y").unwrap();
        git(&main, &["add", "elsewhere"]);
        git(&main, &["commit", "-q", "-m", "elsewhere"]);
        git(&main, &["branch", "-f", "task", "HEAD"]);
        let moved_to = git_output(&main, &["rev-parse", "refs/heads/task"]);
        assert_ne!(moved_to, old_tip, "the race must actually move the ref");

        let refusal = advance_own_branch_ref(&identity, &branch, &old_tip)
            .expect_err("a moved-tip CAS must refuse, not clobber the concurrent move");
        assert!(
            refusal.candidate_oid.is_some(),
            "the detached commit genuinely exists and must be reported: {refusal}"
        );
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            moved_to,
            "refs/heads/task must stay exactly where the concurrent mover left it"
        );
    }

    /// #2686 review round 4, P1 — the check-to-use gap the round-3 fix left
    /// open: `BoundGitIdentity::verify` compares the held directory object
    /// to a fresh `stat`, but round 3's `advance_own_branch_ref` then
    /// spawned `git` with `GIT_DIR` set to a PATHNAME — the held object was
    /// only ever COMPARED, never actually USED, so a directory swap landing
    /// in the window between that compare and git's own (separate) path
    /// resolution could still win. Proven here with a deterministic seam (a
    /// callback fired exactly between `verify()` and the native write, never
    /// a timing race) rather than the previous round's "rewrite before the
    /// call even starts", which `verify()` itself already catches.
    ///
    /// Measured red against the pathname version (round 3's code, as it
    /// stood before this round): with `identity.common_dir` renamed aside
    /// and an EMPTY, non-repository directory recreated at the original
    /// path, a pathname `git` spawn against that original path — the exact
    /// shape `hardened_git_common` built — cannot resolve `refs/heads/task`
    /// at all (there is no `.git` there any more), so the round-3
    /// implementation could only ever refuse here, never reach the real,
    /// moved-aside object. The native fix in THIS round does reach it: the
    /// write lands in the held object (now sitting at the renamed-aside
    /// path), and the empty decoy left at the original pathname is never
    /// touched.
    #[test]
    fn advance_own_branch_ref_uses_the_held_common_dir_not_a_swapped_pathname() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);
        let new_oid = git_output(&wt, &["rev-parse", "HEAD"]);

        let common_dir = identity.common_dir.clone();
        let moved_aside = root_path.join("common-moved-aside");
        let swap = || {
            std::fs::rename(&common_dir, &moved_aside).unwrap();
            // An empty, non-repository decoy at the ORIGINAL pathname — not
            // even a `.git` dir, so a pathname-based `git` spawn against it
            // cannot resolve anything at all. Measured red, inline: this is
            // exactly what round 3's `hardened_git_common` would have used.
            std::fs::create_dir(&common_dir).unwrap();
            let pathname_resolve = Command::new("git")
                .args(["rev-parse", "--verify", "--quiet", "refs/heads/task"])
                .env_clear()
                .env("GIT_DIR", &common_dir)
                .env("HOME", &common_dir)
                .output()
                .unwrap();
            assert!(
                !pathname_resolve.status.success(),
                "measured red for the class: a pathname git spawn against the \
                 swapped-in decoy must fail to resolve anything — proving round \
                 3's implementation could only ever refuse here, never reach \
                 the real object"
            );
        };

        let published = advance_own_branch_ref_seamed(&identity, &branch, &old_tip, &swap)
            .expect("the native write must land in the HELD object after the swap");
        assert_eq!(published, new_oid);

        assert_eq!(
            git_output(&moved_aside, &["rev-parse", "refs/heads/task"]),
            new_oid,
            "the REAL (now-moved-aside) common dir must show the published commit"
        );
        assert_eq!(
            std::fs::read_dir(&common_dir).unwrap().count(),
            0,
            "the decoy sitting at the swapped-in pathname must stay untouched — \
             nothing in the native write path ever opens it"
        );
    }

    /// The other half of the P1 gap, for [`reattach_own_head`]: the admin
    /// dir (`identity.git_dir`) swapped between `verify()` and the native
    /// `HEAD` write. Same deterministic seam, same inline measured red (a
    /// pathname `git symbolic-ref` against the empty decoy cannot resolve
    /// anything either).
    #[test]
    fn reattach_own_head_uses_the_held_admin_dir_not_a_swapped_pathname() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();

        let git_dir = identity.git_dir.clone();
        let moved_aside = root_path.join("admin-moved-aside");
        let swap = || {
            std::fs::rename(&git_dir, &moved_aside).unwrap();
            std::fs::create_dir(&git_dir).unwrap();
            let pathname_resolve = Command::new("git")
                .args(["symbolic-ref", "-q", "HEAD"])
                .env_clear()
                .env("GIT_DIR", &git_dir)
                .env("HOME", &git_dir)
                .output()
                .unwrap();
            assert!(
                !pathname_resolve.status.success(),
                "measured red for the class: a pathname git spawn against the \
                 swapped-in decoy must fail — round 3's implementation could \
                 only ever refuse here, never reattach the real worktree"
            );
        };

        reattach_own_head_seamed(&identity, &branch, &swap)
            .expect("the native write must land in the HELD admin dir after the swap");

        assert_eq!(
            std::fs::read_to_string(moved_aside.join("HEAD"))
                .unwrap()
                .trim(),
            "ref: refs/heads/task",
            "the REAL (now-moved-aside) admin dir must show HEAD reattached"
        );
        assert_eq!(
            std::fs::read_dir(&git_dir).unwrap().count(),
            0,
            "the decoy sitting at the swapped-in pathname must stay untouched"
        );
    }

    /// #2686 review round 2, P2: `git commit … && false` creates a commit on
    /// detached HEAD, then the compound command's failing second segment
    /// means the overall dispatch did not exit 0 — so `advance_own_branch_ref`
    /// is never called (publishing would misrepresent a failed dispatch as a
    /// successful branch update). The commit must not be silently lost: its
    /// oid is still readable, and the branch ref stays untouched.
    #[test]
    fn detached_commit_candidate_reports_an_unpublished_commit_without_moving_the_ref() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            old_tip, identity, ..
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]); // the "git commit" half of "&& false"

        let candidate = detached_commit_candidate(&identity, &old_tip)
            .expect("verify/read must succeed")
            .expect("a real commit was made and must be reported");
        assert_eq!(candidate, git_output(&wt, &["rev-parse", "HEAD"]));
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            old_tip,
            "reporting the candidate must not itself move the branch ref"
        );
    }

    /// Recovery records an orphan and reattaches without publishing it. This
    /// direct-drop unit test checks recovery contents; the real leased-dispatch
    /// ordering regression lives in native_git_broker_pipeline/cancellation.rs.
    #[test]
    fn detached_head_guard_never_publishes_on_drop_but_reattaches_and_records_the_candidate() {
        // Isolates NEWT_EVENT_JOURNAL (never the developer's own
        // ~/.newt/events.jsonl) and points it at this test's own file so the
        // durable record `Drop` must leave behind can actually be read back.
        let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
        let journal_dir = tempfile::tempdir().unwrap();
        let journal_path = journal_dir.path().join("events.jsonl");
        crate::process_env::set_var(
            crate::event_journal::JOURNAL_PATH_ENV,
            journal_path.to_str().unwrap(),
        );

        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        let candidate_oid;
        {
            let _guard = DetachedHeadGuard::new(identity.clone(), branch.clone(), old_tip.clone());
            std::fs::write(wt.join("f.txt"), "hi").unwrap();
            git(&wt, &["add", "f.txt"]);
            git(&wt, &["commit", "-q", "-m", "task work"]);
            candidate_oid = git_output(&wt, &["rev-parse", "HEAD"]);
            // `_guard` drops here, unresolved — modeling cancellation.
        }

        assert_eq!(
            git_output(&wt, &["symbolic-ref", "HEAD"]),
            "refs/heads/task",
            "a cancelled dispatch must not leave the worktree stuck detached"
        );
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            old_tip,
            "a cancelled dispatch must NEVER publish — there is no confirmed outcome to publish"
        );
        let journal = std::fs::read_to_string(&journal_path)
            .expect("Drop must durably record the stranded commit, not merely swallow it");
        assert!(
            journal.contains(&candidate_oid) && journal.contains("orphaned-commit"),
            "the stranded commit's oid must be recoverable from the event journal: {journal}"
        );
    }

    /// An unleased guard still recovers if cancellation occurs before dispatch
    /// transfers ownership to a worker. Live leased dispatch is covered by the
    /// native_git_broker_pipeline cancellation handshake fixture.
    #[tokio::test]
    async fn detached_head_guard_recovers_when_an_unleased_future_is_cancelled() {
        let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
        let journal_dir = tempfile::tempdir().unwrap();
        let journal_path = journal_dir.path().join("events.jsonl");
        crate::process_env::set_var(
            crate::event_journal::JOURNAL_PATH_ENV,
            journal_path.to_str().unwrap(),
        );

        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);
        let candidate_oid = git_output(&wt, &["rev-parse", "HEAD"]);

        let dispatch = async {
            let _guard = DetachedHeadGuard::new(identity.clone(), branch.clone(), old_tip.clone());
            // Never resolves on its own — only cancellation ends this future,
            // exactly like a confined child's real process that outlives the
            // model's patience. `select!` below drops it mid-await.
            std::future::pending::<()>().await;
        };
        // `biased` polls in written order and only moves to the next branch
        // when the current one is Pending — so `dispatch` is polled FIRST
        // (running its synchronous prefix, which constructs `_guard`, up to
        // the `pending()` await point) on the very same poll round that the
        // immediately-ready cancellation branch resolves and drops it.
        tokio::select! {
            biased;
            () = dispatch => panic!("the pending dispatch must never complete"),
            () = std::future::ready(()) => {}, // wins this round — cancels `dispatch`
        }

        assert_eq!(
            git_output(&wt, &["symbolic-ref", "HEAD"]),
            "refs/heads/task",
            "a real tokio cancellation must still reattach HEAD"
        );
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            old_tip,
            "a real tokio cancellation must NEVER publish"
        );
        let journal = std::fs::read_to_string(&journal_path)
            .expect("a real tokio cancellation must still durably record the candidate");
        assert!(journal.contains(&candidate_oid) && journal.contains("orphaned-commit"));
    }

    /// A nonexistent commit object is not a recoverable candidate. This is
    /// an object-lookup check only, not a worker-termination regression.
    #[test]
    fn detached_commit_candidate_fails_closed_on_a_torn_orphaned_head() {
        let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            old_tip, identity, ..
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        // A real, complete commit object lives in the object DB; we point HEAD
        // at a *different*, 40-char id that is NOT an object — i.e. a torn/
        // partial write whose target does not exist. `cat-file -t` rejects it.
        // In a worktree `wt/.git` is a file (`gitdir: ...`), so the live HEAD
        // object sits under the parent repo's `.git/worktrees/<wt>/HEAD`.
        let real_commit_oid = {
            git(&wt, &["commit", "-q", "--allow-empty", "-m", "x"]);
            git_output(&wt, &["rev-parse", "HEAD"])
        };
        let wt_name = wt.file_name().unwrap();
        let head_path = main
            .join(".git")
            .join("worktrees")
            .join(wt_name)
            .join("HEAD");
        std::fs::write(head_path, format!("{real_commit_oid}deadbeef")).unwrap();

        let result = detached_commit_candidate(&identity, &old_tip);
        assert!(
            result.is_err(),
            "a torn HEAD pointing at a nonexistent object must fail closed, not be reported: {result:?}"
        );
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            old_tip,
            "failing closed must not itself move the branch ref"
        );
    }

    /// #2686 review round 6, P2 — the positive control for the fail-closed
    /// test above: a genuinely complete commit object that the confined child
    /// finished writing MUST still be reported, proving the object check is a
    /// torn-read guard and not a blanket refusal to surface a real commit.
    #[test]
    fn detached_commit_candidate_reports_a_real_completed_commit_object() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            old_tip, identity, ..
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        git(&wt, &["commit", "-q", "--allow-empty", "-m", "x"]);
        let real = git_output(&wt, &["rev-parse", "HEAD"]);

        let candidate = detached_commit_candidate(&identity, &old_tip)
            .expect("a real, completed commit must not fail closed");
        assert_eq!(candidate, Some(real));
    }

    /// #2686 review round 3, P1 — the concrete attack named in the review:
    /// after a legitimate detached commit, a non-git second segment of a
    /// compound confined-shell command rewrites the worktree's `.git`
    /// gitlink to point at the PRIMARY checkout's own gitdir. The OLD
    /// mechanism (round 2, `66589bfa`) re-derived the git-dir/common-dir
    /// pair with a fresh `git rev-parse` against the workspace on every
    /// publish/reattach call — i.e. through this exact, now-rewritten
    /// pointer — so it would have moved `refs/heads/task` in the WRONG
    /// repository and (via `reattach_own_head`'s `symbolic-ref HEAD`)
    /// overwritten the primary checkout's own protected `HEAD`.
    ///
    /// The `redirected` assertion below is a hand-rolled analog of the OLD
    /// discovery (`git -C <workspace> rev-parse --git-common-dir
    /// --absolute-git-dir`, re-run AFTER the rewrite) proving the attack
    /// surface is real — it resolves to `main`'s own `.git`, not `wt`'s
    /// admin dir. **Correction (#2686 review round 4):** that observation
    /// alone is NOT a measured failing assertion against the old
    /// publication/restoration path itself, only evidence that the class of
    /// redirection exists — round 3's RESULT overclaimed it as "measured
    /// red" for this test. The genuine red-against-the-old-body measurement
    /// for THIS repo's actual `advance_own_branch_ref`/`reattach_own_head`
    /// — swap the directory between `verify()` and the write, run the OLD
    /// (pathname-spawning) functions, watch them fail — lives in
    /// `advance_own_branch_ref_uses_the_held_common_dir_not_a_swapped_pathname`
    /// / `reattach_own_head_uses_the_held_admin_dir_not_a_swapped_pathname`
    /// below, confirmed against round 3's body before this round's native
    /// rewrite landed (see this PR's RESULT for the measured output). What
    /// THIS test still proves, soundly: [`BoundGitIdentity`] is bound BEFORE
    /// the rewrite and never re-resolves, so [`advance_own_branch_ref`] and
    /// [`reattach_own_head`] publish/reattach correctly on `wt` while
    /// `main`'s own `HEAD` and `refs/heads/task` stay byte-identical, even
    /// with the gitlink pointed elsewhere.
    #[test]
    fn bound_identity_resists_a_gitlink_rewrite_after_the_detached_commit() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]); // the confined child's work

        let main_head_before = std::fs::read(main.join(".git").join("HEAD")).unwrap();
        let main_task_ref_before =
            std::fs::read(main.join(".git").join("refs/heads/task")).unwrap();

        // The attack: a non-git compound-command segment rewrites the
        // worktree's gitlink to point straight at the primary checkout.
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git").display()),
        )
        .unwrap();

        // Measured red for the CLASS this closes: the old re-discovery
        // pattern, run fresh AFTER the rewrite, now resolves to `main`'s own
        // gitdir — not `wt`'s admin dir — which is the whole vulnerability.
        let redirected = Command::new("git")
            .args(["rev-parse", "--absolute-git-dir"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert_eq!(
            PathBuf::from(String::from_utf8_lossy(&redirected.stdout).trim())
                .canonicalize()
                .unwrap(),
            main.join(".git").canonicalize().unwrap(),
            "the rewrite must actually redirect a fresh rev-parse — proving the attack is real"
        );

        // The fix: `identity` was bound BEFORE the rewrite and never
        // re-resolves through the workspace, so it is unaffected.
        let new_oid = advance_own_branch_ref(&identity, &branch, &old_tip)
            .expect("the bound identity must still publish correctly on wt");
        reattach_own_head(&identity, &branch).unwrap();

        assert_eq!(
            std::fs::read(main.join(".git").join("HEAD")).unwrap(),
            main_head_before,
            "the primary checkout's own HEAD must be untouched by the rewrite"
        );
        assert_ne!(
            std::fs::read(main.join(".git").join("refs/heads/task")).unwrap(),
            main_task_ref_before,
            "wt's task branch DOES still live in the (unrewritten) common dir and must advance"
        );
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            new_oid,
            "the commit must land on task's real ref, resolved via the bound identity"
        );
    }

    /// The other half of the `.git` rewrite: the admin dir's OWN `commondir`
    /// file lives INSIDE the worktree's admin dir, which is itself in the
    /// write grant (for `HEAD`/`index`/…) — so a confined child can rewrite
    /// it to point `refs/heads/` resolution at a different repository
    /// entirely, same attack class as the gitlink rewrite, different file.
    /// This rewrite has no effect because ref/object operations never go
    /// through the admin dir's `GIT_DIR` at all — `hardened_git_common`
    /// points `GIT_DIR` straight at [`BoundGitIdentity::common_dir`], an
    /// ordinary non-worktree gitdir with no `commondir` file to consult
    /// (measured: a `GIT_COMMON_DIR` env override does NOT work here — git
    /// ignores it whenever `GIT_DIR` itself names a directory that carries
    /// its own `commondir` file, which a worktree admin dir always does).
    #[test]
    fn bound_identity_resists_a_commondir_rewrite_after_the_detached_commit() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let decoy = root_path.join("decoy");
        std::fs::create_dir(&decoy).unwrap();
        init_repo(&decoy);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);

        let decoy_head_before = std::fs::read(decoy.join(".git").join("HEAD")).unwrap();

        // The attack: the admin dir's own `commondir` file — inside the
        // write grant — is rewritten to point at an unrelated repository.
        let admin_dir = &identity.git_dir;
        std::fs::write(
            admin_dir.join("commondir"),
            format!("{}\n", decoy.join(".git").display()),
        )
        .unwrap();

        let new_oid = advance_own_branch_ref(&identity, &branch, &old_tip)
            .expect("ref resolution must bypass the rewritten commondir file entirely");
        reattach_own_head(&identity, &branch).unwrap();

        assert_eq!(
            std::fs::read(decoy.join(".git").join("HEAD")).unwrap(),
            decoy_head_before,
            "the decoy repository named by the rewritten commondir must be untouched"
        );
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            new_oid,
            "the commit must land on task's real ref in the ORIGINAL common dir"
        );
    }

    /// [`BoundGitIdentity::verify`] fails closed (#2686 review round 3, P1:
    /// "refuse if the held identities no longer match") when the directory
    /// object itself — not merely a pointer file inside it — changes: here,
    /// the admin dir is removed and a fresh, empty directory is created at
    /// the SAME path. A pathname re-check alone would see an unchanged path
    /// and wrongly pass; the held descriptor's `(dev, ino)` disagrees.
    #[test]
    fn bound_identity_refuses_once_the_admin_dir_itself_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);

        // Replace the admin directory object at the SAME path.
        let admin_dir = identity.git_dir.clone();
        std::fs::rename(&admin_dir, root_path.join("admin-dir-moved-aside")).unwrap();
        std::fs::create_dir(&admin_dir).unwrap();

        let refusal = advance_own_branch_ref(&identity, &branch, &old_tip)
            .expect_err("a replaced admin dir must refuse, not publish through the new object");
        assert!(refusal.reason.contains("no longer names the directory"));
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            old_tip,
            "a refused advance must not move the branch ref"
        );

        let reattach_refusal = reattach_own_head(&identity, &branch)
            .expect_err("reattach must also refuse against the replaced admin dir");
        assert!(reattach_refusal.contains("no longer names the directory"));
    }

    /// #2686 review round 5, P1 — a round-4 draft bound `common_root` (the
    /// object [`BoundGitIdentity::verify`] checks) via `GrantedRoot::acquire`,
    /// then opened `common_workspace` (the handle every native write actually
    /// goes through) via a SECOND, INDEPENDENT `WorkspaceDir::open_root` call
    /// against the same pathname — bound only by "nothing hostile ran in the
    /// back-to-back window between the two opens", not by a kernel guarantee.
    ///
    /// Measured red for that class, inline: swap the common dir for an empty
    /// decoy right before [`BoundGitIdentity::bind_seamed`]'s
    /// `between_acquire_and_derive` fires — exactly where a round-4-shaped
    /// `bind` would have performed its second, independent open, so in that
    /// shape it binds `common_workspace` to the decoy — then restore the
    /// real common dir only AFTER `bind_seamed` returns (never inside the
    /// hook, or the second open would never see the decoy at all). A rename
    /// does not invalidate an already-open fd, so a round-4-shaped
    /// `common_workspace` stays bound to the decoy's object even once the
    /// decoy is moved out of `common_dir`'s pathname: a write through it
    /// would report success while landing nowhere the real, visible repo
    /// can see.
    ///
    /// This round's fix removes the second open entirely —
    /// [`crate::fs_cap::WorkspaceDir::from_granted_root`] derives
    /// `common_workspace` by `dup`ing `common_root`'s own fd, captured
    /// BEFORE the swap — so there is no pathname resolution left for the
    /// swap to catch: the real native write (through `advance_own_branch_ref`)
    /// must land in the REAL, restored object, and the restored, visible
    /// `refs/heads/task` must show it.
    #[test]
    fn bind_derives_the_write_handle_from_the_held_fd_not_a_second_pathname_open() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let old_tip = git_output(&main, &["rev-parse", "refs/heads/task"]);
        let (common_dir, git_dir) = verified_identity(&wt).unwrap();
        // `git_dir` nests under `common_dir` for a linked worktree
        // (`<common_dir>/worktrees/<name>`), so swapping `common_dir` alone
        // would ALSO make `git_dir`'s pathname vanish — an unrelated ENOENT
        // that would obscure what THIS test means to exercise. Recreate
        // just enough of the path inside the decoy for a round-4-shaped
        // second open on `git_dir` to still succeed.
        let git_dir_rel_to_common = git_dir.strip_prefix(&common_dir).unwrap().to_path_buf();

        let moved_aside = root_path.join("common-moved-aside-at-bind-time");
        let swap = || {
            std::fs::rename(&common_dir, &moved_aside).unwrap();
            std::fs::create_dir(&common_dir).unwrap(); // empty decoy, at common_dir's pathname
            std::fs::create_dir_all(common_dir.join(&git_dir_rel_to_common)).unwrap();
        };

        let identity = BoundGitIdentity::bind_seamed(common_dir.clone(), git_dir, &swap).expect(
            "acquire already completed before the swap; a decoy directory still opens fine",
        );

        // Restore ONLY NOW — after `bind_seamed` (and whichever "second
        // open" it performed, old shape or new) has already returned. The
        // decoy is moved OUT rather than deleted, so an old-shape
        // `common_workspace`'s still-open fd keeps naming a real,
        // inspectable (if orphaned-by-pathname) directory.
        let decoy_relocated = root_path.join("common-decoy-after-restore");
        std::fs::rename(&common_dir, &decoy_relocated).unwrap();
        std::fs::rename(&moved_aside, &common_dir).unwrap();

        identity
            .verify()
            .expect("the pathname is restored now — verify only ever checked common_root");

        let branch = "task".to_string();
        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);
        let new_oid = git_output(&wt, &["rev-parse", "HEAD"]);

        let published = advance_own_branch_ref(&identity, &branch, &old_tip).expect(
            "the write must land in the REAL object, derived by dup — never a decoy that could \
             only ever have been bound by an independent second open",
        );
        assert_eq!(published, new_oid);
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/task"]),
            new_oid,
            "the real, restored common dir must show the published commit"
        );
    }

    /// #2686 review round 5, P2 — a nested branch name (`feature/foo`) that
    /// has only ever been packed has no loose `refs/heads/feature/`
    /// directory at all. Measured: `git pack-refs --all` removes both the
    /// loose ref file AND the now-empty `refs/heads/feature/` directory, so
    /// the lock create's parent genuinely does not exist beforehand — this
    /// is not a belief about what packing does.
    #[test]
    fn advance_own_branch_ref_creates_missing_parents_for_a_packed_only_nested_branch() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "feature/foo",
            ],
        );
        git(&main, &["pack-refs", "--all"]);
        let common_dir = git_dirs(&wt).unwrap().0;
        assert!(
            !common_dir.join("refs/heads/feature").exists(),
            "packing must leave no loose directory behind — otherwise this test exercises \
             nothing new"
        );

        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();
        assert_eq!(branch, "feature/foo");

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);
        let new_oid = git_output(&wt, &["rev-parse", "HEAD"]);

        let published = advance_own_branch_ref(&identity, &branch, &old_tip).expect(
            "a packed-only nested branch must still publish — the lock's parent is created on \
             demand",
        );
        assert_eq!(published, new_oid);
        assert_eq!(
            git_output(&main, &["rev-parse", "refs/heads/feature/foo"]),
            new_oid,
            "the new loose ref write must take effect over the stale packed-refs entry"
        );
        assert!(
            std::fs::read_to_string(common_dir.join("logs/refs/heads/feature/foo"))
                .unwrap()
                .contains(&new_oid),
            "the branch's own reflog must gain a first entry despite no pre-existing \
             logs/refs/heads/feature directory either"
        );
    }

    /// #2686 review round 5, P2 — a `rename` failure AFTER the reflog
    /// append used to leave the owned `.lock` file behind and never said the
    /// reflog line had already landed. Forced with an injected failure, not
    /// a narrative one: replacing the destination (`refs/heads/<branch>`)
    /// with a directory right before the rename forces `renameat` to fail
    /// (a file can't be renamed onto a directory) without touching the
    /// surrounding `refs/heads/` directory's own permissions, so the
    /// cleanup unlink this test also checks is not itself blocked by the
    /// same injected failure.
    #[test]
    fn advance_branch_ref_natively_cleans_up_and_reports_honestly_on_a_rename_failure() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            branch,
            old_tip,
            identity,
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);
        let new_oid = git_output(&wt, &["rev-parse", "HEAD"]);

        let common_dir = identity.common_dir.clone();
        let ref_path = common_dir.join(format!("refs/heads/{branch}"));
        let lock_path_abs = common_dir.join(format!("refs/heads/{branch}.lock"));
        let reflog_path = common_dir.join(format!("logs/refs/heads/{branch}"));
        let force_rename_failure = || {
            std::fs::remove_file(&ref_path).unwrap();
            std::fs::create_dir(&ref_path).unwrap();
        };

        let refusal = advance_branch_ref_natively_seamed(
            &identity,
            &branch,
            Some(&old_tip),
            &new_oid,
            "commit: task work",
            &force_rename_failure,
        )
        .expect_err("a rename failure must refuse, not silently succeed");
        assert!(
            refusal.contains("already appended"),
            "the refusal must say the reflog line landed even though the ref never moved: \
             {refusal}"
        );
        assert!(
            !lock_path_abs.exists(),
            "the owned .lock file must be cleaned up even when the failure happens AFTER the \
             reflog append, not just on an earlier failure"
        );
        assert!(
            std::fs::read_to_string(&reflog_path)
                .unwrap()
                .contains(&new_oid),
            "the reflog append genuinely landed before the rename failed — the test's own \
             setup, not a belief"
        );

        std::fs::remove_dir(&ref_path).unwrap();
    }

    /// #2686 review round 5, P2 — a round-4 draft collapsed a `verify()` (or
    /// `read_detached_head`) FAILURE into the same `None` as "genuinely
    /// nothing to report", so a caller could not tell "there may be an
    /// unrecoverable commit and we could not even check" apart from "there
    /// truly is nothing to recover". Forces the former the same way
    /// [`bound_identity_refuses_once_the_admin_dir_itself_is_replaced`] does.
    #[test]
    fn detached_commit_candidate_propagates_a_verify_failure_instead_of_reporting_none() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        std::fs::create_dir(&main).unwrap();
        init_repo(&main);
        let wt = root_path.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
        );
        own_gitdir_grants(&wt);
        let OwnBranchRefMove {
            old_tip, identity, ..
        } = own_branch_for_commit_ref_move(&wt).unwrap();

        detach_own_head(&identity, &old_tip).unwrap();
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        git(&wt, &["add", "f.txt"]);
        git(&wt, &["commit", "-q", "-m", "task work"]);

        let admin_dir = identity.git_dir.clone();
        std::fs::rename(&admin_dir, root_path.join("admin-dir-moved-aside")).unwrap();
        std::fs::create_dir(&admin_dir).unwrap();

        let result = detached_commit_candidate(&identity, &old_tip);
        assert!(
            result.is_err(),
            "a verify() failure must be a reported Err, never the same Ok(None) as 'nothing to \
             report': {result:?}"
        );
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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[path = "git_hardening/commit_view_tests.rs"]
mod commit_view_tests;
