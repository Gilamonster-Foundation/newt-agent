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

use newt_core::git_caveats::GitCaveats;
use serde::{Deserialize, Serialize};
use std::path::Path;

use grit_lib::diff::{diff_index_to_tree, diff_index_to_worktree, DiffEntry, DiffStatus};
use grit_lib::index::{entry_from_stat, IndexEntry, MODE_REGULAR};
use grit_lib::merge_base::resolve_commit_specs;
use grit_lib::merge_file::MergeFavor;
use grit_lib::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::stash::apply_stash;
use grit_lib::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use grit_lib::reflog::{delete_reflog_entries, read_reflog};
use grit_lib::refs::{
    append_reflog, delete_ref, read_head, reflog_file_path, resolve_ref, write_ref,
    write_symbolic_ref,
};
use grit_lib::repo::Repository;
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
}

/// Which diff to compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSpec {
    /// Unstaged: worktree vs index.
    Worktree,
    /// Staged: index vs HEAD tree.
    Staged,
}

/// An embedded git engine bound to one repository.
pub struct GitEngine {
    repo: Repository,
}

impl GitEngine {
    /// Discover and open the repository containing `root` (walks up for `.git`).
    pub fn open(root: &Path) -> Result<Self, GitError> {
        let repo = Repository::discover(Some(root))?;
        Ok(Self { repo })
    }

    fn head_oid(&self) -> Result<Option<ObjectId>, GitError> {
        Ok(match resolve_head(&self.repo.git_dir)? {
            HeadState::Branch { oid, .. } => oid,
            HeadState::Detached { oid } => Some(oid),
            HeadState::Invalid => None,
        })
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
    /// This is the bounded observation primitive used by provenance hooks; it
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
                let unstaged = diff_index_to_worktree(&self.repo.odb, &index, &wt, false, false)?;
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

    /// `git log` — a first-parent walk from HEAD, up to `limit` commits. Requires `read`.
    pub fn log(&self, caps: &GitCaveats, limit: usize) -> Result<Vec<CommitInfo>, GitError> {
        if !caps.permits_read() {
            return Err(GitError::Denied("read"));
        }
        let mut out = Vec::new();
        let mut next = self.head_oid()?;
        while let Some(oid) = next {
            if out.len() >= limit {
                break;
            }
            let obj = self.repo.odb.read(&oid)?;
            let commit = parse_commit(&obj.data)?;
            out.push(commit_info(&oid, &commit));
            next = commit.parents.first().cloned();
        }
        Ok(out)
    }

    /// `git diff` for the given `spec`. Requires `read`.
    pub fn diff(&self, caps: &GitCaveats, spec: DiffSpec) -> Result<DiffReport, GitError> {
        if !caps.permits_read() {
            return Err(GitError::Denied("read"));
        }
        let index = self.repo.load_index()?;
        let entries = match spec {
            DiffSpec::Worktree => match self.repo.work_tree.clone() {
                Some(wt) => diff_index_to_worktree(&self.repo.odb, &index, &wt, false, false)?,
                None => Vec::new(),
            },
            DiffSpec::Staged => {
                let tree = self.head_tree()?;
                diff_index_to_tree(&self.repo.odb, &index, tree.as_ref(), false)?
            }
        };
        Ok(DiffReport {
            files: entries.iter().map(file_change).collect(),
        })
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
        let index = self.repo.load_index()?;
        let tree = write_tree_from_index(&self.repo.odb, &index, "")?;
        let parents: Vec<ObjectId> = self.head_oid()?.into_iter().collect();
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
        let oid = self
            .repo
            .odb
            .write(ObjectKind::Commit, &serialize_commit(&commit))?;
        match read_head(&self.repo.git_dir)? {
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
        let oid = self
            .repo
            .odb
            .write(ObjectKind::Commit, &serialize_commit(&commit))?;
        match read_head(&self.repo.git_dir)? {
            Some(branch_ref) => write_ref(&self.repo.git_dir, &branch_ref, &oid)?,
            None => return Err(GitError::Unsupported("cannot amend on a detached HEAD")),
        }
        Ok(commit_info(&oid, &commit))
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
        Ok(self
            .repo
            .odb
            .write(ObjectKind::Commit, &serialize_commit(&commit))?)
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
        let head_ref = read_head(&self.repo.git_dir)?
            .ok_or(GitError::Unsupported("cannot rebase on a detached HEAD"))?;
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
            produced += 1;
        }
        // The single mutating step: advance the branch ref to the new tip.
        write_ref(&self.repo.git_dir, &head_ref, &tip)?;
        Ok(RebaseReport {
            new_head: short_oid(&tip),
            produced,
            dropped,
        })
    }

    /// `git branch <name>` — create `refs/heads/<name>` at the current HEAD commit.
    /// Requires `refs` to permit that ref name. Returns the full ref name.
    pub fn branch(&self, caps: &GitCaveats, name: &str) -> Result<String, GitError> {
        let refname = format!("refs/heads/{name}");
        if !caps.permits_ref(&refname) {
            return Err(GitError::Denied("refs"));
        }
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
    /// (always true for a freshly-created branch). Switching to a branch at a
    /// different commit is *refused* rather than silently leaving the worktree
    /// stale — no side effects on refusal.
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
            return Err(GitError::Refused(format!(
                "refusing to switch to '{name}': it points at a different commit \
                 than HEAD and newt cannot update the working tree (local-only). \
                 Commit or stash first, or create a new branch at HEAD."
            )));
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
        let unstaged = diff_index_to_worktree(&self.repo.odb, &index, &wt, false, false)?;
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
}

impl LocalGitTool {
    /// Capability-governed, O(HEAD) repository identity for harness
    /// provenance. Opening afresh matches [`GitTool::dispatch`]'s stateless
    /// behavior and never invents read authority.
    pub fn head_snapshot(&self, caps: &GitCaveats) -> Result<HeadSnapshot, GitError> {
        GitEngine::open(&self.root)?.head_snapshot(caps)
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
    ) -> Result<String, String> {
        // `init` CREATES a repo, so it runs BEFORE opening one — every other op
        // requires an existing repo (`GitEngine::open` below). It is a write:
        // gate it on the commit/write capability so a read-only session cannot
        // create a repo. This is what lets the tool be advertised (and useful)
        // in a not-yet-a-repo workspace instead of silently disappearing.
        if op == "init" {
            if !caps.permits_commit() {
                return Err(GitError::Denied("init").to_string());
            }
            if GitEngine::open(&self.root).is_ok() {
                return Ok("git: already a repository here".into());
            }
            grit_lib::repo::init_repository(&self.root, false, "main", None, "files")
                .map_err(|e| format!("init failed: {e}"))?;
            return Ok("initialized empty git repository on branch 'main'".into());
        }
        let eng = GitEngine::open(&self.root).map_err(|e| e.to_string())?;
        let s = |e: GitError| e.to_string();
        match op {
            "status" => Ok(render_status(&eng.status(caps).map_err(s)?)),
            "log" => {
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                Ok(render_log(&eng.log(caps, limit).map_err(s)?))
            }
            "diff" => {
                let spec = match args.get("spec").and_then(|v| v.as_str()) {
                    Some("staged") => DiffSpec::Staged,
                    _ => DiffSpec::Worktree,
                };
                Ok(render_diff(&eng.diff(caps, spec).map_err(s)?))
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
                 branch|checkout|branch-delete|stash|stash-list|stash-pop|stash-apply|stash-drop)"
            )),
        }
    }
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
    d.files
        .iter()
        .map(|f| format!("{} {}", f.status, f.path))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "lib_tests/mod.rs"]
mod tests;
