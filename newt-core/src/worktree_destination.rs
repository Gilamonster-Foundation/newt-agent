//! #2757: one approved leaf, held identity, and empty-only failure rollback.
use agent_bridle_fdguard::GrantedRoot;
use rustix::fs::{fstat, mkdirat, openat, unlinkat, AtFlags, Mode, OFlags};
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
        let owner = fstat(object.as_fd()).map_err(|e| e.to_string())?.st_uid;
        // SAFETY: geteuid has no preconditions or memory effects.
        if owner != unsafe { libc::geteuid() } {
            return Err("worktree destination is owned by another user".into());
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
                Mode::from_raw_mode(0o755),
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

    pub(super) fn keep(&mut self) {
        self.created = false;
    }
}

impl Drop for Destination {
    fn drop(&mut self) {
        if !self.created {
            return;
        }
        // Never recursively remove contents, an existing directory, or a
        // replacement. Use the held parent even if its pathname was renamed.
        if let (Some(expected), Ok(current)) = (&self.object, self.open_leaf()) {
            if current.identity() == expected.identity()
                && fstat(current.as_fd()).ok().map(|s| s.st_uid) == self.owner
            {
                let _ = unlinkat(
                    self.parent.as_fd(),
                    Path::new(self.path.file_name().expect("destination leaf")),
                    AtFlags::REMOVEDIR,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2757: swap the parent AFTER its check but BEFORE mkdirat. The mutation
    /// stays in the held parent, the pathname recheck refuses, and Drop undoes it.
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
        assert!(!held.join("task").exists());
        assert!(parent.join("task").is_dir());
        assert_eq!(
            std::fs::read_to_string(parent.join("sentinel")).unwrap(),
            "replacement"
        );
    }

    /// #2757: rollback must preserve a replacement object and concurrent content.
    #[test]
    fn sibling_round2_rollback_preserves_replacement_and_contents() {
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
            assert!(path.is_dir(), "rollback deleted an empty replacement");
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
