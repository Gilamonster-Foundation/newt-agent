//! Directory listing for the agent tool surface.
//!
//! Lists the immediate children of a path, sorted directories-first.

use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

/// A single directory entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirEntry {
    /// Filename (not the full path).
    pub name: String,
    /// Entry kind.
    pub kind: EntryKind,
    /// File size in bytes (only set for regular files).
    pub size_bytes: Option<u64>,
}

/// Coarse entry type.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link.
    Symlink,
    /// Anything else (device, socket, …).
    Other,
}

/// List the immediate children of `path`, sorted directories-first then
/// alphabetically.
///
/// # Errors
///
/// Returns an error if `path` is not a directory or cannot be read.
pub fn list_dir(path: &Path) -> anyhow::Result<Vec<DirEntry>> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?
        .map(|res| {
            let entry = res?;
            let file_type = entry.file_type()?;
            let kind = if file_type.is_dir() {
                EntryKind::Dir
            } else if file_type.is_symlink() {
                EntryKind::Symlink
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            let size_bytes = if matches!(kind, EntryKind::File) {
                entry.metadata().ok().map(|m| m.len())
            } else {
                None
            };
            Ok::<_, std::io::Error>(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
                size_bytes,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    entries.sort_by(|a, b| {
        let a_dir = matches!(a.kind, EntryKind::Dir);
        let b_dir = matches!(b.kind, EntryKind::Dir);
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        }
    });

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn list_dir_happy_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("b.txt"), "hi").unwrap();
        std::fs::create_dir(tmp.path().join("a_dir")).unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello").unwrap();

        let entries = list_dir(tmp.path()).unwrap();
        // Dirs first
        assert!(matches!(entries[0].kind, EntryKind::Dir));
        assert_eq!(entries[0].name, "a_dir");
        // Then files, alphabetically
        assert_eq!(entries[1].name, "a.txt");
        assert_eq!(entries[2].name, "b.txt");
    }

    #[test]
    fn list_dir_not_found() {
        let err = list_dir(Path::new("/no-such-dir-xyz")).unwrap_err().to_string();
        assert!(err.contains("cannot read"), "got: {err}");
    }

    #[test]
    fn list_dir_sizes_set_for_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "hello").unwrap();
        let entries = list_dir(tmp.path()).unwrap();
        let file = entries.iter().find(|e| e.name == "f.txt").unwrap();
        assert_eq!(file.size_bytes, Some(5));
    }
}
