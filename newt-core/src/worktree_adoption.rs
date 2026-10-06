//! Session worktree adoption and original-checkout protection (#2733, #2757).
//! Explicit destination approval can relocate workspace access for the task;
//! paths remain runtime locators, never persisted identities.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "worktree_destination.rs"]
mod destination;

use crate::{Caveats, Scope};
use std::path::{Path, PathBuf};

/// Owned by a session, shared across turns; a fresh task starts empty.
#[derive(Debug, Default)]
pub struct WorktreeSession(std::sync::Mutex<Option<AdoptedWorktree>>);
impl WorktreeSession {
    /// Explicit operator lift, also used at the existing new-task boundary.
    pub fn lift(&self) {
        *self.0.lock().expect("worktree session lock") = None;
    }
    pub(crate) fn snapshot(&self) -> Option<AdoptedWorktree> {
        self.0.lock().expect("worktree session lock").clone()
    }
    pub(crate) fn adopt(&self, candidate: AdoptedWorktree) {
        let mut state = self.0.lock().expect("worktree session lock");
        if state.is_none() {
            *state = Some(candidate);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AdoptedWorktree {
    original: PathBuf,
    pub(crate) worktree: PathBuf,
    common: PathBuf,
    admin: PathBuf,
    protected_branch: Option<String>,
    relocate_workspace_access: bool,
}

fn resolved(path: &Path) -> Option<PathBuf> {
    crate::config::resolve_uncreated_path(path).ok()
}
fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl AdoptedWorktree {
    /// A verified, approved workspace switch carries the original workspace's
    /// access to this task only. Do not replay the one-shot grant or restore an
    /// axis removed by a read-only/deny-all caller. Normal gate ceilings still
    /// apply after this projection; attenuation itself remains narrowing-only.
    pub(crate) fn task_authority(&self, base: &Caveats) -> Caveats {
        let mut out = base.clone();
        if self.relocate_workspace_access {
            for scope in [&mut out.fs_read, &mut out.fs_write] {
                if crate::caveats::permits_path(scope, &self.original.to_string_lossy()) {
                    if let Scope::Only(roots) = scope {
                        roots.insert(self.worktree.to_string_lossy().into_owned());
                    }
                }
            }
        }
        out
    }

    pub(crate) fn valid(&self, caveats: &Caveats) -> bool {
        self.worktree.canonicalize().ok().as_ref() == Some(&self.worktree)
            && crate::git_staging::HeldRoots::bind(&caveats.fs_read)
                .ok()
                .and_then(|held| crate::git_staging::discover_git_dirs(&self.worktree, &held).ok())
                .is_some_and(|(common, admin)| common == self.common && admin == self.admin)
    }
    pub(crate) fn protects_branch(&self, branch: &str) -> bool {
        self.protected_branch.as_deref() == Some(branch)
    }

    /// Logical authority for a bounded new-ref operation, never a directory
    /// grant to arbitrary child code. Commits use the existing detached broker.
    pub(crate) fn create_branch(
        &self,
        cwd: &Path,
        branch: &str,
        authority: &Caveats,
    ) -> Result<(), String> {
        let held =
            crate::git_staging::HeldRoots::bind(&authority.fs_read).map_err(|e| e.to_string())?;
        let dirs = crate::git_staging::discover_git_dirs(cwd, &held).map_err(|e| e.to_string())?;
        if !self.valid(authority)
            || dirs != (self.common.clone(), self.admin.clone())
            || !cwd
                .canonicalize()
                .is_ok_and(|p| p.starts_with(&self.worktree))
            || !crate::caveats::permits_path(&authority.fs_write, &self.worktree.to_string_lossy())
            || self.protects_branch(branch)
        {
            return Err(self.notice());
        }
        crate::git_hardening::create_worktree_branch(&self.worktree, branch)
    }

    fn exception(&self, path: &Path) -> bool {
        path.starts_with(&self.worktree)
            || path.starts_with(&self.admin)
            || path.starts_with(self.common.join("objects"))
    }
    pub(crate) fn blocked(&self, path: &Path) -> bool {
        resolved(path).is_none_or(|p| {
            (overlap(&p, &self.original) || overlap(&p, &self.common)) && !self.exception(&p)
        })
    }
    pub(crate) fn notice(&self) -> String {
        format!("capability denied: the original checkout is read-only after worktree adoption; write in {} instead. Shared Git config stays read-only; use git -c user.name=… -c user.email=… for per-command identity. Only the operator can lift this with /permissions worktree-lift.", self.worktree.display())
    }
    pub(crate) fn attenuate(&self, authority: &Caveats) -> Caveats {
        let mut out = authority.clone();
        let mut roots = std::collections::BTreeSet::new();
        if let Scope::Only(grants) = &authority.fs_write {
            for root in grants {
                if let Some(path) = resolved(Path::new(root)) {
                    if (!overlap(&path, &self.original) && !overlap(&path, &self.common))
                        || self.exception(&path)
                    {
                        roots.insert(path.to_string_lossy().into_owned());
                    }
                }
            }
        }
        // Split broad authority at only the task checkout and its required Git
        // admin/object roots. Each retained root must already be writable;
        // adoption never adds authority to deny-all or unrelated-only callers.
        for root in [&self.worktree, &self.admin, &self.common.join("objects")] {
            if crate::caveats::permits_path(&authority.fs_write, &root.to_string_lossy()) {
                roots.insert(root.to_string_lossy().into_owned());
            }
        }
        out.fs_write = Scope::only(roots);
        out
    }
}

/// Candidate captured BEFORE dispatch. Git output cannot create this evidence.
pub(crate) struct Creation {
    original: PathBuf,
    destination: PathBuf,
    common: PathBuf,
    read: Scope<String>,
    protected_branch: Option<String>,
    relocate_workspace_access: bool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    path_guard: destination::Destination,
}
impl Creation {
    /// Canonical paths also catch destinations reached through a symlink or -C.
    pub(crate) fn nested_in_original(&self) -> bool {
        self.destination.starts_with(&self.original)
    }

    pub(crate) fn destination(&self) -> &Path {
        &self.destination
    }

    /// Only after approval: create the destination through its held parent so
    /// the child fence can bind the destination itself, never its whole parent.
    pub(crate) fn prepare(&mut self, authority: &Caveats) -> Result<(), String> {
        if ![&authority.fs_read, &authority.fs_write]
            .iter()
            .all(|scope| crate::caveats::permits_path(scope, &self.destination.to_string_lossy()))
        {
            return Err("worktree destination needs read and write approval".into());
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.path_guard.prepare()?;
            self.read = authority.fs_read.clone();
            self.relocate_workspace_access = true;
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err("safe worktree destination preparation is unavailable on this platform".into())
    }

    pub(crate) fn before(original: &Path, destination: &Path, caveats: &Caveats) -> Option<Self> {
        let original = original.canonicalize().ok()?;
        let original = original
            .ancestors()
            .find(|dir| dir.join(".git").exists())?
            .to_path_buf();
        let destination = resolved(destination)?;
        // Existing worktrees, including those created by other sessions, never
        // count as this invocation creating one.
        if destination.join(".git").exists() {
            return None;
        }
        let held = crate::git_staging::HeldRoots::bind(&caveats.fs_read).ok()?;
        let (common, admin) = crate::git_staging::discover_git_dirs(&original, &held).ok()?;
        let head = held.read_to_string(&admin, Path::new("HEAD")).ok()??;
        let protected_branch = if let Some(branch) = head.trim().strip_prefix("ref: refs/heads/") {
            crate::git_staging::validate_branch_name(branch).ok()?;
            Some(branch.to_owned())
        } else if crate::git_staging::is_hex_oid(head.trim()) {
            None
        } else {
            return None;
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let path_guard = destination::Destination::capture(&destination).ok()?;
        Some(Self {
            original,
            destination,
            common,
            read: caveats.fs_read.clone(),
            protected_branch,
            relocate_workspace_access: false,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            path_guard,
        })
    }
    pub(crate) fn matches_source(&self, cwd: &Path) -> bool {
        crate::git_staging::HeldRoots::bind(&self.read)
            .ok()
            .and_then(|held| crate::git_staging::discover_git_dirs(cwd, &held).ok())
            .is_some_and(|(common, _)| common == self.common)
    }
    pub(crate) fn ready(&self) -> Result<(), String> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.path_guard.check()?;
        Ok(())
    }

    pub(crate) fn leftover_notice(&self) -> Option<String> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if self.path_guard.left_empty() {
            return Some(format!(
                "left empty directory {}; remove it if unwanted",
                self.destination.display()
            ));
        }
        None
    }

    pub(crate) fn verify(self) -> Option<AdoptedWorktree> {
        self.ready().ok()?;
        let held = crate::git_staging::HeldRoots::bind(&self.read).ok()?;
        let (common, admin) =
            crate::git_staging::discover_git_dirs(&self.destination, &held).ok()?;
        if common != self.common || admin == common || !admin.starts_with(common.join("worktrees"))
        {
            return None;
        }
        // Both directions must agree: the destination is a linked checkout,
        // and its registered admin entry points back at that very gitlink.
        let gitlink = self.destination.join(".git");
        if !gitlink.is_file() {
            return None;
        }
        let back = held.read_to_string(&admin, Path::new("gitdir")).ok()??;
        if resolved(Path::new(back.trim()))? != gitlink.canonicalize().ok()? {
            return None;
        }
        self.ready().ok()?;
        Some(AdoptedWorktree {
            original: self.original,
            worktree: self.destination,
            common,
            admin,
            protected_branch: self.protected_branch,
            relocate_workspace_access: self.relocate_workspace_access,
        })
    }
}

#[cfg(test)]
#[path = "worktree_adoption_tests.rs"]
pub(crate) mod tests;
