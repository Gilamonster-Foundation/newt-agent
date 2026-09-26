//! `newt-git` — the embedded git engine for newt agents.
//!
//! Wraps [`grit-lib`](https://crates.io/crates/grit-lib) (a pure-Rust, from-scratch
//! git reimplementation, MIT) behind the [`GitCaveats`](newt_core::git_caveats::GitCaveats)
//! OCAP surface. Every operation takes the already-composed `&GitCaveats` and **fails
//! closed**; results are this crate's **own** structured serde models, converted at the
//! grit-lib boundary — so grit-lib's pre-1.0 API churn is contained to this one crate
//! and never leaks into the rest of the workspace.
//!
//! **Scope: LOCAL ops** — reads (`open`/`status`/`log`/`diff`) and writes
//! (`add`/`commit`/`branch`), each gated by the matching `GitCaveats` axis and
//! fail-closed without it. Network ops (`clone`/`fetch`/`push`) are deferred (PR5) —
//! fail-closed under the OCAP deviation ratchet, riding the SSH transport. We depend
//! ONLY on the MIT `grit-lib`, never the GPL-2.0 `grit-legacy`.

use newt_core::caveats::{Caveats, Scope};
use newt_core::git_caveats::GitCaveats;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use grit_lib::diff::{
    count_changes, diff_index_to_tree, diff_index_to_worktree_with_options, diff_tree_to_worktree,
    diff_trees, DiffEntry, DiffIndexToWorktreeOptions, DiffStatus,
};
use grit_lib::index::{entry_from_stat, Index, IndexEntry, MODE_REGULAR};
use grit_lib::merge_base::resolve_commit_specs;
use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::pathspec::matches_pathspec_list;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::stash::apply_stash;
use grit_lib::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use grit_lib::reflog::{delete_reflog_entries, read_reflog};
use grit_lib::refs::{
    append_reflog, delete_ref, read_head, reflog_file_path, resolve_ref, write_ref,
    write_symbolic_ref,
};
use grit_lib::repo::Repository;
use grit_lib::rev_list::{rev_list, RevListOptions};
use grit_lib::rev_parse::{
    split_double_dot_range, split_triple_dot_range, try_parse_double_dot_log_range,
};
use grit_lib::state::{resolve_head, HeadState};
use grit_lib::write_tree::write_tree_from_index;

/// Errors from the embedded git engine.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The [`GitCaveats`] surface denied this operation class (fail-closed).
    #[error("capability denied: git {0} not permitted")]
    Denied(&'static str),
    /// The underlying grit-lib engine failed.
    #[error("git: {0}")]
    Engine(#[from] grit_lib::error::Error),
    /// A filesystem read of a worktree file failed (during staging).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The operation isn't supported in this repository state (e.g. bare repo,
    /// detached/unborn HEAD).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// The operation was refused for a runtime reason that carries a dynamic
    /// message — e.g. deleting the current branch, or switching to a branch at a
    /// different commit (no working-tree updater here). No side effects.
    #[error("{0}")]
    Refused(String),
    /// A rebase step produced a merge conflict; the rebase was aborted and the
    /// branch ref was left untouched (no side effects).
    #[error("rebase conflict at {0} — aborted, branch unchanged")]
    Conflict(String),
    /// The rebase plan was malformed (e.g. a squash before any pick).
    #[error("bad rebase plan: {0}")]
    BadPlan(String),
}

/// What a [rebase](GitEngine::rebase) step does with its commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseAction {
    /// Replay the commit as-is.
    Pick,
    /// Replay, but with a new message.
    Reword,
    /// Fold into the previous commit, KEEPING both messages.
    Squash,
    /// Fold into the previous commit, DISCARDING this message.
    Fixup,
    /// Skip the commit entirely.
    Drop,
}

/// One entry in a structured rebase plan.
#[derive(Debug, Clone)]
pub struct RebaseStep {
    /// The commit to act on (id / ref / short oid — resolved at run time).
    pub commit: String,
    pub action: RebaseAction,
    /// New / extra message for `Reword` and `Squash`.
    pub message: Option<String>,
}

/// Outcome of a [rebase](GitEngine::rebase).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseReport {
    /// Short oid of the new branch tip.
    pub new_head: String,
    /// Commits produced on the rebased segment.
    pub produced: usize,
    /// Steps dropped.
    pub dropped: usize,
}

/// Commit authorship — supplied by the caller (e.g. from the agent identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    pub email: String,
}

impl Author {
    /// A git ident line stamped at the current time, UTC: `"Name <email> <secs> +0000"`.
    fn ident_now(&self) -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{} <{}> {} +0000", self.name, self.email, secs)
    }
}

/// A single changed path. `status` is git's status letter (`M`/`A`/`D`/`R`/`C`/…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub status: char,
    pub path: String,
}

/// Working-tree status — the porcelain facts, no presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    /// Current branch short name, or `None` when detached/unborn.
    pub branch: Option<String>,
    /// Short HEAD oid, or `None` on an unborn HEAD.
    pub head: Option<String>,
    /// Staged changes (index vs HEAD tree).
    pub staged: Vec<FileChange>,
    /// Unstaged changes (worktree vs index).
    pub unstaged: Vec<FileChange>,
    /// Untracked paths.
    pub untracked: Vec<String>,
    /// True iff nothing is staged, unstaged, or untracked.
    pub clean: bool,
}

/// Cheap repository identity at one observation edge.
///
/// Unlike [`StatusReport`], this does not scan the index or worktree, and the
/// commit id is the complete object id rather than a presentation prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadSnapshot {
    /// Current branch short name, or `None` when detached/unborn.
    pub branch: Option<String>,
    /// Complete HEAD object id, or `None` on an unborn HEAD.
    pub head: Option<String>,
}

/// One commit's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitInfo {
    pub id: String,
    pub short_id: String,
    pub author_name: String,
    pub author_email: String,
    /// Author time, unix seconds.
    pub timestamp: i64,
    pub summary: String,
    pub parents: Vec<String>,
}

/// The set of files a diff touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffReport {
    pub files: Vec<FileChange>,
    /// Present only when the caller asked for `--stat`: added/removed line
    /// counts per changed file, in the same order as `files`.
    pub stat: Option<Vec<FileStat>>,
}

/// One file's `--stat` line-change counts. Binary files are counted as 0/0,
/// matching git's "Bin" case rather than fabricating a text line count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    pub insertions: usize,
    pub deletions: usize,
}

/// Which diff to compute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSpec {
    /// Unstaged: worktree vs index.
    Worktree,
    /// Staged: index vs HEAD tree.
    Staged,
    /// One revision vs the worktree (`git diff <rev>`). May itself be an
    /// `A..B` range spec, which is equivalent to `RevRange(A, B)`.
    Rev(String),
    /// Two revisions (`git diff A B` / `git diff A..B`).
    RevRange(String, String),
}

/// An embedded git engine bound to one repository.
pub struct GitEngine {
    repo: Repository,
    /// Signs every commit, amend and rebase commit this engine writes
    /// ([`GitEngine::write_commit`]); `None` writes them unsigned.
    signer: Option<std::sync::Arc<dyn newt_core::commit_signing::CommitSigner>>,
}

/// Refuse a ref-move onto the repository's default branch (F32/#2537: newt may
/// never move `main`/`master`/the remote's default, whatever a caller's
/// filesystem grants are). The ONE checkpoint every mutating op that advances a
/// branch ref (`commit`, `amend`, `rebase`) routes through, so the guard cannot
/// be bypassed by reaching one of them through a different path.
///
/// `branch_ref` is the target ref a caller is about to rewrite (`refs/heads/…`,
/// as returned by [`read_head`]). Anything other than `refs/heads/main` or
/// `refs/heads/master` also checks the shared repo's `refs/remotes/origin/HEAD`
/// symbolic target (best-effort — an offline or remote-less repo just falls
/// back to the hardcoded names, same as `newt_core::git_hardening::own_gitdir_grants`).
///
/// `ref_already_exists` is `false` only for the ONE case the exemption exists
/// for: a truly fresh repository with NO refs anywhere at all (see
/// [`repository_has_no_refs`]) — the `git` tool's own `init` op always names
/// the new branch `main`, #461's advertised "commit in a fresh,
/// not-yet-a-repo workspace" flow. F32 protects an EXISTING default branch's
/// history from being retargeted, not the act of creating one in a repo that
/// has nothing yet.
///
/// PR #2577 round 4, Blocker 2: this used to be sourced from `HEAD`'s own
/// `head_oid().is_some()` — "is THIS branch unborn" — which an (until this
/// round unguarded) `branch-delete main` could flip back to `false` by
/// deleting `main`'s only ref, reopening the exemption for a commit that
/// creates a brand-new `main` as a root commit, discarding the deleted
/// branch's history with the guard never firing. Every call site now derives
/// `ref_already_exists` from `repository_has_no_refs`, which asks "does
/// ANYTHING in this repo have a ref" rather than "does this ONE ref" — a
/// `branch-delete main` next to a surviving `task` branch cannot reopen the
/// exemption, because `task` still has a ref.
fn refuse_if_default_branch(
    git_dir: &Path,
    branch_ref: &str,
    ref_already_exists: bool,
) -> Result<(), GitError> {
    if !ref_already_exists {
        return Ok(());
    }
    let Some(name) = branch_ref.strip_prefix("refs/heads/") else {
        return Ok(());
    };
    if name == "main" || name == "master" {
        return Err(GitError::Refused(format!(
            "refusing to move the default branch '{name}' (F32/#2537) — commit on a feature branch instead"
        )));
    }
    let common = grit_lib::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    if let Ok(raw) = std::fs::read_to_string(common.join("refs/remotes/origin/HEAD")) {
        if let Some(default) = raw.trim().strip_prefix("ref: refs/remotes/origin/") {
            if default == name {
                return Err(GitError::Refused(format!(
                    "refusing to move the default branch '{name}' (F32/#2537) — commit on a feature branch instead"
                )));
            }
        }
    }
    Ok(())
}

/// Does this repository have NO refs anywhere — no loose ref under
/// `refs/heads` (recursively, since a branch name may contain `/`) and no
/// `refs/heads/…` line in `packed-refs`? The narrow "creating the default
/// branch is fine" exemption in [`refuse_if_default_branch`] is meant for
/// exactly this state (a fresh `git init`), not for "this ONE branch happens
/// to be unborn" — the latter is reachable by deleting an existing default
/// branch's ref while sibling branches survive (Blocker 2).
fn repository_has_no_refs(git_dir: &Path) -> bool {
    let common = grit_lib::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    if directory_has_any_file(&common.join("refs/heads")) {
        return false;
    }
    match std::fs::read_to_string(common.join("packed-refs")) {
        Ok(contents) => !contents.lines().any(|line| line.contains("refs/heads/")),
        Err(_) => true, // no packed-refs file at all
    }
}

