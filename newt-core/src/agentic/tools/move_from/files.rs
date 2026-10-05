//! Complete-file publication through existing bounded directory capabilities.
use super::transaction::{File, Files};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Write;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub(super) struct DiskFiles {
    paths: [Entry; 2],
}
struct Entry {
    path: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    parent: crate::fs_cap::WorkspaceDir,
}

impl DiskFiles {
    // The caller has authorized both parent directories (staging needs a sibling)
    // and constrained both paths to the canonical session workspace.
    pub fn open(root: &Path, source: &Path, child: &Path) -> Result<Self, String> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let workspace = crate::fs_cap::WorkspaceDir::open_root(root).map_err(error)?;
        let open = |path: &Path| -> Result<Entry, String> {
            let parent = path.parent().ok_or("missing parent directory")?;
            let rel = parent.strip_prefix(root).map_err(error)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            let parent = {
                workspace.create_dir_all(rel).map_err(error)?;
                workspace
                    .open_dir(if rel.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        rel
                    })
                    .map_err(error)?
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = rel;
                std::fs::create_dir_all(parent).map_err(error)?;
                if parent.canonicalize().map_err(error)? != parent {
                    return Err("symlinked parent refused".into());
                }
            }
            Ok(Entry {
                path: path.to_owned(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                parent,
            })
        };
        Ok(Self {
            paths: [open(source)?, open(child)?],
        })
    }
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl Entry {
    fn read(&self) -> Result<Option<String>, String> {
        self.check_parent()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let opened = self
            .parent
            .open_regular(Path::new(self.path.file_name().unwrap()), true);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let opened = {
            self.check_parent()?;
            match std::fs::symlink_metadata(&self.path) {
                Ok(m) if !m.file_type().is_file() => return Err("nonregular target refused".into()),
                _ => {}
            }
            std::fs::File::open(&self.path)
        };
        match opened {
            Ok(mut file) => {
                let mut s = String::new();
                file.read_to_string(&mut s).map_err(error)?;
                Ok(Some(s))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(error(e)),
        }
    }

    fn check_parent(&self) -> Result<(), String> {
        let parent = self.path.parent().unwrap();
        if parent.canonicalize().map_err(error)? != parent {
            return Err("parent changed during extraction".into());
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if !self.parent.still_named_by(parent).map_err(error)? {
            return Err("held directory no longer names the checked tree".into());
        }
        Ok(())
    }

    fn change(&self, expected: Option<&str>, next: Option<&str>) -> Result<(), String> {
        if self.read()?.as_deref() != expected {
            return Err("stale file; refusing replacement/rollback".into());
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let name = Path::new(self.path.file_name().unwrap());
            let Some(next) = next else {
                return self.parent.unlink(name).map_err(error);
            };
            let temp = PathBuf::from(format!(
                ".newt-move-{}.tmp",
                crate::atomic_fs::unique_suffix()
            ));
            let mut staged = self.parent.create_new(&temp).map_err(error)?;
            let result = (|| {
                if expected.is_some() {
                    staged.set_permissions(
                        self.parent
                            .open_regular(name, true)?
                            .metadata()?
                            .permissions(),
                    )?;
                }
                staged.write_all(next.as_bytes())?;
                staged.sync_all()?;
                if self.read().map_err(io::Error::other)?.as_deref() != expected {
                    return Err(io::Error::other("stale file before publication"));
                }
                if expected.is_none() {
                    self.parent.link_new(&temp, name)
                } else {
                    self.parent.rename(&temp, name)
                }
            })();
            let _ = self.parent.unlink(&temp);
            result.map_err(error)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            self.check_parent()?;
            let Some(next) = next else {
                return std::fs::remove_file(&self.path).map_err(error);
            };
            let target = crate::atomic_fs::ResolvedPath::resolve(&self.path).map_err(error)?;
            let permissions = std::fs::metadata(&self.path).ok().map(|m| m.permissions());
            let staged = target
                .stage_with_permissions(next.as_bytes(), permissions.as_ref(), false)
                .map_err(error)?;
            let result = (|| {
                self.check_parent()?;
                if self.read()?.as_deref() != expected {
                    return Err("stale file before publication".into());
                }
                if expected.is_none() {
                    target.durable_create(&staged).map_err(error)
                } else {
                    target.durable_replace(&staged).map_err(error)
                }
            })();
            let _ = std::fs::remove_file(staged);
            result
        }
    }
}

impl Files for DiskFiles {
    fn read(&self, file: File) -> Result<Option<String>, String> {
        self.paths[file as usize].read()
    }
    fn change(&self, file: File, expected: Option<&str>, next: Option<&str>) -> Result<(), String> {
        self.paths[file as usize].change(expected, next)
    }
}
