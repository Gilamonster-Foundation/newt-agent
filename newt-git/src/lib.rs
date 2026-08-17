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
                        tip = self.write_commit_on(
                            cur_parent,
                            cur_tree,
                            &cur_msgs.join("\n\n"),
                            author,
                        )?;
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
            tip = self.write_commit_on(cur_parent, cur_tree, &cur_msgs.join("\n\n"), author)?;
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
    /// model + harness build + operator/agent identity — finalized into every
    /// commit/amend/rebase message by
    /// [`CommitAttribution::finalize_message`](newt_core::attribution::CommitAttribution::finalize_message).
    /// Refreshed as late as practical before the turn that may commit (in the
    /// session loop, from the live inference model + resolved identity) so a
    /// `/model` switch is reflected in the next commit, not the one frozen at
    /// session boot. `None` only in test scaffolds that opt out of signing;
    /// the commit arms then leave the message unchanged.
    pub attribution: Option<newt_core::attribution::CommitAttribution>,
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
            Some(a) => a.finalize_message(message),
            None => message.to_string(),
        }
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
                Ok(format!("committed {}: {}", c.short_id, c.summary))
            }
            "amend" => {
                // Optional message: present → reword (signed); absent → keep
                // HEAD's existing message (which already carries its trailer, so
                // no re-sign needed).
                let msg = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .filter(|m| !m.trim().is_empty());
                let signed = msg.map(|m| self.finalize_commit_message(m));
                let c = eng
                    .amend(caps, signed.as_deref(), &self.author)
                    .map_err(s)?;
                Ok(format!("amended {}: {}", c.short_id, c.summary))
            }
            "rebase" => {
                let onto = args
                    .get("onto")
                    .and_then(|v| v.as_str())
                    .filter(|o| !o.trim().is_empty())
                    .ok_or("rebase: 'onto' (the base commit/ref to replay onto) is required")?;
                let steps = parse_rebase_plan(args, self.attribution.as_ref())?;
                if steps.is_empty() {
                    return Err("rebase: 'plan' must list at least one step".to_string());
                }
                let r = eng.rebase(caps, onto, &steps, &self.author).map_err(s)?;
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
/// `reword`/`squash` messages are finalized through the canonical
/// [`CommitAttribution`] finalizer (the tool owns signing), so rebased
/// commits keep the AI credit too.
///
/// [`CommitAttribution`]: newt_core::attribution::CommitAttribution
fn parse_rebase_plan(
    args: &serde_json::Value,
    attribution: Option<&newt_core::attribution::CommitAttribution>,
) -> Result<Vec<RebaseStep>, String> {
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
        let message = e
            .get("message")
            .and_then(|v| v.as_str())
            .filter(|m| !m.trim().is_empty())
            .map(|m| match action {
                RebaseAction::Reword | RebaseAction::Squash => match attribution {
                    Some(a) => a.finalize_message(m),
                    None => m.to_string(),
                },
                _ => m.to_string(),
            });
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
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("git runs")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// A temp repo with one commit on `a.txt`.
    fn repo_with_commit() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        std::fs::write(p.join("a.txt"), "hello\n").unwrap();
        git(p, &["add", "a.txt"]);
        git(
            p,
            &[
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "first commit",
            ],
        );
        dir
    }

    #[test]
    fn open_and_status_on_clean_repo() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let s = eng.status(&GitCaveats::top()).unwrap();
        assert!(s.clean, "fresh commit -> clean: {s:?}");
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert!(s.head.is_some());
    }

    #[test]
    fn head_snapshot_is_full_oid_cheap_identity_and_read_gated() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let snapshot = eng.head_snapshot(&GitCaveats::read_only()).unwrap();
        let commit = eng.log(&GitCaveats::read_only(), 1).unwrap().remove(0);

        assert_eq!(snapshot.branch.as_deref(), Some("main"));
        assert_eq!(snapshot.head.as_deref(), Some(commit.id.as_str()));
        assert!(snapshot.head.as_ref().unwrap().len() > 7);
        assert!(matches!(
            eng.head_snapshot(&GitCaveats::none()),
            Err(GitError::Denied("read"))
        ));
    }

    #[test]
    fn log_returns_the_commit() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let log = eng.log(&GitCaveats::top(), 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].summary, "first commit");
        assert_eq!(log[0].author_name, "Tester");
        assert_eq!(log[0].author_email, "t@example.com");
        assert!(log[0].parents.is_empty(), "root commit has no parents");
    }

    #[test]
    fn status_sees_unstaged_modification() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        let eng = GitEngine::open(dir.path()).unwrap();
        let s = eng.status(&GitCaveats::top()).unwrap();
        assert!(!s.clean);
        assert!(s.unstaged.iter().any(|f| f.path == "a.txt"));
    }

    #[test]
    fn status_sees_untracked_file() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("new.txt"), "x\n").unwrap();
        let eng = GitEngine::open(dir.path()).unwrap();
        let s = eng.status(&GitCaveats::top()).unwrap();
        assert!(s.untracked.iter().any(|p| p == "new.txt"), "{s:?}");
    }

    #[test]
    fn diff_worktree_lists_the_change() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        let eng = GitEngine::open(dir.path()).unwrap();
        let d = eng.diff(&GitCaveats::top(), DiffSpec::Worktree).unwrap();
        assert!(d.files.iter().any(|f| f.path == "a.txt"));
    }

    // -- Step 27.2: checkout (create+switch) + branch-delete ----------------

    #[test]
    fn checkout_creates_and_switches_to_a_new_branch() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let msg = eng.checkout(&GitCaveats::top(), "feat/y", true).unwrap();
        assert!(msg.contains("created and switched"), "{msg}");
        // The system git agrees HEAD now points at the new branch.
        let out = Command::new("git")
            .current_dir(dir.path())
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "feat/y");
    }

    #[test]
    fn checkout_switches_to_existing_branch_at_same_commit() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        eng.branch(&GitCaveats::top(), "feat/z").unwrap(); // ref at HEAD, HEAD stays main
        let msg = eng.checkout(&GitCaveats::top(), "feat/z", false).unwrap();
        assert_eq!(msg, "switched to branch 'feat/z'");
    }

    #[test]
    fn checkout_refuses_existing_branch_at_a_different_commit() {
        let dir = repo_with_commit();
        let p = dir.path();
        // 'ahead' is one commit past main; switching there would need a worktree
        // update, which newt does not do — it must refuse with no side effects.
        git(p, &["checkout", "-q", "-b", "ahead"]);
        std::fs::write(p.join("a.txt"), "v2\n").unwrap();
        git(p, &["add", "a.txt"]);
        git(
            p,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.c",
                "commit",
                "-q",
                "-m",
                "c2",
            ],
        );
        git(p, &["checkout", "-q", "main"]);
        let eng = GitEngine::open(p).unwrap();
        let err = eng
            .checkout(&GitCaveats::top(), "ahead", false)
            .unwrap_err();
        assert!(matches!(err, GitError::Refused(_)), "{err}");
        let out = Command::new("git")
            .current_dir(p)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "main");
    }

    #[test]
    fn checkout_missing_branch_without_create_is_refused() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let err = eng.checkout(&GitCaveats::top(), "nope", false).unwrap_err();
        assert!(matches!(err, GitError::Refused(_)), "{err}");
    }

    #[test]
    fn branch_delete_removes_a_non_current_branch() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        eng.branch(&GitCaveats::top(), "scratch").unwrap();
        let msg = eng.branch_delete(&GitCaveats::top(), "scratch").unwrap();
        assert_eq!(msg, "deleted branch 'scratch'");
        let exists = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "--verify", "--quiet", "refs/heads/scratch"])
            .status()
            .unwrap()
            .success();
        assert!(!exists, "ref must be gone after branch-delete");
    }

    #[test]
    fn branch_delete_refuses_current_branch_and_missing() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let cur = eng.branch_delete(&GitCaveats::top(), "main").unwrap_err();
        assert!(matches!(cur, GitError::Refused(_)), "{cur}");
        let missing = eng.branch_delete(&GitCaveats::top(), "ghost").unwrap_err();
        assert!(matches!(missing, GitError::Refused(_)), "{missing}");
    }

    #[test]
    fn checkout_and_branch_delete_fail_closed_without_refs() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let ro = GitCaveats::read_only();
        assert!(matches!(
            eng.checkout(&ro, "x", true),
            Err(GitError::Denied("refs"))
        ));
        assert!(matches!(
            eng.branch_delete(&ro, "x"),
            Err(GitError::Denied("refs"))
        ));
    }

    #[test]
    fn read_ops_fail_closed_without_read_capability() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let no = GitCaveats::none();
        assert!(matches!(eng.status(&no), Err(GitError::Denied("read"))));
        assert!(matches!(eng.log(&no, 1), Err(GitError::Denied("read"))));
        assert!(matches!(
            eng.diff(&no, DiffSpec::Worktree),
            Err(GitError::Denied("read"))
        ));
    }

    #[test]
    fn status_report_serde_roundtrip() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let s = eng.status(&GitCaveats::top()).unwrap();
        let json = serde_json::to_string(&s).unwrap();
        let back: StatusReport = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn add_then_commit_advances_history() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("new.txt"), "data\n").unwrap();
        let eng = GitEngine::open(dir.path()).unwrap();
        let caps = GitCaveats::top();

        let staged = eng.add(&caps, &["new.txt".to_string()]).unwrap();
        assert_eq!(staged, vec!["new.txt".to_string()]);
        assert!(eng
            .status(&caps)
            .unwrap()
            .staged
            .iter()
            .any(|f| f.path == "new.txt"));

        let author = Author {
            name: "Bot".into(),
            email: "bot@newt.dev".into(),
        };
        let c = eng.commit(&caps, "add new file", &author).unwrap();
        assert_eq!(c.summary, "add new file");
        assert_eq!(c.author_name, "Bot");

        let log = eng.log(&caps, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].summary, "add new file");
        assert!(eng.status(&caps).unwrap().clean, "clean after commit");

        // The system `git` agrees grit wrote a real, readable history.
        let out = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2);
    }

    #[test]
    fn branch_creates_a_ref_at_head() {
        let dir = repo_with_commit();
        let eng = GitEngine::open(dir.path()).unwrap();
        let refname = eng.branch(&GitCaveats::top(), "feat/x").unwrap();
        assert_eq!(refname, "refs/heads/feat/x");
        let ok = Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "--verify", "refs/heads/feat/x"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "branch ref must resolve under the system git too");
    }

    #[test]
    fn writes_fail_closed_without_capability() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("new.txt"), "x\n").unwrap();
        let eng = GitEngine::open(dir.path()).unwrap();
        let author = Author {
            name: "B".into(),
            email: "b@b".into(),
        };

        let ro = GitCaveats::read_only();
        assert!(matches!(
            eng.add(&ro, &["new.txt".to_string()]),
            Err(GitError::Denied("stage"))
        ));
        assert!(matches!(
            eng.commit(&ro, "m", &author),
            Err(GitError::Denied("commit"))
        ));
        assert!(matches!(
            eng.branch(&ro, "x"),
            Err(GitError::Denied("refs"))
        ));

        // Stage-but-not-commit: add is allowed, commit is refused.
        let stage_only = GitCaveats {
            commit_local: false,
            ..GitCaveats::top()
        };
        assert!(eng.add(&stage_only, &["new.txt".to_string()]).is_ok());
        assert!(matches!(
            eng.commit(&stage_only, "m", &author),
            Err(GitError::Denied("commit"))
        ));
    }

    // --- LocalGitTool (the injected GitTool seam) ---------------------------

    use newt_core::agentic::GitTool as _;

    fn tool(dir: &Path) -> LocalGitTool {
        LocalGitTool {
            root: dir.to_path_buf(),
            author: Author {
                name: "newt-agent[bot]".into(),
                email: "bot@example.com".into(),
            },
            // The canonical attribution the session would refresh from the live
            // model + resolved identity. `from_runtime` is tool-less, so this
            // is deterministic in tests (no wall clock, no subprocess).
            attribution: Some(newt_core::attribution::CommitAttribution::from_runtime(
                "qwen3:30b",
                None,
                "noreply@newt-agent.com",
            )),
        }
    }

    #[test]
    fn dispatch_init_creates_a_repo_in_a_non_repo_dir_then_commit_works() {
        // Regression: before `op=init`, the embedded git tool was only advertised
        // inside an existing repo and had NO way to create one — an agent in a
        // fresh dir saw no git tool and gave up committing. `init` makes the tool
        // useful there. (Would previously fail: "unknown git op 'init'".)
        let dir = tempfile::tempdir().unwrap();
        assert!(
            GitEngine::open(dir.path()).is_err(),
            "precondition: not a repo yet"
        );
        let t = tool(dir.path());
        let out = t
            .dispatch("init", &serde_json::json!({}), &GitCaveats::top())
            .unwrap();
        assert!(out.contains("initialized"), "got: {out}");
        assert!(
            GitEngine::open(dir.path()).is_ok(),
            "init created a real, openable repo"
        );
        // ...and the rest of the tool now works against the fresh repo.
        std::fs::write(dir.path().join("f.txt"), "x\n").unwrap();
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["f.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        let c = t
            .dispatch(
                "commit",
                &serde_json::json!({"message": "first"}),
                &GitCaveats::top(),
            )
            .unwrap();
        assert!(c.contains("committed"), "got: {c}");
    }

    #[test]
    fn dispatch_init_is_idempotent_on_an_existing_repo() {
        let dir = repo_with_commit();
        let out = tool(dir.path())
            .dispatch("init", &serde_json::json!({}), &GitCaveats::top())
            .unwrap();
        assert!(out.contains("already a repository"), "got: {out}");
    }

    #[test]
    fn dispatch_init_is_denied_without_write_permission() {
        let dir = tempfile::tempdir().unwrap();
        let res =
            tool(dir.path()).dispatch("init", &serde_json::json!({}), &GitCaveats::read_only());
        assert!(res.is_err(), "read-only session must not create a repo");
        assert!(
            GitEngine::open(dir.path()).is_err(),
            "a denied init created nothing"
        );
    }

    #[test]
    fn dispatch_checkout_creates_branch_and_branch_delete_removes_it() {
        let dir = repo_with_commit();
        let t = tool(dir.path());
        // checkout defaults create=true → `checkout -b`.
        let out = t
            .dispatch(
                "checkout",
                &serde_json::json!({"name": "feat/dispatch"}),
                &GitCaveats::top(),
            )
            .unwrap();
        assert!(out.contains("created and switched"), "{out}");
        // Switch back to main (same commit), then delete the scratch branch.
        t.dispatch(
            "checkout",
            &serde_json::json!({"name": "main", "create": false}),
            &GitCaveats::top(),
        )
        .unwrap();
        let del = t
            .dispatch(
                "branch-delete",
                &serde_json::json!({"name": "feat/dispatch"}),
                &GitCaveats::top(),
            )
            .unwrap();
        assert_eq!(del, "deleted branch 'feat/dispatch'");
    }

    #[test]
    fn dispatch_unknown_op_lists_the_supported_ops() {
        let dir = repo_with_commit();
        let t = tool(dir.path());
        // 'pull' is no longer advertised or implemented (local-only).
        let err = t
            .dispatch("pull", &serde_json::json!({}), &GitCaveats::top())
            .unwrap_err();
        assert!(err.contains("unknown git op 'pull'"), "{err}");
        assert!(err.contains("checkout"), "{err}");
        assert!(err.contains("branch-delete"), "{err}");
    }

    /// #1709 integration: the tool's commit-message attribution now flows
    /// through ONE boundary — [`LocalGitTool::finalize_commit_message`] →
    /// [`CommitAttribution::finalize_message`] — not the removed
    /// `sign_message`/`attribution_block` pair. The model may supply a bare
    /// subject with zero attribution text; the harness owns the trailer +
    /// provenance, deterministically (no wall clock) and idempotently.
    ///
    /// [`CommitAttribution`]: newt_core::attribution::CommitAttribution
    #[test]
    fn finalize_commit_message_owns_attribution_deterministically() {
        let t = tool(Path::new(".")); // root unused by finalize_commit_message
                                      // Bare subject, zero attribution text → canonical trailer + provenance.
        let out = t.finalize_commit_message("fix the parser");
        let version = newt_core::build_info::PACKAGE_VERSION;
        assert!(
            out.contains(&format!(
                "Co-authored-by: qwen3:30b (newt-agent v{version}) <noreply@newt-agent.com>"
            )),
            "canonical model trailer rendered from the typed value: {out}"
        );
        assert!(
            out.contains("Harness: newt-agent v")
                && out.contains(" | Model: qwen3:30b | Operator: "),
            "canonical provenance line rendered from the same value: {out}"
        );
        assert!(
            !out.contains("Time:"),
            "no wall-clock field (deterministic): {out}"
        );
        assert!(
            out.starts_with("fix the parser\n\n"),
            "subject preserved verbatim: {out}"
        );
        // A legitimate third-party co-author is preserved verbatim.
        let with_third = "feat: x\n\nCo-authored-by: someone <a@b.c>";
        let out2 = t.finalize_commit_message(with_third);
        assert!(
            out2.contains("Co-authored-by: someone <a@b.c>"),
            "third-party kept: {out2}"
        );
        assert!(
            out2.contains("Co-authored-by: qwen3:30b"),
            "newt model trailer added: {out2}"
        );
        // Idempotent: re-finalizing the finalized message yields the same bytes.
        assert_eq!(
            t.finalize_commit_message(&out),
            out,
            "idempotent re-finalization"
        );
        // No attribution configured → message unchanged (test opt-out path).
        let mut t2 = t;
        t2.attribution = None;
        assert_eq!(t2.finalize_commit_message("m"), "m");
    }

    #[test]
    fn commit_carries_the_coauthor_trailer_in_the_message() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("c.txt"), "x\n").unwrap();
        let t = tool(dir.path());
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["c.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        t.dispatch(
            "commit",
            &serde_json::json!({"message": "add c"}),
            &GitCaveats::top(),
        )
        .unwrap();
        // Inspect the real commit message via system git.
        let log = Command::new("git")
            .current_dir(dir.path())
            .args(["log", "-1", "--pretty=%B"])
            .output()
            .unwrap();
        let body = String::from_utf8_lossy(&log.stdout);
        assert!(body.contains("add c"), "subject present: {body}");
        // The canonical harness-managed trailer + provenance, rendered from the
        // typed CommitAttribution (real package version, the configured email).
        let version = newt_core::build_info::PACKAGE_VERSION;
        assert!(
            body.contains(&format!(
                "Co-authored-by: qwen3:30b (newt-agent v{version}) <noreply@newt-agent.com>"
            )),
            "canonical model trailer present: {body}"
        );
        assert!(
            body.contains("Harness: newt-agent v")
                && body.contains(" | Model: qwen3:30b | Operator: "),
            "canonical provenance line present: {body}"
        );
    }

    /// #1709 acceptance condition: a model may supply a bare subject —
    /// "fix the parser" — with ZERO attribution text, and the resulting
    /// first-class Newt commit still carries correct harness-managed
    /// attribution (the canonical model trailer + provenance, rendered from
    /// the typed `CommitAttribution` through the one shared finalizer
    /// boundary). Grounds the mocked `finalize_commit_message` test against a
    /// real commit read back via system git.
    #[test]
    fn bare_model_subject_still_gets_harness_managed_attribution() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("p.txt"), "x\n").unwrap();
        let t = tool(dir.path());
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["p.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        // Bare subject, no attribution text whatsoever from the model.
        t.dispatch(
            "commit",
            &serde_json::json!({"message": "fix the parser"}),
            &GitCaveats::top(),
        )
        .unwrap();
        let body = head_message(dir.path());
        assert!(body.contains("fix the parser"), "subject preserved: {body}");
        let version = newt_core::build_info::PACKAGE_VERSION;
        assert!(
            body.contains(&format!(
                "Co-authored-by: qwen3:30b (newt-agent v{version}) <noreply@newt-agent.com>"
            )),
            "canonical model trailer added by the harness: {body}"
        );
        assert!(
            body.contains(" | Model: qwen3:30b | Operator: "),
            "canonical provenance line added by the harness: {body}"
        );
    }

    /// #551 regression at the commit boundary: a `/model` switch between two
    /// commits must attribute EACH commit to the model actually driving it.
    /// The second commit carries model B — not a stale model A frozen earlier
    /// — and the first commit is NOT retroactively rewritten. This crosses the
    /// `/model` → `session_git_tool.attribution` → `finalize_commit_message`
    /// boundary at the REAL commit level: the unit-tier
    /// `fresh_construction_reflects_a_model_switch` proves the construction
    /// half (a fresh `CommitAttribution` sees the new model); this proves the
    /// WIRED commit half. It mirrors the per-loop-iteration refresh in
    /// `newt-tui::chat` (`tool.attribution = from_identity(&inf_model, …)`)
    /// by refreshing `tool.attribution` between commits, then reads each
    /// commit back via system git. Real-resource (real git) → grounds the
    /// mocked `finalize_commit_message` tests against actual history.
    #[test]
    fn model_switch_between_commits_attributes_each_to_the_live_model() {
        let dir = repo_with_commit();
        let p = dir.path();
        // Session boots under model A.
        let mut t = tool(p);
        t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
            "model-a",
            None,
            "noreply@newt-agent.com",
        ));

        // Commit C1 under model A.
        std::fs::write(p.join("c1.txt"), "x\n").unwrap();
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["c1.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        t.dispatch(
            "commit",
            &serde_json::json!({"message": "c1 under model A"}),
            &GitCaveats::top(),
        )
        .unwrap();
        let body_c1 = head_message(p);
        assert!(
            body_c1.contains(" | Model: model-a | "),
            "C1 → model A: {body_c1}"
        );
        assert!(!body_c1.contains("model-b"), "C1 has no model B: {body_c1}");

        // `/model model-b`: refresh the tool's attribution, exactly as the chat
        // loop does at the top of the next iteration before the turn's ChatCtx.
        t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
            "model-b",
            None,
            "noreply@newt-agent.com",
        ));
        // Commit C2 under model B.
        std::fs::write(p.join("c2.txt"), "x\n").unwrap();
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["c2.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        t.dispatch(
            "commit",
            &serde_json::json!({"message": "c2 under model B"}),
            &GitCaveats::top(),
        )
        .unwrap();
        let body_c2 = head_message(p);
        assert!(
            body_c2.contains(" | Model: model-b | "),
            "C2 → the LIVE model B at commit time: {body_c2}"
        );
        assert!(
            !body_c2.contains("model-a"),
            "model A does NOT survive as stale Newt attribution on C2 (#551): {body_c2}"
        );

        // The switch did not retroactively rewrite C1 — model A is still there.
        let c1_again = Command::new("git")
            .current_dir(p)
            .args(["log", "--pretty=%B", "--skip=1", "-1"])
            .output()
            .unwrap();
        let body_c1_still = String::from_utf8_lossy(&c1_again.stdout).to_string();
        assert!(
            body_c1_still.contains(" | Model: model-a | "),
            "C1 unchanged after the switch (no backward leakage): {body_c1_still}"
        );
        assert!(
            !body_c1_still.contains("model-b"),
            "C1 not corrupted with model B: {body_c1_still}"
        );

        // `/model model-a` back to a previous model (req 7): switching back works.
        t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
            "model-a",
            None,
            "noreply@newt-agent.com",
        ));
        std::fs::write(p.join("c3.txt"), "x\n").unwrap();
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["c3.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        t.dispatch(
            "commit",
            &serde_json::json!({"message": "c3 back under model A"}),
            &GitCaveats::top(),
        )
        .unwrap();
        let body_c3 = head_message(p);
        assert!(
            body_c3.contains(" | Model: model-a | "),
            "C3 → model A after switching back to a previous model: {body_c3}"
        );
    }

    /// #551 for the amend path (req 8): amending after a `/model` switch
    /// re-signs the commit with the LIVE model's attribution, not the stale
    /// model that authored the original commit. The amend arm calls
    /// `finalize_commit_message` with the current `tool.attribution`, so the
    /// switched model is what lands. Real-resource (real git).
    #[test]
    fn amend_after_a_model_switch_resigns_with_the_live_model() {
        let dir = repo_with_commit();
        let p = dir.path();
        let mut t = tool(p);
        t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
            "model-a",
            None,
            "noreply@newt-agent.com",
        ));
        std::fs::write(p.join("c1.txt"), "x\n").unwrap();
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["c1.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        t.dispatch(
            "commit",
            &serde_json::json!({"message": "orig under model A"}),
            &GitCaveats::top(),
        )
        .unwrap();
        assert!(head_message(p).contains(" | Model: model-a | "));

        // `/model model-b`, then amend the commit.
        t.attribution = Some(newt_core::attribution::CommitAttribution::from_runtime(
            "model-b",
            None,
            "noreply@newt-agent.com",
        ));
        std::fs::write(p.join("c2.txt"), "x\n").unwrap();
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["c2.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        t.dispatch(
            "amend",
            &serde_json::json!({"message": "reworded under model B"}),
            &GitCaveats::top(),
        )
        .unwrap();
        let body = head_message(p);
        assert!(
            body.contains(" | Model: model-b | "),
            "amended commit → the live model B: {body}"
        );
        assert!(
            !body.contains("model-a"),
            "stale model A did NOT survive the amend (#551): {body}"
        );
        assert!(
            body.contains("reworded under model B"),
            "amend subject preserved: {body}"
        );
    }

    #[test]
    fn local_git_tool_status_renders_readable_text() {
        let dir = repo_with_commit();
        let t = tool(dir.path());
        let out = t
            .dispatch("status", &serde_json::json!({}), &GitCaveats::top())
            .unwrap();
        assert!(out.contains("on branch main"), "got: {out}");
        assert!(out.contains("working tree clean"), "got: {out}");
    }

    #[test]
    fn local_git_tool_log_lists_commits() {
        let dir = repo_with_commit();
        let t = tool(dir.path());
        let out = t
            .dispatch("log", &serde_json::json!({"limit": 5}), &GitCaveats::top())
            .unwrap();
        assert!(out.contains("first commit"), "got: {out}");
    }

    #[test]
    fn local_git_tool_add_then_commit_succeeds_when_permitted() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        let t = tool(dir.path());
        let staged = t
            .dispatch(
                "add",
                &serde_json::json!({"paths": ["b.txt"]}),
                &GitCaveats::top(),
            )
            .unwrap();
        assert!(staged.contains("b.txt"), "got: {staged}");
        let committed = t
            .dispatch(
                "commit",
                &serde_json::json!({"message": "add b"}),
                &GitCaveats::top(),
            )
            .unwrap();
        assert!(committed.starts_with("committed "), "got: {committed}");
        assert!(committed.contains("add b"), "got: {committed}");
    }

    #[test]
    fn local_git_tool_amend_rewords_head_without_adding_a_commit() {
        let dir = repo_with_commit();
        std::fs::write(dir.path().join("d.txt"), "d\n").unwrap();
        let t = tool(dir.path());
        t.dispatch(
            "add",
            &serde_json::json!({"paths": ["d.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
        t.dispatch(
            "commit",
            &serde_json::json!({"message": "add d"}),
            &GitCaveats::top(),
        )
        .unwrap();
        let count_before = commit_count(dir.path());

        // Reword the last commit.
        let out = t
            .dispatch(
                "amend",
                &serde_json::json!({"message": "add d (reworded)"}),
                &GitCaveats::top(),
            )
            .unwrap();
        assert!(out.starts_with("amended "), "got: {out}");
        // Same number of commits (HEAD replaced, not stacked).
        assert_eq!(commit_count(dir.path()), count_before);
        // The new subject is in HEAD.
        let body = head_message(dir.path());
        assert!(body.contains("add d (reworded)"), "got: {body}");
        assert!(
            body.contains("Co-authored-by: qwen3:30b"),
            "amend re-signs the new message: {body}"
        );
    }

    #[test]
    fn local_git_tool_amend_keeps_message_when_omitted() {
        let dir = repo_with_commit();
        let t = tool(dir.path());
        // Amend with no message → keep "first commit".
        t.dispatch("amend", &serde_json::json!({}), &GitCaveats::top())
            .unwrap();
        assert!(head_message(dir.path()).contains("first commit"));
    }

    #[test]
    fn local_git_tool_amend_denied_on_read_only() {
        let dir = repo_with_commit();
        let t = tool(dir.path());
        let err = t
            .dispatch(
                "amend",
                &serde_json::json!({"message": "x"}),
                &GitCaveats::read_only(),
            )
            .unwrap_err();
        assert!(
            err.contains("denied") && err.contains("commit"),
            "got: {err}"
        );
    }

    fn commit_count(dir: &Path) -> usize {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }

    fn head_message(dir: &Path) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["log", "-1", "--pretty=%B"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[test]
    fn local_git_tool_commit_denied_on_read_only_caveats() {
        let dir = repo_with_commit();
        let t = tool(dir.path());
        // read_only permits status/log/diff but never a commit.
        let err = t
            .dispatch(
                "commit",
                &serde_json::json!({"message": "nope"}),
                &GitCaveats::read_only(),
            )
            .unwrap_err();
        assert!(
            err.contains("denied") && err.contains("commit"),
            "got: {err}"
        );
        // …but a read op is allowed under the same caveats.
        assert!(t
            .dispatch("status", &serde_json::json!({}), &GitCaveats::read_only())
            .is_ok());
    }

    // --- rebase (structured plan) ------------------------------------------

    /// A repo with three linear commits c1→c2→c3 (a/b/c.txt). Returns the dir
    /// and the full oids [c1, c2, c3].
    fn repo_with_three() -> (tempfile::TempDir, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        let mk = |name: &str, content: &str, msg: &str| {
            std::fs::write(p.join(name), content).unwrap();
            git(p, &["add", name]);
            git(
                p,
                &[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@e.c",
                    "commit",
                    "-q",
                    "-m",
                    msg,
                ],
            );
        };
        mk("a.txt", "v1\n", "c1");
        mk("b.txt", "b\n", "c2");
        mk("c.txt", "c\n", "c3");
        let out = Command::new("git")
            .current_dir(p)
            .args(["log", "--format=%H", "--reverse"])
            .output()
            .unwrap();
        let oids = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(String::from)
            .collect();
        (dir, oids)
    }

    #[test]
    fn rebase_rewords_a_middle_commit() {
        let (dir, oids) = repo_with_three();
        let t = tool(dir.path());
        let out = t
            .dispatch(
                "rebase",
                &serde_json::json!({
                    "onto": oids[0],
                    "plan": [
                        {"commit": oids[1], "action": "reword", "message": "b reworded"},
                        {"commit": oids[2], "action": "pick"},
                    ]
                }),
                &GitCaveats::top(),
            )
            .unwrap();
        assert!(out.starts_with("rebased onto"), "got: {out}");
        assert_eq!(commit_count(dir.path()), 3, "same number of commits");
        // History: c1, b reworded, c3.
        let log = Command::new("git")
            .current_dir(dir.path())
            .args(["log", "--format=%s", "--reverse"])
            .output()
            .unwrap();
        let subjects = String::from_utf8_lossy(&log.stdout);
        assert!(subjects.contains("b reworded"), "got: {subjects}");
        assert!(
            !subjects.contains("\nc2\n"),
            "old c2 subject gone: {subjects}"
        );
        // b.txt and c.txt still present (changes preserved).
    }

    #[test]
    fn rebase_squashes_two_commits_into_one() {
        let (dir, oids) = repo_with_three();
        let t = tool(dir.path());
        t.dispatch(
            "rebase",
            &serde_json::json!({
                "onto": oids[0],
                "plan": [
                    {"commit": oids[1], "action": "pick"},
                    {"commit": oids[2], "action": "squash", "message": "folded note"},
                ]
            }),
            &GitCaveats::top(),
        )
        .unwrap();
        // c1 + one squashed commit = 2.
        assert_eq!(commit_count(dir.path()), 2);
        // The squashed commit carries both messages.
        let body = head_message(dir.path());
        assert!(
            body.contains("c2") && body.contains("folded note"),
            "got: {body}"
        );
        // Both files landed in the squashed tree.
        let files = Command::new("git")
            .current_dir(dir.path())
            .args(["ls-tree", "--name-only", "-r", "HEAD"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&files.stdout);
        assert!(
            names.contains("b.txt") && names.contains("c.txt"),
            "got: {names}"
        );
    }

    #[test]
    fn rebase_drops_a_commit() {
        let (dir, oids) = repo_with_three();
        let t = tool(dir.path());
        t.dispatch(
            "rebase",
            &serde_json::json!({
                "onto": oids[0],
                "plan": [
                    {"commit": oids[1], "action": "pick"},
                    {"commit": oids[2], "action": "drop"},
                ]
            }),
            &GitCaveats::top(),
        )
        .unwrap();
        assert_eq!(commit_count(dir.path()), 2);
        let names = Command::new("git")
            .current_dir(dir.path())
            .args(["ls-tree", "--name-only", "-r", "HEAD"])
            .output()
            .unwrap();
        let names = String::from_utf8_lossy(&names.stdout);
        assert!(
            !names.contains("c.txt"),
            "dropped commit's file gone: {names}"
        );
    }

    #[test]
    fn rebase_aborts_on_conflict_leaving_the_branch_unchanged() {
        // c1: a=v1; c2: a=v2; c3: a=v3. Cherry-picking c3 onto c1 conflicts
        // (both c1 and c3 changed a.txt from c2's base).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q", "-b", "main"]);
        let mk = |content: &str, msg: &str| {
            std::fs::write(p.join("a.txt"), content).unwrap();
            git(p, &["add", "a.txt"]);
            git(
                p,
                &[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@e.c",
                    "commit",
                    "-q",
                    "-m",
                    msg,
                ],
            );
        };
        mk("v1\n", "c1");
        mk("v2\n", "c2");
        mk("v3\n", "c3");
        let head_before = Command::new("git")
            .current_dir(p)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let oids: Vec<String> = String::from_utf8_lossy(
            &Command::new("git")
                .current_dir(p)
                .args(["log", "--format=%H", "--reverse"])
                .output()
                .unwrap()
                .stdout,
        )
        .lines()
        .map(String::from)
        .collect();
        let t = tool(p);
        let err = t
            .dispatch(
                "rebase",
                &serde_json::json!({
                    "onto": oids[0],
                    "plan": [{"commit": oids[2], "action": "pick"}]
                }),
                &GitCaveats::top(),
            )
            .unwrap_err();
        assert!(
            err.contains("conflict") && err.contains("aborted"),
            "got: {err}"
        );
        // The branch ref did NOT move.
        let head_after = Command::new("git")
            .current_dir(p)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            head_before.stdout, head_after.stdout,
            "branch must be unchanged"
        );
    }

    #[test]
    fn rebase_denied_on_read_only() {
        let (dir, oids) = repo_with_three();
        let t = tool(dir.path());
        let err = t
            .dispatch(
                "rebase",
                &serde_json::json!({"onto": oids[0], "plan": [{"commit": oids[1], "action": "pick"}]}),
                &GitCaveats::read_only(),
            )
            .unwrap_err();
        assert!(
            err.contains("denied") && err.contains("commit"),
            "got: {err}"
        );
    }

    #[test]
    fn local_git_tool_unknown_op_and_missing_args_error() {
        let dir = repo_with_commit();
        let t = tool(dir.path());
        let err = t
            .dispatch("frobnicate", &serde_json::json!({}), &GitCaveats::top())
            .unwrap_err();
        assert!(err.contains("unknown git op"), "got: {err}");
        // commit without a message is a clear arg error, not a panic.
        let err = t
            .dispatch("commit", &serde_json::json!({}), &GitCaveats::top())
            .unwrap_err();
        assert!(err.contains("message"), "got: {err}");
    }

    #[test]
    fn stash_push_resets_worktree_and_pop_restores() {
        // Regression (#992): pure-Rust `git stash` — push saves tracked changes
        // (worktree back to HEAD) + lists them; pop restores + drops the entry.
        let dir = repo_with_commit();
        let p = dir.path();
        std::fs::write(p.join("a.txt"), "changed\n").unwrap(); // dirty a tracked file
        let eng = GitEngine::open(p).unwrap();
        let author = Author {
            name: "T".into(),
            email: "t@e.x".into(),
        };
        let out = eng.stash_push(&GitCaveats::top(), &author).unwrap();
        assert!(out.contains("Saved working directory"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(p.join("a.txt")).unwrap(),
            "hello\n",
            "worktree reset to HEAD after push"
        );
        let list = eng.stash_list(&GitCaveats::top()).unwrap();
        assert_eq!(list.len(), 1, "one stash entry: {list:?}");
        assert!(list[0].starts_with("stash@{0}:"), "{}", list[0]);

        let out = eng.stash_pop(&GitCaveats::top(), 0).unwrap();
        assert!(out.contains("popped"), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(p.join("a.txt")).unwrap(),
            "changed\n",
            "pop restored the stashed change"
        );
        assert!(
            eng.stash_list(&GitCaveats::top()).unwrap().is_empty(),
            "entry dropped after a clean pop"
        );
    }

    #[test]
    fn stash_is_a_known_op_and_write_gated() {
        // Regression (#992): `git: stash` was "unknown git op 'stash'".
        let dir = repo_with_commit();
        let p = dir.path();
        let t = tool(p);
        let out = t
            .dispatch("stash-list", &serde_json::json!({}), &GitCaveats::top())
            .unwrap();
        assert!(
            !out.contains("unknown git op"),
            "stash-list recognized: {out}"
        );
        // Push is a write → denied under read-only caps (fail-closed like commit).
        std::fs::write(p.join("a.txt"), "dirty\n").unwrap();
        let err = t
            .dispatch("stash", &serde_json::json!({}), &GitCaveats::read_only())
            .unwrap_err();
        assert!(
            err.contains("not permitted"),
            "read-only denies stash push: {err}"
        );
    }
}
