//! Complete-file publication through existing bounded directory capabilities.
use super::transaction::{File, Files};
mod publication;
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
    // and constrained both paths to the selected build root.
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
        self.read_name(Path::new(self.path.file_name().unwrap()))
    }

    fn read_name(&self, name: &Path) -> Result<Option<String>, String> {
        let opened = self.open_name(name);
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
}

impl Files for DiskFiles {
    fn read(&self, file: File) -> Result<Option<String>, String> {
        self.paths[file as usize].read()
    }
    fn change(&self, file: File, expected: Option<&str>, next: Option<&str>) -> Result<(), String> {
        self.paths[file as usize].change(expected, next)
    }
}

#[cfg(test)]
mod tests;
