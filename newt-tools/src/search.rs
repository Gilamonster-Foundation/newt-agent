//! Regex-based code search across a directory tree.
//!
//! Walks the tree rooted at `root`, skipping hidden directories and common
//! ignore patterns (`target`, `node_modules`). Returns up to [`MAX_HITS`]
//! matching lines with path and line number.

use std::path::Path;

use regex::Regex;

/// Hard cap on results to avoid unbounded memory.
const MAX_HITS: usize = 1000;

/// A single search hit: file path, 1-based line number, and line content.
#[derive(Debug, Clone)]
pub struct Hit {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

/// Search for `query` (a regex pattern) in all text files under `root`.
///
/// # Errors
///
/// - Invalid regex pattern.
/// - I/O error reading the root directory.
pub fn search(query: &str, root: &Path) -> anyhow::Result<Vec<Hit>> {
    let re = Regex::new(query).map_err(|e| anyhow::anyhow!("invalid regex: {e}"))?;
    let mut hits = Vec::new();
    search_dir(root, &re, &mut hits)?;
    Ok(hits)
}

fn search_dir(dir: &Path, re: &Regex, hits: &mut Vec<Hit>) -> anyhow::Result<()> {
    if hits.len() >= MAX_HITS {
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if hits.len() >= MAX_HITS {
            break;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden dirs and common ignore patterns.
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }

        if path.is_dir() {
            search_dir(&path, re, hits)?;
        } else if path.is_file() {
            search_file(&path, re, hits)?;
        }
    }
    Ok(())
}

fn search_file(path: &Path, re: &Regex, hits: &mut Vec<Hit>) -> anyhow::Result<()> {
    // Skip binary/unreadable files silently.
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    for (i, line) in content.lines().enumerate() {
        if hits.len() >= MAX_HITS {
            break;
        }
        if re.is_match(line) {
            hits.push(Hit {
                path: path.to_string_lossy().to_string(),
                line_number: i + 1,
                line: line.to_string(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) {
        let file_path = dir.join(name);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&file_path, content).unwrap();
    }

    #[test]
    fn single_hit() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "hello.txt", "hello world\ngoodbye moon\n");
        let hits = search("hello", tmp.path()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line_number, 1);
        assert!(hits[0].line.contains("hello world"));
    }

    #[test]
    fn multiple_hits() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "foo bar\nbaz foo\n");
        write_file(tmp.path(), "b.txt", "foo again\n");
        let hits = search("foo", tmp.path()).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn no_hits() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "hello world\n");
        let hits = search("zzz_not_present", tmp.path()).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn hidden_dirs_skipped() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), ".hidden/secret.txt", "secret match\n");
        write_file(tmp.path(), "visible.txt", "no match here\n");
        let hits = search("secret", tmp.path()).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn binary_files_skipped() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("binary.bin");
        fs::write(&bin_path, [0xFF, 0xFE, 0x00, 0x01]).unwrap();
        // Should not crash.
        let hits = search(".", tmp.path()).unwrap();
        // Binary file won't match because it's not valid UTF-8.
        assert!(hits.is_empty());
    }

    #[test]
    fn invalid_regex_returns_error() {
        let tmp = TempDir::new().unwrap();
        let err = search("[invalid", tmp.path()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid regex"), "unexpected error: {msg}");
    }
}
