//! `newt-git` — the embedded git engine for newt agents.
//!
//! Wraps [`grit-lib`](https://crates.io/crates/grit-lib) (a pure-Rust, from-scratch
//! git reimplementation, MIT) behind the [`GitCaveats`](newt_core::git_caveats::GitCaveats)
//! OCAP surface. Every operation takes the already-composed `&GitCaveats` and **fails
//! closed**; results are this crate's **own** structured serde models, converted at the
//! grit-lib boundary — so grit-lib's pre-1.0 API churn is contained to this one crate
//! and never leaks into the rest of the workspace.
//!
//! **PR2 scope: safe LOCAL READ ops** (`open`/`status`/`log`/`diff`). Local writes
//! (`add`/`commit`/`branch`) are PR3; network ops (`clone`/`fetch`/`push`) are PR5 —
//! fail-closed under the OCAP deviation ratchet. We depend ONLY on the MIT `grit-lib`,
//! never the GPL-2.0 `grit-legacy`.

use newt_core::git_caveats::GitCaveats;
use serde::{Deserialize, Serialize};
use std::path::Path;

use grit_lib::diff::{diff_index_to_tree, diff_index_to_worktree, DiffEntry};
use grit_lib::objects::{parse_commit, CommitData, ObjectId};
use grit_lib::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};

/// Errors from the embedded git engine.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The [`GitCaveats`] surface denied this operation class (fail-closed).
    #[error("capability denied: git {0} not permitted")]
    Denied(&'static str),
    /// The underlying grit-lib engine failed.
    #[error("git: {0}")]
    Engine(#[from] grit_lib::error::Error),
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
}
