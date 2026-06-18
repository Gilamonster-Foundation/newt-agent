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

use grit_lib::diff::{diff_index_to_tree, diff_index_to_worktree, DiffEntry};
use grit_lib::index::{IndexEntry, MODE_REGULAR};
use grit_lib::objects::{parse_commit, serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use grit_lib::refs::{read_head, write_ref};
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
}
