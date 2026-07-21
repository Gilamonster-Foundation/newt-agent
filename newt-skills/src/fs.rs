//! The filesystem seam discovery is written against.
//!
//! Discovery and body-loading never call `std::fs` directly — they go through
//! [`SkillFs`]. The production implementation is [`OsFs`] (the real disk). This
//! is what lets the discovery logic — hidden-entry skipping, collision
//! precedence, the traversal guard — be exercised as **fully-mocked unit tests**
//! against an in-memory filesystem, with no `tempfile` and no disk, so the
//! cheap tier can gate every change to the security-relevant code path.
//!
//! The seam is also an adoption affordance: because it is read-only and
//! capability-shaped, an embedder can back skills with something other than the
//! local disk (an embedded bundle, a read-through cache, an already-confined
//! view) without the discovery logic knowing.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// A read-only view of a filesystem: the four operations skill discovery needs.
///
/// Kept intentionally tiny. `is_dir` / `is_file` follow symlinks (matching
/// [`std::path::Path::is_dir`]); `read_dir_names` lists the immediate children
/// of a directory and surfaces an error for an absent or unreadable directory,
/// which discovery treats as "no skills here" rather than propagating.
pub trait SkillFs {
    /// List the file names of the immediate entries of `dir`.
    ///
    /// # Errors
    /// Returns the underlying I/O error when `dir` is absent, is not a
    /// directory, or cannot be read (e.g. a permission error or a symlink
    /// loop). Callers treat that as an empty listing.
    fn read_dir_names(&self, dir: &Path) -> io::Result<Vec<OsString>>;

    /// Read the entire contents of `path` as a UTF-8 string.
    ///
    /// # Errors
    /// Returns the underlying I/O error when `path` is absent or unreadable,
    /// or a UTF-8 error when the bytes are not valid UTF-8.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;

    /// Whether `path` resolves to a directory (following symlinks).
    fn is_dir(&self, path: &Path) -> bool;

    /// Whether `path` resolves to a regular file (following symlinks).
    fn is_file(&self, path: &Path) -> bool;
}

/// The production [`SkillFs`]: the real local filesystem via [`std::fs`].
#[derive(Debug, Clone, Copy, Default)]
pub struct OsFs;

impl SkillFs for OsFs {
    fn read_dir_names(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        let mut names = Vec::new();
        // A single unreadable entry should not sink the whole listing; `flatten`
        // skips the erroring entries and keeps the rest.
        for entry in std::fs::read_dir(dir)?.flatten() {
            names.push(entry.file_name());
        }
        Ok(names)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

/// An in-memory [`SkillFs`] for fully-mocked unit tests — no disk, no
/// `tempfile`, deterministic, parallel-safe.
///
/// A path is registered as a directory, a file with contents, or an
/// unreadable directory (whose `read_dir_names` errors, modelling a permission
/// failure or symlink loop). `is_dir`/`is_file` answer from the registry;
/// `read_dir_names` returns the immediate children of a registered directory.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MemFs {
    nodes: std::collections::BTreeMap<std::path::PathBuf, Node>,
}

#[cfg(test)]
enum Node {
    Dir,
    /// A directory whose listing fails (permission denied / symlink loop).
    UnreadableDir,
    File(String),
}

#[cfg(test)]
impl MemFs {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register a directory at `path` (and mark it readable).
    pub(crate) fn dir(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.nodes.insert(path.into(), Node::Dir);
        self
    }

    /// Register a directory whose `read_dir_names` fails.
    pub(crate) fn unreadable_dir(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.nodes.insert(path.into(), Node::UnreadableDir);
        self
    }

    /// Register a file at `path` with the given contents.
    pub(crate) fn file(
        mut self,
        path: impl Into<std::path::PathBuf>,
        contents: impl Into<String>,
    ) -> Self {
        self.nodes.insert(path.into(), Node::File(contents.into()));
        self
    }

    /// Register a complete skill folder `<root>/<folder>/SKILL.md` in one call.
    pub(crate) fn skill(self, root: &Path, folder: &str, skill_md: impl Into<String>) -> Self {
        let dir = root.join(folder);
        let manifest = dir.join("SKILL.md");
        self.dir(root.to_path_buf())
            .dir(dir)
            .file(manifest, skill_md)
    }
}

#[cfg(test)]
impl SkillFs for MemFs {
    fn read_dir_names(&self, dir: &Path) -> io::Result<Vec<OsString>> {
        match self.nodes.get(dir) {
            Some(Node::Dir) => {}
            Some(Node::UnreadableDir) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unreadable",
                ))
            }
            _ => return Err(io::Error::from(io::ErrorKind::NotFound)),
        }
        let mut names = Vec::new();
        for path in self.nodes.keys() {
            if path.parent() == Some(dir) {
                if let Some(name) = path.file_name() {
                    names.push(name.to_os_string());
                }
            }
        }
        Ok(names)
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        match self.nodes.get(path) {
            Some(Node::File(contents)) => Ok(contents.clone()),
            _ => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        matches!(self.nodes.get(path), Some(Node::Dir | Node::UnreadableDir))
    }

    fn is_file(&self, path: &Path) -> bool {
        matches!(self.nodes.get(path), Some(Node::File(_)))
    }
}