fn directory_has_any_file(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if directory_has_any_file(&path) {
                return true;
            }
        } else {
            return true;
        }
    }
    false
}

impl GitEngine {
    /// Discover and open the repository containing `root` (walks up for `.git`).
    /// Legacy discovery/config/ODB reads require unrestricted read authority;
    /// the separate refs-only branch listing does not open this engine.
    pub fn open(root: &Path, read_scope: &Scope<String>) -> Result<Self, GitError> {
        newt_core::agentic::check_git_read_scope("open", read_scope)
            .map_err(GitError::Unsupported)?;
        let repo = Repository::discover(Some(root))?;
        Ok(Self { repo, signer: None })
    }

    fn head_oid(&self) -> Result<Option<ObjectId>, GitError> {
        Ok(match resolve_head(&self.repo.git_dir)? {
            HeadState::Branch { oid, .. } => oid,
            HeadState::Detached { oid } => Some(oid),
            HeadState::Invalid => None,
        })
    }

    /// Index vs worktree, hashing "racily clean" entries (mtime not older
    /// than the index file's) instead of trusting their cached stat, as git
    /// does. Without it a same-size edit made in the index write's timestamp
    /// tick reads as clean, and `rebase`/`checkout` would overwrite it. (An edit
    /// landing between this check and their reset is still lost, as in git.)
    fn worktree_changes(&self, index: &Index, wt: &Path) -> Result<Vec<DiffEntry>, GitError> {
        // The same resolver `load_index` uses, so `GIT_INDEX_FILE` is honoured.
        let index_mtime = self
            .repo
            .index_path_for_env()
            .ok()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs() as u32, d.subsec_nanos()));
        let options = DiffIndexToWorktreeOptions {
            index_mtime,
            ..DiffIndexToWorktreeOptions::default()
        };
        Ok(diff_index_to_worktree_with_options(
            &self.repo.odb,
            index,
            wt,
            options,
        )?)
    }

    fn head_tree(&self) -> Result<Option<ObjectId>, GitError> {
        match self.head_oid()? {
            Some(oid) => {
                let obj = self.repo.odb.read(&oid)?;
                Ok(Some(parse_commit(&obj.data)?.tree))
            }
            None => Ok(None),
        }
    }

    /// Read only the current branch and complete HEAD object id.
    ///
    /// This is the small observation payload used by provenance hooks; it
    /// deliberately avoids the O(worktree) status scan. Requires `read` just
    /// like every other repository observation.
    pub fn head_snapshot(&self, caps: &GitCaveats) -> Result<HeadSnapshot, GitError> {
        if !caps.permits_read() {
            return Err(GitError::Denied("read"));
        }
        let (branch, head) = match resolve_head(&self.repo.git_dir)? {
            HeadState::Branch {
                short_name, oid, ..
            } => (Some(short_name), oid.map(|oid| oid.to_hex())),
            HeadState::Detached { oid } => (None, Some(oid.to_hex())),
            HeadState::Invalid => (None, None),
        };
        Ok(HeadSnapshot { branch, head })
    }

    /// Read the full commit message of the current HEAD commit (the message an
    /// `amend` with no new message would preserve). Requires `read` like every
    /// other repository observation. Returns the empty string for an unborn
    /// HEAD (no commit yet) — the caller (the `amend` arm) treats that as "no
    /// message to re-finalize" because `GitEngine::amend` itself refuses an
    /// unborn HEAD.
    pub fn head_message(&self, caps: &GitCaveats) -> Result<String, GitError> {
        if !caps.permits_read() {
            return Err(GitError::Denied("read"));
        }
        let Some(oid) = self.head_oid()? else {
            return Ok(String::new());
        };
        let commit = parse_commit(&self.repo.odb.read(&oid)?.data)?;
        Ok(commit.message)
    }

    /// `git status` — requires the `read` capability.
    pub fn status(&self, caps: &GitCaveats) -> Result<StatusReport, GitError> {
        if !caps.permits_read() {
            return Err(GitError::Denied("read"));
        }
        let index = self.repo.load_index()?;
        let (branch, head) = match resolve_head(&self.repo.git_dir)? {
            HeadState::Branch {
                short_name, oid, ..
            } => (Some(short_name), oid.as_ref().map(short_oid)),
            HeadState::Detached { oid } => (None, Some(short_oid(&oid))),
            HeadState::Invalid => (None, None),
        };
        let tree = self.head_tree()?;
        let staged = diff_index_to_tree(&self.repo.odb, &index, tree.as_ref(), false)?;
        let (unstaged, untracked) = match self.repo.work_tree.clone() {
            Some(wt) => {
                let unstaged = self.worktree_changes(&index, &wt)?;
                let untracked = collect_untracked_and_ignored(
                    &self.repo,
                    &index,
                    &wt,
                    IgnoredMode::No,
                    false,
                    &[],
                )?
                .0;
                (unstaged, untracked)
            }
            None => (Vec::new(), Vec::new()),
        };
        let staged: Vec<FileChange> = staged.iter().map(file_change).collect();
        let unstaged: Vec<FileChange> = unstaged.iter().map(file_change).collect();
        let clean = staged.is_empty() && unstaged.is_empty() && untracked.is_empty();
        Ok(StatusReport {
            branch,
            head,
            staged,
            unstaged,
            untracked,
            clean,
        })
    }

    /// `git log`, up to `limit` commits. Requires `read`.
    ///
    /// `revision` selects the walk: `None` starts at HEAD; a single rev starts
    /// there instead; an `A..B` range walks from `B`, excluding everything
    /// reachable from `A` — real reachability, via grit-lib's `rev_list`, not
    /// a first-parent-chain scan (a branch cut from `main` and then advanced
    /// past its fork point is walked correctly; the left side need not be a
    /// first-parent ancestor of the right). `paths` (when non-empty) keeps
    /// only commits whose tree differs from all their parents' at a matching
    /// path (grit-lib's own history simplification, matching real `git log
    /// -- <path>` — not limited to the first-parent diff).
    pub fn log(
        &self,
        caps: &GitCaveats,
        limit: usize,
        revision: Option<&str>,
        paths: &[String],
    ) -> Result<Vec<CommitInfo>, GitError> {
        if !caps.permits_read() {
            return Err(GitError::Denied("read"));
        }
        let (positive, negative) = match revision {
            None => (vec!["HEAD".to_string()], Vec::new()),
            Some(rev) => match split_double_dot_range(rev) {
                Some((left, right)) => {
                    let left = if left.is_empty() { "HEAD" } else { left };
                    let right = if right.is_empty() { "HEAD" } else { right };
                    (vec![right.to_string()], vec![left.to_string()])
                }
                None => {
                    self.reject_symmetric_or_ambiguous(rev)?;
                    (vec![rev.to_string()], Vec::new())
                }
            },
        };
        let options = RevListOptions {
            max_count: Some(limit),
            paths: paths.to_vec(),
            ..Default::default()
        };
        let result = rev_list(&self.repo, &positive, &negative, &options)?;
        result
            .commits
            .iter()
            .map(|oid| {
                let obj = self.repo.odb.read(oid)?;
                let commit = parse_commit(&obj.data)?;
                Ok(commit_info(oid, &commit))
            })
            .collect()
    }

    /// `git diff` for the given `spec`. Requires `read`. `paths` (when
    /// non-empty) filters the result to matching pathspecs; `stat` computes
    /// per-file `--stat` insertion/deletion counts.
    pub fn diff(
        &self,
        caps: &GitCaveats,
        spec: DiffSpec,
        paths: &[String],
        stat: bool,
    ) -> Result<DiffReport, GitError> {
        if !caps.permits_read() {
            return Err(GitError::Denied("read"));
        }
        let index = self.repo.load_index()?;
        let mut entries = match spec {
            DiffSpec::Worktree => match self.repo.work_tree.clone() {
                Some(wt) => self.worktree_changes(&index, &wt)?,
                None => Vec::new(),
            },
            DiffSpec::Staged => {
                let tree = self.head_tree()?;
                diff_index_to_tree(&self.repo.odb, &index, tree.as_ref(), false)?
            }
            DiffSpec::Rev(rev) => match try_parse_double_dot_log_range(&self.repo, &rev)? {
                Some((a, b)) => self.diff_rev_range(&a.to_hex(), &b.to_hex())?,
                None => {
                    self.reject_symmetric_or_ambiguous(&rev)?;
                    let oid = self.resolve_one(&rev)?;
                    let tree = self.commit_tree(&oid)?;
                    match self.repo.work_tree.clone() {
                        Some(wt) => {
                            diff_tree_to_worktree(&self.repo.odb, Some(&tree), &wt, &index)?
                        }
                        None => Vec::new(),
                    }
                }
            },
            DiffSpec::RevRange(a, b) => self.diff_rev_range(&a, &b)?,
        };
        if !paths.is_empty() {
            entries.retain(|e| matches_pathspec_list(e.path(), paths));
        }
        let stat = if stat {
            Some(self.diff_stat(&entries)?)
        } else {
            None
        };
        Ok(DiffReport {
            files: entries.iter().map(file_change).collect(),
            stat,
        })
    }

    /// Tree-vs-tree diff between two revision specs.
    fn diff_rev_range(&self, a: &str, b: &str) -> Result<Vec<DiffEntry>, GitError> {
        let oid_a = self.resolve_one(a)?;
        let oid_b = self.resolve_one(b)?;
        let tree_a = self.commit_tree(&oid_a)?;
        let tree_b = self.commit_tree(&oid_b)?;
        Ok(diff_trees(
            &self.repo.odb,
            Some(&tree_a),
            Some(&tree_b),
            "",
        )?)
    }

    /// `--stat` counts per changed file. Reads both blob sides (missing side
    /// treated as empty, matching Added/Deleted); binary files count 0/0.
    fn diff_stat(&self, entries: &[DiffEntry]) -> Result<Vec<FileStat>, GitError> {
        let zero = grit_lib::diff::zero_oid();
        entries
            .iter()
            .map(|e| {
                let old = if e.old_oid == zero {
                    Vec::new()
                } else {
                    self.repo.odb.read(&e.old_oid)?.data
                };
                let new = if e.new_oid == zero {
                    Vec::new()
                } else {
                    self.repo.odb.read(&e.new_oid)?.data
                };
                let (insertions, deletions) = if grit_lib::merge_file::is_binary(&old)
                    || grit_lib::merge_file::is_binary(&new)
                {
                    (0, 0)
                } else {
                    count_changes(
                        &String::from_utf8_lossy(&old),
                        &String::from_utf8_lossy(&new),
                    )
                };
                Ok(FileStat {
                    path: e.path().to_string(),
                    insertions,
                    deletions,
                })
            })
            .collect()
    }

    /// `git add` — stage worktree files into the index. Requires `stage`.
    /// Each `path` is repository-relative. Returns the paths actually staged.
    pub fn add(&self, caps: &GitCaveats, paths: &[String]) -> Result<Vec<String>, GitError> {
        if !caps.permits_stage() {
            return Err(GitError::Denied("stage"));
        }
        let wt = self
            .repo
            .work_tree
            .clone()
            .ok_or(GitError::Unsupported("cannot stage in a bare repository"))?;
        let mut index = self.repo.load_index()?;
        let mut staged = Vec::with_capacity(paths.len());
        for rel in paths {
            let abs = wt.join(rel);
            let bytes = std::fs::read(&abs)?;
            let oid = self.repo.odb.write(ObjectKind::Blob, &bytes)?;
            let size = bytes.len() as u32;
            let path = rel.as_bytes().to_vec();
            // The stat fields are left zero (a benign "needs refresh" to git); the
            // blob oid is authoritative for diffs. `flags` carries the name length.
            let flags = path.len().min(0x0FFF) as u16;
            index.add_or_replace(IndexEntry {
                ctime_sec: 0,
                ctime_nsec: 0,
                mtime_sec: 0,
                mtime_nsec: 0,
                dev: 0,
                ino: 0,
                mode: MODE_REGULAR,
                uid: 0,
                gid: 0,
                size,
                oid,
                flags,
                flags_extended: None,
                path,
                base_index_pos: 0,
            });
            staged.push(rel.clone());
        }
        self.repo.write_index(&mut index)?;
        Ok(staged)
    }

    /// `git commit` — build a tree from the index, write the commit, and advance the
    /// current branch. Requires `commit_local`. Errors on detached HEAD.
    pub fn commit(
        &self,
        caps: &GitCaveats,
        message: &str,
        author: &Author,
    ) -> Result<CommitInfo, GitError> {
        if !caps.permits_commit() {
            return Err(GitError::Denied("commit"));
        }
        let branch_ref = read_head(&self.repo.git_dir)?;
        let head_oid = self.head_oid()?;
        if let Some(branch_ref) = &branch_ref {
            refuse_if_default_branch(
                &self.repo.git_dir,
                branch_ref,
                !repository_has_no_refs(&self.repo.git_dir),
            )?;
        }
        let index = self.repo.load_index()?;
        let tree = write_tree_from_index(&self.repo.odb, &index, "")?;
        let parents: Vec<ObjectId> = head_oid.into_iter().collect();
        let ident = author.ident_now();
        let commit = CommitData {
            tree,
            parents,
            author: ident.clone(),
            committer: ident,
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: message.to_string(),
            raw_message: None,
        };
        let oid = self.write_commit(&commit)?;
        match branch_ref {
            Some(branch_ref) => write_ref(&self.repo.git_dir, &branch_ref, &oid)?,
            None => return Err(GitError::Unsupported("cannot commit on a detached HEAD")),
        }
        Ok(commit_info(&oid, &commit))
    }

    /// `git commit --amend` — replace HEAD with a new commit carrying the
    /// current index tree, keeping HEAD's PARENTS (not HEAD itself). `message`
    /// `None` reuses HEAD's existing message (amend-to-add-files); `Some` rewords
    /// it. Requires `commit_local`. Errors on an unborn / detached HEAD.
    pub fn amend(
        &self,
        caps: &GitCaveats,
        message: Option<&str>,
        author: &Author,
    ) -> Result<CommitInfo, GitError> {
        if !caps.permits_commit() {
            return Err(GitError::Denied("commit"));
        }
        let branch_ref = read_head(&self.repo.git_dir)?;
        // Amend always rewrites an existing commit, so `repository_has_no_refs`
        // is unconditionally false here — computed via the shared helper
        // anyway, for one source of truth (round 4).
        if let Some(branch_ref) = &branch_ref {
            refuse_if_default_branch(
                &self.repo.git_dir,
                branch_ref,
                !repository_has_no_refs(&self.repo.git_dir),
            )?;
        }
        let head = self
            .head_oid()?
            .ok_or(GitError::Unsupported("nothing to amend (unborn HEAD)"))?;
        let head_commit = parse_commit(&self.repo.odb.read(&head)?.data)?;
        let index = self.repo.load_index()?;
        let tree = write_tree_from_index(&self.repo.odb, &index, "")?;
        let ident = author.ident_now();
        let commit = CommitData {
            tree,
            // The defining difference from `commit`: keep HEAD's parents so the
            // amended commit replaces HEAD rather than stacking on top of it.
            parents: head_commit.parents.clone(),
            author: ident.clone(),
            committer: ident,
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: message.map(str::to_string).unwrap_or(head_commit.message),
            raw_message: None,
        };
        let oid = self.write_commit(&commit)?;
        match branch_ref {
            Some(branch_ref) => write_ref(&self.repo.git_dir, &branch_ref, &oid)?,
            None => return Err(GitError::Unsupported("cannot amend on a detached HEAD")),
        }
        Ok(commit_info(&oid, &commit))
    }

    /// Refuse a `log`/`diff` operand this engine cannot answer honestly: an
    /// `A...B` symmetric-diff (no merge-base support here — real git resolves
    /// it to a merge base, which this engine does not compute for this path)
    /// or a bare `spec` that is simultaneously a resolvable revision AND an
    /// existing worktree path (the same shape real git refuses without an
    /// explicit `--` to disambiguate; answering the revision silently would
    /// hide that the caller may have meant the path).
    fn reject_symmetric_or_ambiguous(&self, spec: &str) -> Result<(), GitError> {
        if split_triple_dot_range(spec).is_some() {
            return Err(GitError::Refused(
                "symmetric ranges are not supported — put -- before paths, or use A..B".to_string(),
            ));
        }
        let is_path = self
            .repo
            .work_tree
            .as_ref()
            .is_some_and(|wt| wt.join(spec).exists());
        if is_path && self.resolve_one(spec).is_ok() {
            return Err(GitError::Refused(format!(
                "ambiguous argument '{spec}': both revision and filename — put -- before paths"
            )));
        }
        Ok(())
    }

    /// Resolve a commit spec (short oid / ref / id) to an `ObjectId`.
    fn resolve_one(&self, spec: &str) -> Result<ObjectId, GitError> {
        resolve_commit_specs(&self.repo, &[spec.to_string()])?
            .into_iter()
            .next()
            .ok_or(GitError::Unsupported("could not resolve commit"))
    }

    fn commit_tree(&self, oid: &ObjectId) -> Result<ObjectId, GitError> {
        Ok(parse_commit(&self.repo.odb.read(oid)?.data)?.tree)
    }

    /// Write `commit` to the object database, signed when this engine has a
    /// signer. The one write path for `commit`, `amend` and rebase, so no
    /// commit from them can go out unsigned once the operator asked for
    /// signatures. A signing failure writes nothing.
    fn write_commit(&self, commit: &CommitData) -> Result<ObjectId, GitError> {
        let mut bytes = serialize_commit(commit);
        if let Some(signer) = &self.signer {
            let signature = signer
                .sign(&bytes)
                .map_err(|e| GitError::Refused(format!("commit signing failed: {e}")))?;
            bytes = newt_core::commit_signing::with_signature(&bytes, &signature);
        }
        Ok(self.repo.odb.write(ObjectKind::Commit, &bytes)?)
    }

    /// Write a single-parent commit with the agent's identity; returns its oid.
    fn write_commit_on(
        &self,
        parent: ObjectId,
        tree: ObjectId,
        message: &str,
        author: &Author,
    ) -> Result<ObjectId, GitError> {
        let ident = author.ident_now();
        let commit = CommitData {
            tree,
            parents: vec![parent],
            author: ident.clone(),
            committer: ident,
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: message.to_string(),
            raw_message: None,
        };
        self.write_commit(&commit)
    }

    /// Structured-plan rebase: replay `steps` (in order) onto `onto`, applying
    /// pick / reword / squash / fixup / drop. All new trees and commits are
    /// written to the ODB; the branch ref is advanced **only at the very end**,
    /// so a conflict (or any error) aborts with the branch unchanged — no
    /// working-tree, index, or ref side effects. Requires `commit_local`.
    ///
    /// Cherry-pick per step is a 3-way tree merge with `MergeFavor::None` so
    /// real conflicts are reported (not silently resolved). Authorship on the
    /// produced commits is the agent's (the typical case: rewriting its own
    /// recent history). Root commits (no parent) cannot be replayed.
    pub fn rebase(
        &self,
        caps: &GitCaveats,
        onto: &str,
        steps: &[RebaseStep],
        author: &Author,
        // #1709 req 9: an optional commit-message finalizer applied to EVERY
        // newly created commit's joined message (pick / reword / squash), so
        // even an ordinary `pick` — which replays the original commit's
        // message verbatim — receives canonical Newt attribution. The
        // finalizer is the SAME one `commit`/`amend` use
        // (`LocalGitTool::finalize_commit_message`), so no rebase path
        // formats attribution itself. `None` (test scaffolds with no
        // attribution) leaves messages untouched.
        finalize: Option<&dyn Fn(&str) -> String>,
    ) -> Result<RebaseReport, GitError> {
        if !caps.permits_commit() {
            return Err(GitError::Denied("commit"));
        }
        // #2485: a ref-moving op must leave the working tree == HEAD, or
        // refuse. Require a clean tree up front so the `checkout_between_trees`
        // reset below (old HEAD tree → new tip tree) never discards
        // uncommitted work — the same clean-tree precondition `stash` relies
        // on for its own tree reset.
        let pre_status = self.status(caps)?;
        if !pre_status.clean {
            return Err(GitError::Refused(format!(
                "rebase needs a clean working tree ({} staged, {} unstaged, {} untracked); commit or stash first",
                pre_status.staged.len(),
                pre_status.unstaged.len(),
                pre_status.untracked.len()
            )));
        }
        let head_ref = read_head(&self.repo.git_dir)?
            .ok_or(GitError::Unsupported("cannot rebase on a detached HEAD"))?;
        // Rebase always replays onto existing history — same shared helper
        // as `commit`/`amend` (round 4), one source of truth.
        refuse_if_default_branch(
            &self.repo.git_dir,
            &head_ref,
            !repository_has_no_refs(&self.repo.git_dir),
        )?;
        let old_head_tree = self.head_tree()?;
        let onto_oid = self.resolve_one(onto)?;

        // The commit currently being assembled (a `pick`/`reword` opens it;
        // `squash`/`fixup` extend it; the next pick or the end closes it).
        let mut tip = onto_oid;
        let mut tip_tree = self.commit_tree(&onto_oid)?;
        let mut open = false;
        let mut cur_parent = onto_oid;
        let mut cur_tree = tip_tree;
        let mut cur_msgs: Vec<String> = Vec::new();
        let mut produced = 0usize;
        let mut dropped = 0usize;

        for step in steps {
            if step.action == RebaseAction::Drop {
                dropped += 1;
                continue;
            }
            let c = self.resolve_one(&step.commit)?;
            let cc = parse_commit(&self.repo.odb.read(&c)?.data)?;
            let parent = cc
                .parents
                .first()
                .copied()
                .ok_or(GitError::Unsupported("cannot rebase a root commit"))?;
            let base_tree = self.commit_tree(&parent)?;
            let ours = if open { cur_tree } else { tip_tree };
            let merged = merge_trees_three_way(
                &self.repo,
                base_tree,
                ours,
                cc.tree,
                MergeFavor::None,
                WhitespaceMergeOptions::default(),
                None,
                TreeMergeConflictPresentation::default(),
            )?;
            if !merged.conflict_content.is_empty() {
                let subj = cc.message.lines().next().unwrap_or("").trim();
                return Err(GitError::Conflict(format!("{} ({subj})", short_oid(&c))));
            }
            let new_tree = write_tree_from_index(&self.repo.odb, &merged.index, "")?;

            match step.action {
                RebaseAction::Pick | RebaseAction::Reword => {
                    // Close any open commit first.
                    if open {
                        let msg = cur_msgs.join("\n\n");
                        // #1709 req 9: finalize EVERY newly created commit's
                        // message — including the one closed here by the next
                        // pick/reword — so an ordinary pick receives canonical
                        // attribution, not just reword/squash.
                        let msg = match finalize {
                            Some(f) => f(&msg),
                            None => msg,
                        };
                        tip = self.write_commit_on(cur_parent, cur_tree, &msg, author)?;
                        tip_tree = cur_tree;
                        produced += 1;
                    }
                    cur_parent = tip;
                    cur_tree = new_tree;
                    cur_msgs = vec![match step.action {
                        RebaseAction::Reword => step
                            .message
                            .clone()
                            .ok_or(GitError::BadPlan("reword needs a message".into()))?,
                        _ => cc.message.clone(),
                    }];
                    open = true;
                }
                RebaseAction::Squash => {
                    if !open {
                        return Err(GitError::BadPlan("squash before any pick".into()));
                    }
                    cur_tree = new_tree;
                    cur_msgs.push(step.message.clone().unwrap_or_else(|| cc.message.clone()));
                }
                RebaseAction::Fixup => {
                    if !open {
                        return Err(GitError::BadPlan("fixup before any pick".into()));
                    }
                    cur_tree = new_tree; // message discarded
                }
                RebaseAction::Drop => unreachable!("filtered above"),
            }
        }
        // Close the final open commit.
        if open {
            let msg = cur_msgs.join("\n\n");
            // #1709 req 9: the final produced commit receives canonical
            // attribution too (same finalizer as every other rebase commit).
            let msg = match finalize {
                Some(f) => f(&msg),
                None => msg,
            };
            tip = self.write_commit_on(cur_parent, cur_tree, &msg, author)?;
            tip_tree = cur_tree;
            produced += 1;
        }
        self.guard_move_ref_and_sync_tree(
            old_head_tree,
            tip_tree,
            "rebase",
            short_oid(&tip),
            || Ok(write_ref(&self.repo.git_dir, &head_ref, &tip)?),
        )?;
        Ok(RebaseReport {
            new_head: short_oid(&tip),
            produced,
            dropped,
        })
    }

    /// Shared ref-moving tail for `rebase` and `checkout`: refuse if the new
    /// tree would overwrite an ignored file the `clean` precondition can't
    /// see (#2518), then move the ref via `move_ref`, then reset the
    /// worktree + index to `new_tree` LAST so tree == HEAD after the ref
    /// moves. `op`/`new_head` label the error text if that final reset fails
    /// partway (ref already moved; `GitError::Refused` is used anyway — a
    /// moved ref plus a half-updated tree is the least-bad state to report
    /// through the same variant as an outright refusal).
    fn guard_move_ref_and_sync_tree(
        &self,
        old_tree: Option<ObjectId>,
        new_tree: ObjectId,
        op: &str,
        new_head: String,
        move_ref: impl FnOnce() -> Result<(), GitError>,
    ) -> Result<(), GitError> {
        if let Some(wt) = self.repo.work_tree.clone() {
            let changes = diff_trees(&self.repo.odb, old_tree.as_ref(), Some(&new_tree), "")?;
            for change in &changes {
                if change.status != DiffStatus::Added {
                    continue;
                }
                let Some(path) = &change.new_path else {
                    continue;
                };
                if wt.join(path).symlink_metadata().is_ok() {
                    return Err(GitError::Refused(format!(
                        "{op}: refusing — {path} already exists on disk and would be overwritten by the checked-out tree"
                    )));
                }
            }
        }
        move_ref()?;
        // `None` (an unborn HEAD) is the empty tree to grit, so every path in
        // `new_tree` is written; skipping the reset here would leave tree != HEAD.
        checkout_between_trees(&self.repo, old_tree.as_ref(), &new_tree).map_err(|e| {
            GitError::Refused(format!(
                "{op}: HEAD moved to {new_head} but the working tree may be partially updated and the index still reflects the previous HEAD; run status before continuing: {e}"
            ))
        })?;
        Ok(())
    }

    /// `git branch <name>` — create `refs/heads/<name>` at the current HEAD commit.
    /// Requires `refs` to permit that ref name. Returns the full ref name.
    pub fn branch(&self, caps: &GitCaveats, name: &str) -> Result<String, GitError> {
        let refname = format!("refs/heads/{name}");
        if !caps.permits_ref(&refname) {
            return Err(GitError::Denied("refs"));
        }
        // `write_ref` below has no existence check — this is an implicit
        // force-move if `refname` already points elsewhere. Round 4, Blocker
        // 2's audit: gate it the same way `commit`/`amend`/`rebase` are.
        refuse_if_default_branch(
            &self.repo.git_dir,
            &refname,
            !repository_has_no_refs(&self.repo.git_dir),
        )?;
        let oid = self
            .head_oid()?
            .ok_or(GitError::Unsupported("cannot branch from an unborn HEAD"))?;
        write_ref(&self.repo.git_dir, &refname, &oid)?;
        Ok(refname)
    }

    /// `git checkout [-b] <name>` — point HEAD at branch `<name>`, creating it at
    /// the current commit first when `create` is set (the `-b` case the model
    /// reaches for). Requires `refs` to permit the branch ref.
    ///
    /// newt is local-only and has no working-tree updater, so this only moves
    /// HEAD when the target branch is at the SAME commit as the current HEAD
    /// (always true for a freshly-created branch), or resets the worktree +
    /// index to the target commit's tree (#2485, same guarded tail as
    /// `rebase`) when the tree is clean. A dirty tree, or an ignored file in
    /// the way, refuses with no side effects.
    pub fn checkout(
        &self,
        caps: &GitCaveats,
        name: &str,
        create: bool,
    ) -> Result<String, GitError> {
        let refname = format!("refs/heads/{name}");
        if !caps.permits_ref(&refname) {
            return Err(GitError::Denied("refs"));
        }
        let head = self.head_oid()?;
        let existing = resolve_ref(&self.repo.git_dir, &refname).ok();
        let (target, created) = match (existing, create) {
            (Some(oid), _) => (Some(oid), false),
            (None, true) => {
                // Round 4, Blocker 2's audit: `checkout -b main` creating a
                // FRESH `refs/heads/main` while the repository already has
                // other refs is the same "retarget the default branch"
                // shape the exemption exists to NOT cover.
                refuse_if_default_branch(
                    &self.repo.git_dir,
                    &refname,
                    !repository_has_no_refs(&self.repo.git_dir),
                )?;
                let oid = head.ok_or(GitError::Unsupported(
                    "cannot create a branch from an unborn HEAD",
                ))?;
                write_ref(&self.repo.git_dir, &refname, &oid)?;
                (Some(oid), true)
            }
            (None, false) => {
                return Err(GitError::Refused(format!(
                    "branch '{name}' does not exist (pass create=true to make it)"
                )));
            }
        };
        if target != head {
            // #2485: same ref-moving invariant as `rebase` — only switch if
            // the tree can be synced to the target commit without side
            // effects, via the same guarded tail.
            let pre_status = self.status(caps)?;
            if !pre_status.clean {
                return Err(GitError::Refused(format!(
                    "refusing to switch to '{name}': working tree is not clean \
                     ({} staged, {} unstaged, {} untracked); commit or stash first",
                    pre_status.staged.len(),
                    pre_status.unstaged.len(),
                    pre_status.untracked.len()
                )));
            }
            let old_tree = self.head_tree()?;
            let new_tree = self.commit_tree(&target.expect("target set above"))?;
            self.guard_move_ref_and_sync_tree(
                old_tree,
                new_tree,
                "checkout",
                name.to_string(),
                || Ok(write_symbolic_ref(&self.repo.git_dir, "HEAD", &refname)?),
            )?;
            return Ok(format!("switched to branch '{name}'"));
        }
        write_symbolic_ref(&self.repo.git_dir, "HEAD", &refname)?;
        Ok(if created {
            format!("created and switched to branch '{name}'")
        } else {
            format!("switched to branch '{name}'")
        })
    }

    /// `git branch -d <name>` — delete `refs/heads/<name>`. Requires `refs` to
    /// permit the ref. Refuses to delete the branch HEAD is currently on, or a
    /// branch that does not exist (no side effects on refusal).
    pub fn branch_delete(&self, caps: &GitCaveats, name: &str) -> Result<String, GitError> {
        let refname = format!("refs/heads/{name}");
        if !caps.permits_ref(&refname) {
            return Err(GitError::Denied("refs"));
        }
        if let HeadState::Branch { short_name, .. } = resolve_head(&self.repo.git_dir)? {
            if short_name == name {
                return Err(GitError::Refused(format!(
                    "cannot delete branch '{name}': it is the current branch"
                )));
            }
        }
        if resolve_ref(&self.repo.git_dir, &refname).is_err() {
            return Err(GitError::Refused(format!("branch '{name}' does not exist")));
        }
        // Round 4, Blocker 2: deleting `refname` here means it just resolved
        // above, so it unconditionally exists — `true`, always. This closes
        // the bypass where `branch-delete main` (previously unguarded) made
        // `main` unborn, reopening the OLD (per-ref) exemption for a commit
        // that created a brand-new `main` as a root commit with the guard
        // never firing.
        refuse_if_default_branch(&self.repo.git_dir, &refname, true)?;
        delete_ref(&self.repo.git_dir, &refname)?;
        Ok(format!("deleted branch '{name}'"))
    }

    // --- stash (#992): pure-Rust, no git binary. `push` builds the standard
    // 2-parent stash commit from primitives; list/pop/apply/drop reuse grit-lib's
    // reflog + `apply_stash`. Scope: TRACKED changes (untracked left in place).

    /// `git stash push` (tracked changes) — gated on the `commit` capability.
    pub fn stash_push(&self, caps: &GitCaveats, author: &Author) -> Result<String, GitError> {
        if !caps.permits_commit() {
            return Err(GitError::Denied("stash"));
        }
        let wt = self
            .repo
            .work_tree
            .clone()
            .ok_or(GitError::Unsupported("cannot stash in a bare repository"))?;
        let head = self.head_oid()?.ok_or_else(|| {
            GitError::Refused("nothing to stash: no commits yet (unborn HEAD)".into())
        })?;
        let head_tree = self
            .head_tree()?
            .ok_or(GitError::Unsupported("cannot resolve HEAD tree"))?;

        let index = self.repo.load_index()?;
        let staged = diff_index_to_tree(&self.repo.odb, &index, Some(&head_tree), false)?;
        let unstaged = self.worktree_changes(&index, &wt)?;
        if staged.is_empty() && unstaged.is_empty() {
            return Ok("No local changes to save".into());
        }

        let ident = author.ident_now();
        let branch = match resolve_head(&self.repo.git_dir)? {
            HeadState::Branch { short_name, .. } => short_name,
            _ => "(no branch)".to_string(),
        };
        let subj = parse_commit(&self.repo.odb.read(&head)?.data)?
            .message
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        let head_short = short_oid(&head);
        let wip_msg = format!("WIP on {branch}: {head_short} {subj}");
        let idx_msg = format!("index on {branch}: {head_short} {subj}");

        // parent[1] = the index-commit (tree = staged state). ALWAYS written —
        // apply_stash requires >= 2 parents even when nothing is staged.
        let i_tree = write_tree_from_index(&self.repo.odb, &index, "")?;
        let i_oid = self.repo.odb.write(
            ObjectKind::Commit,
            &serialize_commit(&CommitData {
                tree: i_tree,
                parents: vec![head],
                author: ident.clone(),
                committer: ident.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: idx_msg,
                raw_message: None,
            }),
        )?;

        // The worktree tree: the index with the unstaged worktree changes folded in.
        let mut temp = index.clone();
        for e in &unstaged {
            let p = e.path();
            if e.status == DiffStatus::Deleted {
                temp.remove(p.as_bytes());
            } else {
                let bytes = std::fs::read(wt.join(p))?;
                let oid = self.repo.odb.write(ObjectKind::Blob, &bytes)?;
                let mode = index
                    .get(p.as_bytes(), 0)
                    .map_or(MODE_REGULAR, |ie| ie.mode);
                temp.add_or_replace(entry_from_stat(&wt.join(p), p.as_bytes(), oid, mode)?);
            }
        }
        temp.sort();
        let w_tree = write_tree_from_index(&self.repo.odb, &temp, "")?;
        let w_oid = self.repo.odb.write(
            ObjectKind::Commit,
            &serialize_commit(&CommitData {
                tree: w_tree,
                parents: vec![head, i_oid],
                author: ident.clone(),
                committer: ident.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: wip_msg.clone(),
                raw_message: None,
            }),
        )?;

        // Store the ref + reflog (two calls; write_ref never logs). force_create
        // is MANDATORY — refs/stash is excluded from reflog auto-creation, so
        // without it `stash list` would silently never exist.
        let old =
            resolve_ref(&self.repo.git_dir, "refs/stash").unwrap_or_else(|_| ObjectId::zero());
        write_ref(&self.repo.git_dir, "refs/stash", &w_oid)?;
        append_reflog(
            &self.repo.git_dir,
            "refs/stash",
            &old,
            &w_oid,
            &ident,
            &wip_msg,
            true,
        )?;

        // Reset the worktree + index to HEAD LAST — this destroys the dirty state,
        // so it must run only after the stash commit + ref are safely written.
        checkout_between_trees(&self.repo, Some(&w_tree), &head_tree)?;
        Ok(format!("Saved working directory and index state {wip_msg}"))
    }

    /// `git stash list` — the `refs/stash` reflog, newest first. Needs `read`.
    pub fn stash_list(&self, caps: &GitCaveats) -> Result<Vec<String>, GitError> {
        if !caps.permits_read() {
            return Err(GitError::Denied("read"));
        }
        Ok(read_reflog(&self.repo.git_dir, "refs/stash")?
            .iter()
            .rev()
            .enumerate()
            .map(|(i, e)| format!("stash@{{{i}}}: {}", e.message))
            .collect())
    }

    /// The stash commit oid for `stash@{k}` (0 = newest), or a Refused error.
    fn stash_oid_at(&self, k: usize) -> Result<ObjectId, GitError> {
        let entries = read_reflog(&self.repo.git_dir, "refs/stash")?;
        if entries.is_empty() {
            return Err(GitError::Refused("no stash entries found".into()));
        }
        entries
            .iter()
            .rev()
            .nth(k)
            .map(|e| e.new_oid)
            .ok_or_else(|| GitError::Refused(format!("no stash entry stash@{{{k}}}")))
    }

    /// `git stash apply stash@{k}` — apply without dropping. Gated on `commit`.
    pub fn stash_apply(&self, caps: &GitCaveats, k: usize) -> Result<String, GitError> {
        if !caps.permits_commit() {
            return Err(GitError::Denied("stash"));
        }
        let wt = self
            .repo
            .work_tree
            .clone()
            .ok_or(GitError::Unsupported("bare repository"))?;
        let oid = self.stash_oid_at(k)?;
        let conflicts = apply_stash(&self.repo, &wt, &oid, false, true)?;
        Ok(if conflicts {
            format!("applied stash@{{{k}}} with conflicts (resolve, then drop it)")
        } else {
            format!("applied stash@{{{k}}}")
        })
    }

    /// `git stash pop stash@{k}` — apply, then drop ONLY on a clean apply (git
    /// keeps the entry on conflict). Gated on `commit`.
    pub fn stash_pop(&self, caps: &GitCaveats, k: usize) -> Result<String, GitError> {
        if !caps.permits_commit() {
            return Err(GitError::Denied("stash"));
        }
        let wt = self
            .repo
            .work_tree
            .clone()
            .ok_or(GitError::Unsupported("bare repository"))?;
        let oid = self.stash_oid_at(k)?;
        if apply_stash(&self.repo, &wt, &oid, false, true)? {
            return Ok(format!(
                "stash@{{{k}}} applied with conflicts — entry kept (resolve, then drop it)"
            ));
        }
        self.stash_drop_impl(k)?;
        Ok(format!("popped stash@{{{k}}}"))
    }

    /// `git stash drop stash@{k}`. Gated on `commit`.
    pub fn stash_drop(&self, caps: &GitCaveats, k: usize) -> Result<String, GitError> {
        if !caps.permits_commit() {
            return Err(GitError::Denied("stash"));
        }
        self.stash_drop_impl(k)?;
        Ok(format!("dropped stash@{{{k}}}"))
    }

    fn stash_drop_impl(&self, k: usize) -> Result<(), GitError> {
        let _ = self.stash_oid_at(k)?; // validates the slot exists
        let git_dir = &self.repo.git_dir;
        delete_reflog_entries(git_dir, "refs/stash", &[k])?;
        // delete_reflog_entries only rewrites the log — re-point (or remove) the ref.
        match read_reflog(git_dir, "refs/stash")?.last() {
            Some(top) => write_ref(git_dir, "refs/stash", &top.new_oid)?,
            None => {
                let _ = delete_ref(git_dir, "refs/stash");
                let _ = std::fs::remove_file(reflog_file_path(git_dir, "refs/stash"));
            }
        }
        Ok(())
    }
}

