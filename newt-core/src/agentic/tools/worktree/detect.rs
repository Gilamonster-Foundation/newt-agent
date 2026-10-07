//! #2791: discover the result of a command without interpreting its shell syntax.
//! This is advisory task routing, never confinement adoption or a grant.
use crate::worktree_adoption::{common_git_dir, WorktreeSession};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
};

pub(super) struct Snapshot {
    directory: PathBuf,
    names: BTreeSet<OsString>,
}

fn names(directory: &Path) -> Option<BTreeSet<OsString>> {
    match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(BTreeSet::new()),
        Err(_) => None,
    }
}

impl Snapshot {
    pub(super) fn before(workspace: &Path, read: &crate::Scope<String>) -> Option<Self> {
        let directory = common_git_dir(workspace)?.join("worktrees");
        if !crate::permits_path(read, &directory.to_string_lossy()) {
            return None;
        }
        Some(Self {
            names: names(&directory)?,
            directory,
        })
    }

    pub(super) fn record(
        self,
        session: &WorktreeSession,
        workspace: &Path,
        outcome: Option<&crate::ExecOutcome>,
    ) -> Option<String> {
        if outcome != Some(&crate::ExecOutcome::Passed)
            || session.snapshot().is_some()
            || common_git_dir(workspace)?.join("worktrees") != self.directory
        {
            return None;
        }
        let after = names(&self.directory)?;
        let mut added = after.difference(&self.names);
        let name = added.next()?;
        if added.next().is_some() {
            return Some("Multiple new worktrees appeared; task worktree routing is unchanged because the destination is ambiguous.".into());
        }
        let admin = self.directory.join(name);
        if !std::fs::symlink_metadata(&admin).ok()?.is_dir() {
            return None;
        }
        let backlink = std::fs::read_to_string(admin.join("gitdir")).ok()?;
        if backlink.trim().is_empty() {
            return None;
        }
        let gitfile = admin.join(backlink.trim()).canonicalize().ok()?;
        let root = WorktreeSession::bound_task_root(gitfile.parent()?, workspace)?;
        // The discovered entry must be the checkout's own admin, not a new
        // directory pointing at some previously existing linked checkout.
        if crate::workspace_key::resolve_gitdir_file(&root.join(".git"), &root)? != admin {
            return None;
        }
        let head = std::fs::read_to_string(admin.join("HEAD")).ok()?;
        let branch = head.trim().strip_prefix("ref: refs/heads/")?;
        crate::git_staging::validate_branch_name(branch).ok()?;
        if session.task_root(workspace).as_ref() == Some(&root) {
            return None; // The literal/adoption recorder already handled it.
        }
        session.record_task_worktree(&root, branch);
        session.task_hint()
    }
}
