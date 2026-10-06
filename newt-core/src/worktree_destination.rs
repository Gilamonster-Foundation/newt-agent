//! #2757: one private approved leaf, held identity, and no rollback deletion.
//!
//! A concurrent writer to the parent running as the SAME uid is outside the
//! threat model: it already holds the user's authority. An indistinguishable
//! empty, same-owner, mode-0700 replacement gains that writer nothing. Binding
//! refuses other owners, nonempty directories and modes other than 0700.
//! Failure retains the directory; pathname checks cannot make unlink conditional
//! on a held object, so this module never deletes a directory.
use agent_bridle_fdguard::GrantedRoot;
use rustix::fs::{fstat, mkdirat, openat, Mode, OFlags};
use std::path::{Path, PathBuf};

pub(super) struct Destination {
    parent: GrantedRoot,
    path: PathBuf,
    object: Option<GrantedRoot>,
    owner: Option<u32>,
    created: bool,
}

impl Destination {
    pub(super) fn capture(path: &Path) -> Result<Self, String> {
        // Retain the nearest parent for admission, but prepare refuses unless
        // it is the immediate parent. No missing ancestor is ever created.
        let parent = GrantedRoot::acquire(
            path.parent()
                .and_then(|p| p.ancestors().find(|p| p.is_dir()))
                .ok_or("worktree destination has no existing parent")?,
        )
        .map_err(|e| e.to_string())?;
        let mut out = Self {
            parent,
            path: path.into(),
            object: None,
            owner: None,
            created: false,
        };
        if out.path.parent() == Some(out.parent.provenance()) {
            match out.open_leaf() {
                Ok(object) => out.bind(object)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(out)
    }

    fn open_leaf(&self) -> std::io::Result<GrantedRoot> {
        let fd = openat(
            self.parent.as_fd(),
            Path::new(self.path.file_name().expect("destination leaf")),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        GrantedRoot::from_owned_fd(fd, &self.path)
    }

    fn bind(&mut self, object: GrantedRoot) -> Result<(), String> {
        let stat = fstat(object.as_fd()).map_err(|e| e.to_string())?;
        let owner = stat.st_uid;
        // SAFETY: geteuid has no preconditions or memory effects.
        if owner != unsafe { libc::geteuid() } {
            return Err("worktree destination is owned by another user".into());
        }
        if stat.st_mode & 0o7777 != 0o700 {
            return Err("worktree destination must have mode 0700".into());
        }
        if !Self::empty(&object)? {
            return Err("worktree destination must be empty".into());
        }
        self.owner = Some(owner);
        self.object = Some(object);
        Ok(())
    }

    fn check_parent(&self) -> Result<(), String> {
        if self.path.parent() != Some(self.parent.provenance()) {
            return Err("worktree destination needs an existing immediate parent; create the missing intermediate directories separately".into());
        }
        let current = GrantedRoot::acquire(self.parent.provenance()).map_err(|e| e.to_string())?;
        if current.identity() != self.parent.identity() {
            return Err("worktree destination parent changed during approval or execution".into());
        }
        Ok(())
    }

    pub(super) fn prepare(&mut self) -> Result<(), String> {
        self.check_parent()?;
        self.materialize()?;
        self.check()
    }

    fn materialize(&mut self) -> Result<(), String> {
        if self.object.is_none() {
            // EEXIST is a refusal: an absent-at-admission leaf may not be
            // replaced by an unapproved object, even an empty directory.
            mkdirat(
                self.parent.as_fd(),
                Path::new(self.path.file_name().expect("destination leaf")),
                Mode::from_raw_mode(0o700),
            )
            .map_err(|e| format!("cannot create approved worktree destination: {e}"))?;
            let object = self.open_leaf().map_err(|e| e.to_string())?;
            self.bind(object)?;
            self.created = true;
        }
        Ok(())
    }

    /// Reacquisition is only an identity check against the retained object,
    /// never authority to replace it. Check both pathname and held-parent view.
    pub(super) fn check(&self) -> Result<(), String> {
        self.check_parent()?;
        let expected = self
            .object
            .as_ref()
            .ok_or("worktree destination was not prepared")?;
        for current in [
            self.open_leaf().map_err(|e| e.to_string())?,
            GrantedRoot::acquire(&self.path).map_err(|e| e.to_string())?,
        ] {
            if current.identity() != expected.identity()
                || Some(fstat(current.as_fd()).map_err(|e| e.to_string())?.st_uid) != self.owner
            {
                return Err(
                    "worktree destination identity or owner changed during approval or execution"
                        .into(),
                );
            }
        }
        Ok(())
    }

    fn empty(object: &GrantedRoot) -> Result<bool, String> {
        let directory =
            crate::fs_cap::WorkspaceDir::from_granted_root(object).map_err(|e| e.to_string())?;
        Ok(directory
            .read_dir(Path::new("."))
            .map_err(|e| e.to_string())?
            .is_empty())
    }

    pub(super) fn left_empty(&self) -> bool {
        self.created
            && self.check().is_ok()
            && self
                .object
                .as_ref()
                .is_some_and(|object| Self::empty(object) == Ok(true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2757: swap the parent AFTER its check but BEFORE mkdirat. The mutation
    /// stays in the held parent, the pathname recheck refuses, and Drop preserves it.
    #[test]
    fn sibling_round2_parent_swap_between_check_and_mkdir() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let held = temp.path().join("held");
        std::fs::create_dir(&parent).unwrap();
        let mut guard = Destination::capture(&parent.join("task")).unwrap();
        guard.check_parent().unwrap();
        std::fs::rename(&parent, &held).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("sentinel"), "replacement").unwrap();
        std::fs::create_dir(parent.join("task")).unwrap();
        guard.materialize().unwrap();
        assert!(guard.check().is_err());
        assert!(held.join("task").exists());
        drop(guard);
        assert!(held.join("task").is_dir());
        assert!(parent.join("task").is_dir());
        assert_eq!(
            std::fs::read_to_string(parent.join("sentinel")).unwrap(),
            "replacement"
        );
    }

    /// #2757: failure must preserve a replacement object and concurrent content.
    #[test]
    fn sibling_round3_drop_preserves_replacement_and_contents() {
        for replace in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("task");
            let mut guard = Destination::capture(&path).unwrap();
            guard.prepare().unwrap();
            if replace {
                std::fs::rename(&path, temp.path().join("held")).unwrap();
                std::fs::create_dir(&path).unwrap();
                assert!(guard.check().is_err());
            }
            if !replace {
                std::fs::write(path.join("sentinel"), "keep").unwrap();
            }
            drop(guard);
            assert!(path.is_dir(), "drop deleted an empty replacement");
            if !replace {
                assert_eq!(
                    std::fs::read_to_string(path.join("sentinel")).unwrap(),
                    "keep"
                );
            }
        }
    }

    /// #2757: owner is part of the retained identity, not just dev/ino.
    #[test]
    fn sibling_round2_owner_change_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let mut guard = Destination::capture(&temp.path().join("task")).unwrap();
        guard.prepare().unwrap();
        // Inject the previously-observed owner; changing filesystem ownership
        // would require privileges and would not be a hermetic unit test.
        guard.owner = guard.owner.map(|uid| uid.wrapping_add(1));
        assert!(guard.check().is_err());
    }
}

#[cfg(test)]
mod round3_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// #2757: creating a leaf must not grant access to other users.
    #[test]
    fn sibling_round3_leaf_is_private_and_drop_preserves_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("task");
        let mut guard = Destination::capture(&path).unwrap();
        guard.prepare().unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        drop(guard);
        assert!(path.is_dir(), "failure/cancellation must not unlink a leaf");
    }

    /// #2757: deterministic mkdir-to-open swap: reject a same-owner replacement
    /// that is nonempty or not mode 0700, preserving both objects on refusal.
    #[test]
    fn sibling_round3_bind_refuses_replacement_contents_or_mode() {
        for nonempty in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("task");
            let held = temp.path().join("held");
            let mut guard = Destination::capture(&path).unwrap();
            mkdirat(guard.parent.as_fd(), "task", Mode::from_raw_mode(0o700)).unwrap();
            std::fs::rename(&path, &held).unwrap();
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(if nonempty { 0o700 } else { 0o755 }),
            )
            .unwrap();
            if nonempty {
                std::fs::write(path.join("sentinel"), "keep").unwrap();
            }
            let object = guard.open_leaf().unwrap();
            assert!(guard.bind(object).is_err(), "replacement was bound");
            drop(guard);
            assert!(held.is_dir() && path.is_dir());
            if nonempty {
                assert_eq!(
                    std::fs::read_to_string(path.join("sentinel")).unwrap(),
                    "keep"
                );
            }
        }
    }
}