fn short_oid(oid: &ObjectId) -> String {
    oid.to_hex().chars().take(7).collect()
}

fn file_change(e: &DiffEntry) -> FileChange {
    FileChange {
        status: e.status.letter(),
        path: e.path().to_string(),
    }
}

fn commit_info(oid: &ObjectId, c: &CommitData) -> CommitInfo {
    let (author_name, author_email, timestamp) = parse_ident(&c.author);
    let summary = c.message.lines().next().unwrap_or("").to_string();
    CommitInfo {
        id: oid.to_hex(),
        short_id: short_oid(oid),
        author_name,
        author_email,
        timestamp,
        summary,
        parents: c.parents.iter().map(|p| p.to_hex()).collect(),
    }
}

/// Parse a git ident line `"Name <email> 1700000000 +0000"` → (name, email, unix secs).
fn parse_ident(s: &str) -> (String, String, i64) {
    let name = s.split(" <").next().unwrap_or("").trim().to_string();
    let email = s
        .split_once('<')
        .and_then(|(_, rest)| rest.split_once('>'))
        .map(|(e, _)| e.to_string())
        .unwrap_or_default();
    let timestamp = s
        .rsplit('>')
        .next()
        .and_then(|tail| tail.split_whitespace().next())
        .and_then(|n| n.parse::<i64>().ok())
        .unwrap_or(0);
    (name, email, timestamp)
}

