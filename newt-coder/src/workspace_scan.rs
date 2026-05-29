//! Scan the workspace for files relevant to the task.
//!
//! Two-pass strategy:
//!
//! 1. **Mentioned files.** Walk the workspace once for source files
//!    (extensions in `SCAN_EXTENSIONS`); if the task prompt mentions
//!    any of their relative paths, file names, or stems, return only
//!    those (precision wins for targeted refactors like
//!    "rename greet to hello in src/lib.rs").
//!
//! 2. **Fallback.** If no file matches, return every source file in
//!    the workspace (rg-style: skip `target/`, `node_modules/`, and
//!    hidden dirs). The prompt builder applies the
//!    `DEFAULT_CONTEXT_CAP_CHARS` budget on top, so even a large
//!    workspace gets bounded context.
//!
//! Paths returned are **relative to the workspace root** so the
//! prompt and the apply step can share the same identifiers.

use std::path::{Path, PathBuf};

use crate::error::{CoderError, Result};

/// File extensions considered "source" for the scan. Heavy enough to
/// cover the languages the bake-off targets (Rust + adjacent ecosystem
/// + docs); light enough to skip vendored binaries.
const SCAN_EXTENSIONS: &[&str] = &[
    "rs", "toml", "py", "js", "ts", "go", "java", "c", "h", "cpp", "hpp", "md",
];

/// Directories to skip during the walk. Matching is exact on the
/// directory component name.
const SKIP_DIRS: &[&str] = &["target", "node_modules"];

/// Two-pass scan: prefer mentioned files; fall back to all source.
pub fn scan_workspace_for_files(workspace: &Path, task: &str) -> Result<Vec<PathBuf>> {
    let all = scan_all_source_files(workspace)?;
    let mentioned = filter_mentioned(&all, task);
    if !mentioned.is_empty() {
        Ok(mentioned)
    } else {
        Ok(all)
    }
}

/// Return the subset of `all` whose path, file name, or file stem
/// appears anywhere in `task`. Empty stems / names are skipped so we
/// don't accidentally match on every file when an extensionless dotfile
/// slips through.
fn filter_mentioned(all: &[PathBuf], task: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    for path in all {
        let rel = path.display().to_string();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if task.contains(&rel)
            || (!fname.is_empty() && task.contains(fname))
            || (!stem.is_empty() && task.contains(stem))
        {
            hits.push(path.clone());
        }
    }
    hits
}

/// Walk the workspace and collect every source file (extension in
/// `SCAN_EXTENSIONS`, not under a skipped dir, not hidden).
fn scan_all_source_files(workspace: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(workspace, workspace, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| CoderError::Workspace(format!("read_dir {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str.as_ref()) {
            continue;
        }

        if path.is_dir() {
            walk(root, &path, out)?;
        } else if path.is_file() {
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                if SCAN_EXTENSIONS.contains(&ext) {
                    let rel = path.strip_prefix(root).map_err(|e| {
                        CoderError::Workspace(format!("strip_prefix {}: {e}", path.display()))
                    })?;
                    out.push(rel.to_path_buf());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, contents: &str) {
        let abs = dir.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(abs, contents).unwrap();
    }

    #[test]
    fn finds_rust_sources_and_skips_target() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn x() {}\n");
        write(tmp.path(), "src/main.rs", "fn main() {}\n");
        // target/ artifacts should NOT appear in the scan.
        write(tmp.path(), "target/debug/junk.rs", "fn junk() {}\n");

        let files = scan_all_source_files(tmp.path()).unwrap();
        let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
        assert!(paths.contains(&"src/lib.rs".to_string()));
        assert!(paths.contains(&"src/main.rs".to_string()));
        assert!(
            !paths.iter().any(|p| p.starts_with("target/")),
            "target/ leaked into the scan: {paths:?}"
        );
    }

    #[test]
    fn skips_hidden_dirs() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn x() {}\n");
        write(tmp.path(), ".git/config", "[core]\n");
        write(tmp.path(), ".hidden/file.rs", "fn x() {}\n");

        let files = scan_all_source_files(tmp.path()).unwrap();
        let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
        assert!(paths.contains(&"src/lib.rs".to_string()));
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with(".git/") || p.starts_with(".hidden/")),
            "hidden dir leaked: {paths:?}"
        );
    }

    #[test]
    fn scan_prefers_mentioned_files() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn greet() {}\n");
        write(tmp.path(), "src/other.rs", "pub fn other() {}\n");
        write(tmp.path(), "Cargo.toml", "[package]\n");

        let hits = scan_workspace_for_files(tmp.path(), "Rename greet in src/lib.rs").unwrap();
        let paths: Vec<String> = hits.iter().map(|p| p.display().to_string()).collect();
        assert!(paths.contains(&"src/lib.rs".to_string()));
        // other.rs is not mentioned, so it must not be included.
        assert!(
            !paths.contains(&"src/other.rs".to_string()),
            "unrelated file leaked into mentioned-only scan: {paths:?}"
        );
    }

    #[test]
    fn scan_falls_back_to_all_when_nothing_mentioned() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn a() {}\n");
        write(tmp.path(), "src/other.rs", "pub fn b() {}\n");

        // A task that doesn't mention any path/name/stem in the workspace.
        let hits = scan_workspace_for_files(tmp.path(), "Add a license header everywhere").unwrap();
        let paths: Vec<String> = hits.iter().map(|p| p.display().to_string()).collect();
        assert!(paths.contains(&"src/lib.rs".to_string()));
        assert!(paths.contains(&"src/other.rs".to_string()));
    }

    #[test]
    fn ignores_unknown_extensions() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/lib.rs", "pub fn x() {}\n");
        write(tmp.path(), "binary.bin", "junk");
        write(tmp.path(), "image.png", "fake png bytes");

        let files = scan_all_source_files(tmp.path()).unwrap();
        let paths: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
        assert!(paths.contains(&"src/lib.rs".to_string()));
        assert!(!paths
            .iter()
            .any(|p| p.ends_with(".bin") || p.ends_with(".png")));
    }

    #[test]
    fn file_stem_match_triggers_mention() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "src/parser.rs", "pub fn parse() {}\n");

        // "parser" is the file stem.
        let hits =
            scan_workspace_for_files(tmp.path(), "Update the parser to handle commas").unwrap();
        let paths: Vec<String> = hits.iter().map(|p| p.display().to_string()).collect();
        assert!(paths.contains(&"src/parser.rs".to_string()));
    }
}
