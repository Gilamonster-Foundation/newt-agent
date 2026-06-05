//! File-read tool: load a file from disk with safety checks.
//!
//! Validates the file exists, is valid UTF-8, and doesn't exceed the
//! size cap (5 MiB). Binary files are rejected early via the UTF-8 check.

use std::path::Path;

/// Maximum file size we'll read (5 MiB).
const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024;

/// Read a file to a UTF-8 string, with size and encoding validation.
///
/// # Errors
///
/// - I/O error if the path doesn't exist or isn't readable.
/// - Size error if the file exceeds 5 MiB.
/// - Encoding error if the file is not valid UTF-8.
pub fn read(path: &Path) -> anyhow::Result<String> {
    let metadata = std::fs::metadata(path)?;

    if metadata.len() > MAX_FILE_SIZE {
        anyhow::bail!(
            "file too large: {} bytes (max {MAX_FILE_SIZE})",
            metadata.len()
        );
    }

    let bytes = std::fs::read(path)?;
    String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("file is not valid UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn happy_path() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "hello world").unwrap();
        let content = read(f.path()).unwrap();
        assert!(content.contains("hello world"));
    }

    #[test]
    fn missing_file() {
        let err = read(Path::new("/tmp/newt-tools-does-not-exist-xyz")).unwrap_err();
        let io = err
            .downcast_ref::<std::io::Error>()
            .expect("missing file should return an io error");
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn non_utf8() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&[0xFF, 0xFE]).unwrap();
        let err = read(f.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("UTF-8"), "expected UTF-8 error, got: {msg}");
    }

    #[test]
    fn empty_file() {
        let f = NamedTempFile::new().unwrap();
        let content = read(f.path()).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn small_file_within_limit() {
        let mut f = NamedTempFile::new().unwrap();
        let data = "a".repeat(1024); // 1 KiB — well under 5 MiB
        write!(f, "{data}").unwrap();
        let content = read(f.path()).unwrap();
        assert_eq!(content.len(), 1024);
    }
}