// ---------------------------------------------------------------------------
// LocalGitTool — the injected `GitTool` impl (PR4, #461)
// ---------------------------------------------------------------------------

/// The on-disk [`GitEngine`] adapted to newt-core's
/// [`GitTool`](newt_core::agentic::GitTool) seam. The binary constructs one per
/// session (root = workspace, author = the resolved agent identity) and injects
/// it into the agent loop; `execute_tool`'s `git` arm calls
/// [`dispatch`](GitTool::dispatch). A fresh `GitEngine::open` per call keeps it
/// stateless and cheap (no long-lived handle across turns).
pub struct LocalGitTool {
    pub root: std::path::PathBuf,
    pub author: Author,
    /// The canonical, harness-owned commit attribution envelope — the active
    /// model + harness build + operator/agent identity, finalized into every
    /// commit/amend/rebase message by
    /// [`CommitAttribution::finalize_message`](newt_core::attribution::CommitAttribution::finalize_message).
    /// Refreshed as late as practical before the turn that may commit (in the
    /// session loop, from the live inference model + resolved identity) so a
    /// `/model` switch is reflected in the next commit, not the one frozen at
    /// session boot. `None` only in test scaffolds that opt out of signing;
    /// the commit arms then leave the message unchanged.
    pub attribution: Option<newt_core::attribution::CommitAttribution>,
    /// #1709 family — the EXPLICIT commit-success signal. Incremented in the
    /// `commit` / `amend` / `rebase` arms ONLY on a confirmed successful
    /// `eng.*` call (the actual commit creation), never on a `HEAD` change.
    /// The session loop drains this ([`LocalGitTool::drain_commit_success`])
    /// after a turn and clears the contributor ledger ONLY when a real Newt
    /// commit landed — so a `HEAD` move from an external/manual action (a
    /// user `git reset`, a fetch advancing the branch, …) does NOT discard
    /// pending contributors, and a commit whose `HEAD`-diff proxy was
    /// unreliable still clears. Atomic for cross-thread visibility (the
    /// session runs on its own thread; the drain runs on the loop thread).
    pub commit_succeeded: std::sync::atomic::AtomicUsize,
    /// #1709 family — the per-lifecycle contributor-consumption cursor. The
    /// envelope's `contributors` snapshot is FROZEN for the turn (the field
    /// is owned, and [`GitTool::dispatch`] takes `&self`, so it cannot be
    /// mutated at the commit boundary). This cursor is the interior-mutable
    /// view of how many of those frozen contributors a confirmed successful
    /// commit has already consumed: [`LocalGitTool::finalize_commit_message`]
    /// renders only `contributors[cursor..]`, and each `commit`/`amend`/
    /// `rebase` arm advances `cursor → contributors.len()` on success. So a
    /// SECOND commit in the SAME tool/turn lifecycle (C1 → more work → C2)
    /// sees an empty contributor slice and re-credits nobody from C1 — the
    /// snapshot is consumed at the actual commit boundary, not deferred to
    /// the end-of-turn drain. Reset to 0 by the session loop when it
    /// refreshes the envelope at the top of each iteration. Atomic for the
    /// same cross-thread reason as `commit_succeeded`.
    pub contributors_consumed: std::sync::atomic::AtomicUsize,
    /// Signs the commits this tool writes, per the operator's
    /// `[agent-identity.git] signing` (`newt_core::commit_signing::signer_for`).
    pub signer: Option<std::sync::Arc<dyn newt_core::commit_signing::CommitSigner>>,
}

