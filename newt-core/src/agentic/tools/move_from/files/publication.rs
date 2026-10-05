//! Capture entries before comparison; publication/recovery never replaces a
//! newly occupied name. Retain captured inodes, including on success: an editor
//! may still hold a writable descriptor to one after our comparison.
use super::{error, Entry};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

impl Entry {
    pub(super) fn change(&self, expected: Option<&str>, next: Option<&str>) -> Result<(), String> {
        self.change_with(expected, next, || {})
    }

    pub(super) fn change_with(
        &self,
        expected: Option<&str>,
        next: Option<&str>,
        before_act: impl FnOnce(),
    ) -> Result<(), String> {
        self.change_with_hooks(expected, next, before_act, || {})
    }

    pub(super) fn change_with_hooks(
        &self,
        expected: Option<&str>,
        next: Option<&str>,
        before_act: impl FnOnce(),
        after_capture: impl FnOnce(),
    ) -> Result<(), String> {
        if self.read()?.as_deref() != expected {
            return Err("CONFLICT: stale file; refusing replacement/rollback".into());
        }
        let name = Path::new(self.path.file_name().unwrap());
        let staged = next
            .map(|text| self.stage(text, expected.is_some()))
            .transpose()?;
        let result = (|| {
            if self.read()?.as_deref() != expected {
                return Err("CONFLICT: stale file before publication".into());
            }
            before_act();
            if expected.is_none() {
                return match &staged {
                    Some(staged) => self
                        .link_new(staged, name)
                        .map_err(|e| format!("CONFLICT: create refused: {e}")),
                    None => Ok(()),
                };
            }
            let (saved, placeholder) = self.temporary("saved").map_err(error)?;
            drop(placeholder);
            // Rename captures the actual entry atomically under the held dir fd.
            // It replaces only our reserved placeholder, never a user target.
            if let Err(e) = self.rename(name, &saved) {
                let _ = self.unlink(&saved);
                return Err(format!("CONFLICT: capture failed: {e}"));
            }
            let publish: Result<(), String> = (|| {
                if self.read_name(&saved)?.as_deref() != expected {
                    return Err("captured bytes differ from expected".into());
                }
                after_capture();
                if let Some(staged) = &staged {
                    self.link_new(staged, name).map_err(error)?;
                } else if self.read()?.is_some() {
                    return Err("target reappeared during removal".into());
                }
                Ok(())
            })();
            if let Err(e) = publish {
                // EEXIST means another writer owns the name. Keep both versions.
                let recovery = match self.link_new(&saved, name) {
                    Ok(()) => "captured entry returned to its original name".to_owned(),
                    Err(e) => format!("original name not replaced: {e}"),
                };
                return Err(format!(
                    "CONFLICT: {e}; {recovery}; displaced entry retained at {}",
                    self.path.with_file_name(&saved).display()
                ));
            }
            Ok(())
        })();
        if let Some(staged) = staged {
            let _ = self.unlink(&staged);
        }
        result
    }

    pub(super) fn open_name(&self, name: &Path) -> io::Result<std::fs::File> {
        self.check_parent().map_err(io::Error::other)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.parent.open_regular(name, true)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let path = self.path.with_file_name(name);
            if !std::fs::symlink_metadata(&path)?.file_type().is_file() {
                return Err(io::Error::other("nonregular target refused"));
            }
            std::fs::File::open(path)
        }
    }

    fn temporary(&self, extension: &str) -> io::Result<(PathBuf, std::fs::File)> {
        self.check_parent().map_err(io::Error::other)?;
        let name = PathBuf::from(format!(
            ".newt-move-{}.{}",
            crate::atomic_fs::unique_suffix(),
            extension
        ));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let file = self.parent.create_new(&name)?;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path.with_file_name(&name))?;
        Ok((name, file))
    }

    fn stage(&self, text: &str, preserve_permissions: bool) -> Result<PathBuf, String> {
        let (name, mut file) = self.temporary("tmp").map_err(error)?;
        let result = (|| {
            if preserve_permissions {
                file.set_permissions(
                    self.open_name(Path::new(self.path.file_name().unwrap()))?
                        .metadata()?
                        .permissions(),
                )?;
            }
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })();
        drop(file);
        if let Err(e) = result {
            let _ = self.unlink(&name);
            return Err(error(e));
        }
        Ok(name)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check_parent().map_err(io::Error::other)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.parent.rename(from, to)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            std::fs::rename(self.path.with_file_name(from), self.path.with_file_name(to))
        }
    }

    fn link_new(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.check_parent().map_err(io::Error::other)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.parent.link_new(from, to)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            std::fs::hard_link(self.path.with_file_name(from), self.path.with_file_name(to))
        }
    }

    fn unlink(&self, name: &Path) -> io::Result<()> {
        self.check_parent().map_err(io::Error::other)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.parent.unlink(name)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            std::fs::remove_file(self.path.with_file_name(name))
        }
    }
}