impl LocalGitTool {
    /// Capability-governed, O(HEAD) repository identity for harness
    /// provenance. Opening afresh matches [`GitTool::dispatch`]'s stateless
    /// behavior and never invents read authority.
    pub fn head_snapshot(
        &self,
        caps: &GitCaveats,
        session: &Caveats,
    ) -> Result<HeadSnapshot, GitError> {
        GitEngine::open(&self.root, &session.fs_read)?.head_snapshot(caps)
    }

    /// The ONE first-class commit-message attribution boundary (#1709
    /// integration). Every `commit` / `amend` / `rebase` arm routes its
    /// model-provided subject+body through here, so no caller independently
    /// formats attribution — the typed [`CommitAttribution`] owns it
    /// (deterministic, idempotent, replaces stale Newt-owned trailers,
    /// preserves legitimate third-party ones). Returns the message unchanged
    /// when no attribution is configured (test scaffolds).
    ///
    /// [`CommitAttribution`]: newt_core::attribution::CommitAttribution
    fn finalize_commit_message(&self, message: &str) -> String {
        match &self.attribution {
            // Semantic B: the envelope's `contributors` snapshot (the
            // accumulated ledger, captured at the latest refresh) is merged
            // with the active model by `finalize_message` →
            // `finalize_message_with`, so every contributing model is
            // credited. An empty snapshot yields the single active-model
            // floor (semantic A).
            //
            // #1709 family: the snapshot is CONSUMED at the commit boundary,
            // not the end-of-turn boundary. `contributors_consumed` is a
            // cursor into the frozen `contributors` Vec — render only the
            // UNCONSUMED tail `contributors[cursor..]`. A prior successful
            // commit in this same lifecycle advanced the cursor past the
            // contributors it already credited, so this commit re-credits
            // none of them (C1 → more work → C2: C2's slice is empty).
            Some(a) => {
                let cursor = self
                    .contributors_consumed
                    .load(std::sync::atomic::Ordering::Relaxed);
                let start = cursor.min(a.contributors.len());
                a.finalize_message_with(message, &a.contributors[start..])
            }
            None => message.to_string(),
        }
    }

    /// Consume the contributor snapshot at the confirmed-successful commit
    /// boundary — advance the cursor past every contributor the just-landed
    /// commit credited, so a subsequent commit in the SAME lifecycle re-credits
    /// none of them. No-op when no attribution is configured (test scaffolds).
    fn consume_contributors(&self) {
        if let Some(a) = &self.attribution {
            self.contributors_consumed
                .store(a.contributors.len(), std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Drain the explicit commit-success counter — returns the number of
    /// Newt commits that ACTUALLY landed since the last drain, and resets it
    /// to zero. The session loop calls this after a turn and clears the
    /// contributor ledger ONLY when it is non-zero (a confirmed successful
    /// commit), never merely because `HEAD` moved (the historical
    /// stale-attribution class). See [`LocalGitTool::commit_succeeded`].
    #[must_use]
    pub fn drain_commit_success(&self) -> usize {
        self.commit_succeeded
            .swap(0, std::sync::atomic::Ordering::Relaxed)
    }
}

// #1709 integration: commit-message attribution is owned by the canonical
// finalizer [`CommitAttribution::finalize_message`] (in `newt-core`), reached
// through [`LocalGitTool::finalize_commit_message`]. The old per-call
// `sign_message` + `attribution_block` + `operator_name` formatting — which
// duplicated the finalizer and stamped a non-deterministic wall-clock
// `Time`/`Date` footer — is removed; every commit arm now routes through the
// one shared boundary so no caller formats attribution itself.
//
// [`CommitAttribution`]: newt_core::attribution::CommitAttribution

impl newt_core::agentic::GitTool for LocalGitTool {
    fn dispatch(
        &self,
        op: &str,
        args: &serde_json::Value,
        caps: &GitCaveats,
        session: &Caveats,
    ) -> Result<String, String> {
        newt_core::agentic::check_git_read_scope(op, &session.fs_read).map_err(str::to_owned)?;
        if op == "branch-list" && !caps.permits_read() {
            return Err(GitError::Denied("read").to_string());
        }
        let read_scope = canonical_read_scope(session);
        let root = checked_dispatch_root(&self.root, args, &read_scope)?;
        let explicit_cwd = args.get("cwd").is_some_and(|v| !v.is_null());
        let mutates = !matches!(op, "status" | "log" | "diff" | "branch-list" | "stash-list");
        let write_scope = if explicit_cwd && mutates {
            canonical_scope(&session.fs_write)
        } else {
            Scope::none()
        };
        if explicit_cwd
            && mutates
            && !newt_core::caveats::permits_path(&write_scope, &root.to_string_lossy())
        {
            return Err("capability denied: fs_write for Git cwd".into());
        }
        // `init` CREATES a repo, so it runs BEFORE opening one — every other op
        // requires an existing repo (`GitEngine::open` below). It is a write:
        // gate it on the commit/write capability so a read-only session cannot
        // create a repo. This is what lets the tool be advertised (and useful)
        // in a not-yet-a-repo workspace instead of silently disappearing.
        if op == "init" {
            if !caps.permits_commit() {
                return Err(GitError::Denied("init").to_string());
            }
            checked_git_read(&root, &read_scope).map_err(|e| e.to_string())?;
            // An existing gitfile must not be mistaken for a missing repo
            // merely because its target is outside the read grant.
            if checked_optional_git_read(&root.join(".git"), &read_scope)
                .map_err(|e| e.to_string())?
            {
                scoped_repository_paths(&root, &read_scope).map_err(|e| e.to_string())?;
                return Ok("git: already a repository here".into());
            }
            if scoped_repository_paths(&root, &read_scope).is_ok() {
                return Ok("git: already a repository here".into());
            }
            grit_lib::repo::init_repository(&root, false, "main", None, "files")
                .map_err(|e| format!("init failed: {e}"))?;
            return Ok("initialized empty git repository on branch 'main'".into());
        }
        if explicit_cwd
            && !checked_optional_git_read(&root.join(".git"), &read_scope)
                .map_err(|e| e.to_string())?
        {
            return Err("Git cwd must identify a repository/worktree root with .git; refusing parent discovery".into());
        }
        let (git_dir, common_dir, worktree) =
            scoped_repository_paths(&root, &read_scope).map_err(|e| e.to_string())?;
        // PR #2577 round 2 removed a whole-directory `permits_path(write_scope,
        // git_dir/common_dir)` check here on the theory that `cwd` resolving
        // *inside* `self.root` (proved by `checked_dispatch_root`) means `cwd`
        // is always the session's own repository. Round 3 correction: that is
        // FALSE — a NESTED repository (#2552 `InsideRepo`) is supported, and a
        // nested LINKED worktree's `git_dir`/`common_dir` can point anywhere on
        // disk via its `commondir` file, regardless of where the worktree
        // directory itself sits. So the gate is restored, widened by exactly
        // one case: mutation through `cwd` is permitted when the resolved
        // `git_dir`/`common_dir` are EITHER an explicit `fs_write` grant, OR
        // are the session workspace's OWN repository — the same
        // `git_dir`/`common_dir` `self.root` itself resolves to (the case
        // `refuse_if_default_branch` protects at the engine layer). Anything
        // else — a foreign nested repo `cwd` merely happens to point at — falls
        // back to needing the ordinary explicit `fs_write` grant.
        if explicit_cwd
            && mutates
            && ![&git_dir, &common_dir]
                .iter()
                .all(|path| newt_core::caveats::permits_path(&write_scope, &path.to_string_lossy()))
        {
            let is_own_repo = scoped_repository_paths(&self.root, &read_scope)
                .ok()
                .is_some_and(|(own_git_dir, own_common_dir, _)| {
                    git_dir == own_git_dir && common_dir == own_common_dir
                });
            if !is_own_repo {
                return Err(
                    "capability denied: fs_write for selected worktree Git metadata".into(),
                );
            }
        }
        if op == "branch-list" {
            validate_branch_ref_inputs(&git_dir, &common_dir, &read_scope)
                .map_err(|e| e.to_string())?;
            return render_branch_list(&git_dir, args).map_err(|e| e.to_string());
        }
        // Explicit open: discovery must not override the injected root through
        // ambient GIT_DIR / GIT_WORK_TREE after the filesystem scope check.
        let eng = GitEngine {
            repo: Repository::open(&git_dir, worktree.as_deref()).map_err(|e| e.to_string())?,
            signer: self.signer.clone(),
        };
        let s = |e: GitError| e.to_string();
        match op {
            "status" => Ok(render_status(&eng.status(caps).map_err(s)?)),
            "log" => {
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let revision = args.get("revision").and_then(|v| v.as_str());
                let paths = str_array(args, "paths");
                Ok(render_log(&eng.log(caps, limit, revision, &paths).map_err(s)?))
            }
            "diff" => {
                let rev = args.get("rev").and_then(|v| v.as_str());
                let rev2 = args.get("rev2").and_then(|v| v.as_str());
                let spec = match (args.get("spec").and_then(|v| v.as_str()), rev, rev2) {
                    (Some("staged"), ..) => DiffSpec::Staged,
                    (_, Some(a), Some(b)) => DiffSpec::RevRange(a.to_string(), b.to_string()),
                    (_, Some(a), None) => DiffSpec::Rev(a.to_string()),
                    _ => DiffSpec::Worktree,
                };
                let paths = str_array(args, "paths");
                let stat = args.get("stat").and_then(|v| v.as_bool()).unwrap_or(false);
                Ok(render_diff(&eng.diff(caps, spec, &paths, stat).map_err(s)?))
            }
            "add" => {
                let paths = str_array(args, "paths");
                if paths.is_empty() {
                    return Err("add: 'paths' (array of repo-relative paths) is required".into());
                }
                let staged = eng.add(caps, &paths).map_err(s)?;
                Ok(format!(
                    "staged {} path(s): {}",
                    staged.len(),
                    staged.join(", ")
                ))
            }
            "commit" => {
                let msg = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .filter(|m| !m.trim().is_empty())
                    .ok_or("commit: 'message' is required")?;
                let signed = self.finalize_commit_message(msg);
                let c = eng.commit(caps, &signed, &self.author).map_err(s)?;
                // #1709 family: the explicit commit-success signal — a confirmed
                // Newt commit landed. The session loop clears the contributor
                // ledger off THIS, not a `HEAD` diff.
                self.commit_succeeded
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // #1709 family: consume the contributor snapshot AT the commit
                // boundary — the contributors this commit just credited are
                // spent, so a second commit in this same lifecycle re-credits
                // none of them.
                self.consume_contributors();
                Ok(format!("committed {}: {}", c.short_id, c.summary))
            }
            "amend" => {
                // Optional message: present → reword (signed); absent → keep
                // HEAD's existing message. #1709 req 7: even with NO new
                // message, read HEAD's existing FULL message and run it through
                // the canonical attribution finalizer before creating the
                // amended commit, so attribution is REFRESHED (a `/model`
                // switch since the original commit replaces the stale Newt
                // model trailers + provenance; legitimate third-party trailers
                // and the user subject/body are preserved — the finalizer is
                // idempotent). When no attribution is configured (test
                // scaffolds), fall back to the engine's "keep HEAD's message"
                // path (pass `None`) so an unborn-HEAD amend still reports its
                // own error rather than a read failure.
                let msg = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .filter(|m| !m.trim().is_empty());
                let signed = match (&self.attribution, msg) {
                    (Some(_), Some(m)) => Some(self.finalize_commit_message(m)),
                    (Some(_), None) => {
                        let head_msg = eng.head_message(caps).map_err(s)?;
                        // Unborn HEAD → empty: let `eng.amend(None, …)` report
                        // "nothing to amend" rather than finalizing an empty
                        // string into a bogus message.
                        if head_msg.is_empty() {
                            None
                        } else {
                            Some(self.finalize_commit_message(&head_msg))
                        }
                    }
                    (None, Some(m)) => Some(m.to_string()),
                    (None, None) => None,
                };
                let c = eng
                    .amend(caps, signed.as_deref(), &self.author)
                    .map_err(s)?;
                // #1709 family: amend creates a commit too — signal it.
                self.commit_succeeded
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // #1709 family: amend finalized from the same frozen snapshot;
                // consume it here too so a later commit in this lifecycle does
                // not re-credit the contributors amend just stamped.
                self.consume_contributors();
                Ok(format!("amended {}: {}", c.short_id, c.summary))
            }
            "rebase" => {
                let onto = args
                    .get("onto")
                    .and_then(|v| v.as_str())
                    .filter(|o| !o.trim().is_empty())
                    .ok_or("rebase: 'onto' (the base commit/ref to replay onto) is required")?;
                let steps = parse_rebase_plan(args)?;
                if steps.is_empty() {
                    return Err("rebase: 'plan' must list at least one step".to_string());
                }
                // #1709 req 9: every newly created rebase commit (pick/reword/
                // squash) is finalized through the SAME canonical finalizer as
                // `commit`/`amend` — `finalize_commit_message` reads the
                // consumption cursor, which is stable for the whole rebase (it
                // advances once, below, after the rebase lands), so every
                // rebase commit shares the one frozen contributor slice. `None`
                // when no attribution is configured (test scaffolds) → messages
                // pass through untouched.
                let r = {
                    // Bind the closure to a `let` so it outlives the `&` borrow
                    // (rustc 1.88 rejects the temporary-closure form E0716).
                    let finalize_fn = |m: &str| self.finalize_commit_message(m);
                    let finalize: Option<&dyn Fn(&str) -> String> = match &self.attribution {
                        Some(_) => Some(&finalize_fn),
                        None => None,
                    };
                    eng.rebase(caps, onto, &steps, &self.author, finalize)
                        .map_err(s)?
                };
                // #1709 family: a rebase is an attribution EPOCH only when it
                // actually PRODUCED commits (`r.produced > 0`). An all-drop plan
                // (`produced == 0`) is a successful history operation — it
                // rewrites nothing and creates no commit — so it is NOT an
                // attribution epoch: the pending contributors are PRESERVED (a
                // later commit in this lifecycle still credits them), and
                // `commit_succeeded` is NOT reported (no Newt commit landed for
                // the turn telemetry to count). Gating both the explicit
                // commit-success signal AND the contributor-snapshot consumption
                // on `produced > 0` keeps the two consumption paths (this
                // per-tool cursor + the session-loop ledger clear) in agreement:
                // a 0-produced rebase consumes nothing on either path.
                if r.produced > 0 {
                    self.commit_succeeded
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // The rebase's reword/squash steps finalized from the same
                    // frozen contributor slice; consume it so a later commit in
                    // this lifecycle does not re-credit them.
                    self.consume_contributors();
                }
                Ok(format!(
                    "rebased onto {onto} → {} ({} commit(s), {} dropped)",
                    r.new_head, r.produced, r.dropped
                ))
            }
            "branch" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|n| !n.trim().is_empty())
                    .ok_or("branch: 'name' is required")?;
                let r = eng.branch(caps, name).map_err(s)?;
                Ok(format!("created {r}"))
            }
            "checkout" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|n| !n.trim().is_empty())
                    .ok_or("checkout: 'name' (the branch to switch to) is required")?;
                // Default to creating the branch when absent — the `checkout -b`
                // the model reaches for to start work. Pass create=false for a
                // plain switch to an existing branch.
                let create = args.get("create").and_then(|v| v.as_bool()).unwrap_or(true);
                eng.checkout(caps, name, create).map_err(s)
            }
            "branch-delete" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|n| !n.trim().is_empty())
                    .ok_or("branch-delete: 'name' is required")?;
                eng.branch_delete(caps, name).map_err(s)
            }
            "stash" | "stash-push" => eng.stash_push(caps, &self.author).map_err(s),
            "stash-list" => {
                let list = eng.stash_list(caps).map_err(s)?;
                Ok(if list.is_empty() {
                    "no stash entries".to_string()
                } else {
                    list.join("\n")
                })
            }
            "stash-pop" => eng.stash_pop(caps, stash_index(args)).map_err(s),
            "stash-apply" => eng.stash_apply(caps, stash_index(args)).map_err(s),
            "stash-drop" => eng.stash_drop(caps, stash_index(args)).map_err(s),
            other => Err(format!(
                "unknown git op '{other}' (use init|status|log|diff|add|commit|amend|rebase|\
                 branch|branch-list|checkout|branch-delete|stash|stash-list|stash-pop|stash-apply|stash-drop)"
            )),
        }
    }
}

fn canonical_read_scope(session: &Caveats) -> Scope<String> {
    canonical_scope(&session.fs_read)
}

fn canonical_scope(scope: &Scope<String>) -> Scope<String> {
    match scope {
        Scope::All => Scope::All,
        Scope::Only(paths) => Scope::only(paths.iter().filter_map(|path| {
            Path::new(path)
                .canonicalize()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })),
    }
}

/// Target selection uses the same canonical containment as the existing Git
/// seam; it does not turn the legacy engine into an object-bound file broker.
/// The scoped-read guard remains mandatory before this resolver is reached.
fn checked_dispatch_root(
    root: &Path,
    args: &serde_json::Value,
    scope: &Scope<String>,
) -> Result<PathBuf, String> {
    let cwd = match args.get("cwd") {
        None | Some(serde_json::Value::Null) => return Ok(root.to_path_buf()),
        Some(serde_json::Value::String(cwd)) if !cwd.trim().is_empty() => Path::new(cwd),
        _ => return Err("Git cwd must be a non-empty relative directory or null".into()),
    };
    if cwd.components().any(|part| {
        !matches!(
            part,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err("Git cwd must stay relative to the session workspace; absolute paths and parent traversal are refused".into());
    }
    let root = checked_git_read(root, scope).map_err(|e| e.to_string())?;
    let target = checked_git_read(&root.join(cwd), scope).map_err(|e| e.to_string())?;
    if !target.starts_with(&root) || !target.is_dir() {
        return Err("Git cwd must be a directory inside the session workspace".into());
    }
    Ok(target)
}

fn checked_git_read(path: &Path, scope: &Scope<String>) -> Result<PathBuf, GitError> {
    let canonical = path.canonicalize()?;
    if !newt_core::caveats::permits_path(scope, &canonical.to_string_lossy()) {
        return Err(GitError::Refused(format!(
            "capability denied: fs_read for Git path {}",
            path.display()
        )));
    }
    let metadata = canonical.metadata()?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(GitError::Unsupported(
            "non-regular Git input cannot be read",
        ));
    }
    Ok(canonical)
}

fn checked_optional_git_read(path: &Path, scope: &Scope<String>) -> Result<bool, GitError> {
    match path.symlink_metadata() {
        Ok(_) => {
            checked_git_read(path, scope)?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Discover only inside authorized ancestors, then authorize both the resolved
/// gitfile target and the shared worktree metadata before any engine read.
fn scoped_repository_paths(
    root: &Path,
    scope: &Scope<String>,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>), GitError> {
    let mut current = checked_git_read(root, scope)?;
    let (git_dir, worktree) = loop {
        let dot_git = current.join(".git");
        if checked_optional_git_read(&dot_git, scope)? {
            break (
                checked_git_read(&grit_lib::repo::resolve_dot_git(&dot_git)?, scope)?,
                Some(current),
            );
        }
        if current.join("HEAD").exists() && current.join("objects").is_dir() {
            break (current, None);
        }
        current = checked_git_read(
            current
                .parent()
                .ok_or(GitError::Unsupported("not a Git repository"))?,
            scope,
        )?;
    };
    if !checked_optional_git_read(&git_dir.join("HEAD"), scope)? {
        return Err(GitError::Unsupported("Git directory has no HEAD"));
    }
    let common_dir = if checked_optional_git_read(&git_dir.join("commondir"), scope)? {
        checked_git_read(
            &grit_lib::refs::common_dir(&git_dir)
                .ok_or(GitError::Unsupported("invalid Git commondir"))?,
            scope,
        )?
    } else {
        git_dir.clone()
    };
    Ok((git_dir, common_dir, worktree))
}

fn render_branch_list(git_dir: &Path, args: &serde_json::Value) -> Result<String, GitError> {
    if !args.as_object().is_some_and(|args| {
        args.keys()
            .all(|key| matches!(key.as_str(), "op" | "scope" | "cwd"))
    }) {
        return Err(GitError::Unsupported(
            "branch-list accepts only op, scope and cwd",
        ));
    }
    let scope = match args.get("scope") {
        None | Some(serde_json::Value::Null) => "all",
        Some(serde_json::Value::String(scope))
            if matches!(scope.as_str(), "local" | "remote" | "all") =>
        {
            scope
        }
        _ => {
            return Err(GitError::Unsupported(
                "branch-list scope must be local, remote, or all",
            ))
        }
    };
    let mut groups = Vec::new();
    for (kind, label, prefix) in [
        ("local", "local branches", "refs/heads/"),
        ("remote", "cached remote-tracking branches", "refs/remotes/"),
    ] {
        if scope != "all" && scope != kind {
            continue;
        }
        let mut names = Vec::new();
        for (name, _) in grit_lib::refs::list_refs(git_dir, prefix)? {
            // Packed refs can contain arbitrary names. Validate before the
            // symbolic lookup turns one into a path, or it contributes a count.
            if !name.starts_with(prefix)
                || grit_lib::check_ref_format::check_refname_format(
                    &name,
                    &grit_lib::check_ref_format::RefNameOptions::default(),
                )
                .is_err()
            {
                return Err(GitError::Refused("invalid branch ref name".into()));
            }
            if kind != "remote" || grit_lib::refs::read_symbolic_ref(git_dir, &name)?.is_none() {
                names.push(name);
            }
        }
        groups.push((label, names));
    }
    // Put every count first so a long ref list may spill without hiding the
    // remote/local distinction the operator needs to interpret the answer.
    let mut out = groups
        .iter()
        .map(|(label, names)| format!("{label}: {}", names.len()))
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str("\nRemote-tracking refs are cached locally; no network access. These are branch refs, not open pull requests.\n");
    for (_, names) in groups {
        for name in names {
            out.push_str(&name);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Grit's files-backed ref reader follows child paths and symbolic targets.
/// Preflight those inputs before invoking its existing parser/enumerator. This
/// is canonical containment, not protection against concurrent path replacement
/// (the existing object-bound filesystem confinement deviation still applies).
fn validate_branch_ref_inputs(
    git_dir: &Path,
    common_dir: &Path,
    scope: &Scope<String>,
) -> Result<(), GitError> {
    // This operation reports the injected repository's physical branches, not
    // an ambient process namespace. Refuse rather than silently changing the
    // meaning of its counts, and never mutate process-global environment.
    if grit_lib::ref_namespace::raw_git_namespace_from_env().is_some() {
        return Err(GitError::Unsupported(
            "GIT_NAMESPACE is unsupported for branch-list",
        ));
    }
    for dir in [git_dir, common_dir] {
        for name in ["config", "commondir", "packed-refs"] {
            checked_optional_git_read(&dir.join(name), scope)?;
        }
        let config = dir.join("config");
        if config.exists() {
            // Pure parsing of this authorized file: no global configuration
            // or include cascade. Handles Git quoting/comments correctly.
            let parsed = grit_lib::config::ConfigFile::parse(
                &config,
                &std::fs::read_to_string(&config)?,
                grit_lib::config::ConfigScope::Local,
            )?;
            if let Some(storage) = parsed.get("extensions.refStorage") {
                if !storage.eq_ignore_ascii_case("files") {
                    return Err(GitError::Refused(format!(
                        "unsupported refStorage={storage} for branch-list"
                    )));
                }
            }
        }
    }
    if common_dir != git_dir && common_dir.join("commondir").exists() {
        return Err(GitError::Unsupported(
            "nested Git commondir is unsupported for branch-list",
        ));
    }
    // Reftables have a separate manifest and symbolic-resolution surface. Do
    // not return zero/incomplete counts when that layout has not been scoped.
    if grit_lib::reftable::is_reftable_repo(git_dir) {
        return Err(GitError::Unsupported(
            "reftable branch-list is not yet supported",
        ));
    }
    for dir in [git_dir, common_dir] {
        let refs = dir.join("refs");
        if !checked_optional_git_read(&refs, scope)? {
            continue;
        }
        let mut pending = vec![refs];
        let mut visited = std::collections::BTreeSet::new();
        while let Some(path) = pending.pop() {
            let canonical = checked_git_read(&path, scope)?;
            if path.is_dir() {
                if !visited.insert(canonical) {
                    return Err(GitError::Unsupported(
                        "repeated ref directory is unsupported for branch-list",
                    ));
                }
                for entry in std::fs::read_dir(&path)? {
                    pending.push(entry?.path());
                }
            } else {
                let target = match grit_lib::refs::read_ref_file(&path) {
                    Ok(grit_lib::refs::Ref::Symbolic(target)) => target,
                    // Match the enumerator: an empty lock/invalid non-ref
                    // contributes no ref, but IO failures still fail closed.
                    Ok(grit_lib::refs::Ref::Direct(_))
                    | Err(grit_lib::error::Error::InvalidRef(_)) => continue,
                    Err(error) => return Err(error.into()),
                };
                // All refs/ paths were traversed above/below. Restrict target
                // resolution to that namespace; root/worktree pseudo-refs and
                // path-like targets could otherwise select an unchecked file.
                if !target.starts_with("refs/")
                    || grit_lib::check_ref_format::check_refname_format(
                        &target,
                        &grit_lib::check_ref_format::RefNameOptions::default(),
                    )
                    .is_err()
                {
                    return Err(GitError::Unsupported("symbolic ref target outside the refs namespace is unsupported for branch-list"));
                }
            }
        }
        if git_dir == common_dir {
            break;
        }
    }
    Ok(())
}

/// Parse the `plan` array (`[{commit, action, message?}]`) into `RebaseStep`s.
/// Messages are passed through RAW — finalization (canonical attribution) is
/// the engine's job now: [`GitEngine::rebase`] applies the shared finalizer to
/// every newly created commit's joined message (pick / reword / squash), so no
/// rebase path formats attribution itself and an ordinary `pick` receives
/// canonical attribution too (#1709 req 9).
fn parse_rebase_plan(args: &serde_json::Value) -> Result<Vec<RebaseStep>, String> {
    let plan = args
        .get("plan")
        .and_then(|v| v.as_array())
        .ok_or("rebase: 'plan' (array of {commit, action, message?}) is required")?;
    let mut steps = Vec::with_capacity(plan.len());
    for (i, e) in plan.iter().enumerate() {
        let commit = e
            .get("commit")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("rebase plan[{i}]: 'commit' is required"))?
            .to_string();
        let action = match e.get("action").and_then(|v| v.as_str()).unwrap_or("pick") {
            "pick" => RebaseAction::Pick,
            "reword" => RebaseAction::Reword,
            "squash" => RebaseAction::Squash,
            "fixup" => RebaseAction::Fixup,
            "drop" => RebaseAction::Drop,
            other => {
                return Err(format!(
                    "rebase plan[{i}]: unknown action '{other}' (pick|reword|squash|fixup|drop)"
                ))
            }
        };
        // Raw message — the engine finalizes at commit-creation time.
        let message = e
            .get("message")
            .and_then(|v| v.as_str())
            .filter(|m| !m.trim().is_empty())
            .map(str::to_string);
        steps.push(RebaseStep {
            commit,
            action,
            message,
        });
    }
    Ok(steps)
}

fn str_array(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The `stash@{k}` index for pop/apply/drop — `index` arg, default 0 (newest).
fn stash_index(args: &serde_json::Value) -> usize {
    args.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

/// Compact, model-readable status (not raw JSON — the model reads prose better).
fn render_status(s: &StatusReport) -> String {
    let branch = s.branch.as_deref().unwrap_or("(detached)");
    let head = s.head.as_deref().unwrap_or("(unborn)");
    let mut out = format!("on branch {branch} (HEAD {head})\n");
    if s.clean {
        out.push_str("working tree clean");
        return out;
    }
    let mut group = |label: &str, files: &[FileChange]| {
        if !files.is_empty() {
            out.push_str(label);
            out.push_str(":\n");
            for f in files {
                out.push_str(&format!("  {} {}\n", f.status, f.path));
            }
        }
    };
    group("staged", &s.staged);
    group("unstaged", &s.unstaged);
    if !s.untracked.is_empty() {
        out.push_str("untracked:\n");
        for p in &s.untracked {
            out.push_str(&format!("  ? {p}\n"));
        }
    }
    out.trim_end().to_string()
}

fn render_log(commits: &[CommitInfo]) -> String {
    if commits.is_empty() {
        return "no commits".to_string();
    }
    commits
        .iter()
        .map(|c| format!("{}  {}  ({})", c.short_id, c.summary, c.author_name))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_diff(d: &DiffReport) -> String {
    if d.files.is_empty() {
        return "no changes".to_string();
    }
    let files = d
        .files
        .iter()
        .map(|f| format!("{} {}", f.status, f.path))
        .collect::<Vec<_>>()
        .join("\n");
    match &d.stat {
        Some(stat) => {
            let lines = stat
                .iter()
                .map(|s| format!("{} | +{} -{}", s.path, s.insertions, s.deletions))
                .collect::<Vec<_>>()
                .join("\n");
            format!("{files}\n\n{lines}")
        }
        None => files,
    }
}

#[cfg(test)]
#[path = "lib_tests/mod.rs"]
mod tests;
