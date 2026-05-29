//! Unified-diff parser and applier.
//!
//! Parses `--- a/…` / `+++ b/…` headers and `@@ -X,Y +X,Y @@` hunk headers,
//! then applies context/add/remove lines to produce the patched file.
//! Writes atomically via temp-file-then-rename.

use std::path::Path;

// ── Data model ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct FilePatch {
    path: String,
    hunks: Vec<Hunk>,
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    #[allow(dead_code)]
    old_count: usize,
    #[allow(dead_code)]
    new_start: usize,
    #[allow(dead_code)]
    new_count: usize,
    lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
enum DiffLine {
    Context(String),
    Add(String),
    Remove(String),
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Apply a unified diff to files under `root`.
///
/// Each file referenced in the diff is resolved relative to `root`.
/// New files are created if they don't exist. Writes are atomic
/// (write to `.newt-tmp`, then rename).
///
/// # Errors
///
/// - Malformed diff (missing headers, bad hunk header syntax).
/// - Context mismatch (the file doesn't match what the diff expects).
/// - I/O errors on read/write.
pub fn apply_patch(root: &Path, diff: &str) -> anyhow::Result<()> {
    let patches = parse_unified_diff(diff)?;

    if patches.is_empty() {
        anyhow::bail!("no file patches found in diff");
    }

    // Validate all patches first so we don't partially apply.
    let mut results: Vec<(std::path::PathBuf, String)> = Vec::new();

    for patch in &patches {
        let file_path = root.join(&patch.path);
        let original = if file_path.exists() {
            std::fs::read_to_string(&file_path)?
        } else {
            String::new()
        };

        let result = apply_hunks(&original, &patch.hunks)?;
        results.push((file_path, result));
    }

    // All patches validated — now write atomically.
    for (file_path, content) in &results {
        let tmp_path = file_path.with_extension("newt-tmp");
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&tmp_path, content)?;
        std::fs::rename(&tmp_path, file_path)?;
    }

    Ok(())
}

/// Convenience wrapper: apply a patch scoped to a single file's parent.
pub fn edit(path: &Path, patch: &str) -> anyhow::Result<()> {
    let root = path.parent().unwrap_or(Path::new("."));
    apply_patch(root, patch)
}

/// Write a set of whole files into `workspace` atomically.
///
/// For each `(relative_path, contents)` entry: create parent
/// directories as needed, write to `<file>.newt-coder-tmp`, then
/// rename into place. Returns the list of relative paths actually
/// written so the caller can log them and (after writing) capture
/// the diff via `git diff`.
///
/// This is the multi-file landing pad for the newt-coder plugin's
/// `Emission::WholeFiles` shape. Kept here (rather than inline in
/// newt-coder) so any other caller that produces a set of whole
/// files (test harnesses, future strategies) can share the atomic
/// write semantics.
///
/// # Errors
///
/// - `create_dir_all` failure on a parent directory.
/// - `write` failure on the temp file.
/// - `rename` failure when moving temp -> final.
pub fn apply_whole_files<P, M, S, T>(workspace: P, files: M) -> anyhow::Result<Vec<String>>
where
    P: AsRef<Path>,
    M: IntoIterator<Item = (S, T)>,
    S: Into<String>,
    T: AsRef<str>,
{
    let workspace = workspace.as_ref();
    let mut written = Vec::new();
    for (rel, contents) in files {
        let rel = rel.into();
        let abs = workspace.join(&rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = abs.with_extension("newt-coder-tmp");
        std::fs::write(&tmp, contents.as_ref())?;
        std::fs::rename(&tmp, &abs)?;
        written.push(rel);
    }
    Ok(written)
}

// ── Diff parser ─────────────────────────────────────────────────────────────

fn parse_unified_diff(diff: &str) -> anyhow::Result<Vec<FilePatch>> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut patches = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        // Scan for --- / +++ header pair.
        if lines[i].starts_with("--- ") {
            if i + 1 >= lines.len() || !lines[i + 1].starts_with("+++ ") {
                anyhow::bail!("expected +++ header after --- at line {}", i + 1);
            }

            let path = extract_path(lines[i + 1])?;
            i += 2;

            let mut hunks = Vec::new();
            while i < lines.len() && lines[i].starts_with("@@ ") {
                let (hunk, consumed) = parse_hunk(&lines[i..])?;
                hunks.push(hunk);
                i += consumed;
            }

            if hunks.is_empty() {
                anyhow::bail!("file patch for '{path}' has no hunks");
            }

            patches.push(FilePatch { path, hunks });
        } else {
            i += 1;
        }
    }

    Ok(patches)
}

/// Extract the file path from a `+++ b/path` or `+++ path` line.
fn extract_path(line: &str) -> anyhow::Result<String> {
    let rest = line
        .strip_prefix("+++ ")
        .ok_or_else(|| anyhow::anyhow!("invalid +++ line: {line}"))?;

    // Handle `+++ b/path` (git-style) and `+++ path` (plain).
    let path = rest.strip_prefix("b/").unwrap_or(rest);

    if path.is_empty() || path == "/dev/null" {
        anyhow::bail!("cannot determine target path from: {line}");
    }

    Ok(path.to_string())
}

/// Parse one hunk starting at `lines[0]` which must be `@@ … @@`.
/// Returns the parsed hunk and the number of lines consumed.
fn parse_hunk(lines: &[&str]) -> anyhow::Result<(Hunk, usize)> {
    let header = lines[0];
    let (old_start, old_count, new_start, new_count) = parse_hunk_header(header)?;

    let mut diff_lines = Vec::new();
    let mut i = 1;

    while i < lines.len() {
        let line = lines[i];

        if line.starts_with("@@ ") || line.starts_with("--- ") {
            break;
        }

        if let Some(rest) = line.strip_prefix(' ') {
            diff_lines.push(DiffLine::Context(rest.to_string()));
        } else if let Some(rest) = line.strip_prefix('+') {
            diff_lines.push(DiffLine::Add(rest.to_string()));
        } else if let Some(rest) = line.strip_prefix('-') {
            diff_lines.push(DiffLine::Remove(rest.to_string()));
        } else if line == "\\ No newline at end of file" {
            // Ignore this marker.
        } else {
            // Treat as context (handles empty context lines).
            diff_lines.push(DiffLine::Context(line.to_string()));
        }

        i += 1;
    }

    Ok((
        Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines: diff_lines,
        },
        i,
    ))
}

/// Parse `@@ -old_start,old_count +new_start,new_count @@` header.
fn parse_hunk_header(header: &str) -> anyhow::Result<(usize, usize, usize, usize)> {
    // Strip the leading @@ and trailing @@ (and anything after).
    let inner = header
        .strip_prefix("@@ ")
        .and_then(|s| s.split(" @@").next())
        .ok_or_else(|| anyhow::anyhow!("malformed hunk header: {header}"))?;

    let parts: Vec<&str> = inner.split_whitespace().collect();
    if parts.len() != 2 {
        anyhow::bail!("malformed hunk header: {header}");
    }

    let (old_start, old_count) = parse_range(parts[0].strip_prefix('-').unwrap_or(parts[0]))?;
    let (new_start, new_count) = parse_range(parts[1].strip_prefix('+').unwrap_or(parts[1]))?;

    Ok((old_start, old_count, new_start, new_count))
}

/// Parse `start,count` or just `start` (count defaults to 1).
fn parse_range(s: &str) -> anyhow::Result<(usize, usize)> {
    if let Some((start, count)) = s.split_once(',') {
        Ok((start.parse()?, count.parse()?))
    } else {
        Ok((s.parse()?, 1))
    }
}

// ── Hunk application ────────────────────────────────────────────────────────

fn apply_hunks(original: &str, hunks: &[Hunk]) -> anyhow::Result<String> {
    let old_lines: Vec<&str> = if original.is_empty() {
        Vec::new()
    } else {
        original.lines().collect()
    };

    let mut result = Vec::new();
    let mut old_idx: usize = 0; // 0-based index into old_lines

    for hunk in hunks {
        // old_start is 1-based in the diff header; convert to 0-based.
        let hunk_start = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };

        // Copy lines before this hunk.
        if hunk_start > old_lines.len() {
            anyhow::bail!(
                "hunk starts at line {} but file only has {} lines",
                hunk.old_start,
                old_lines.len()
            );
        }

        while old_idx < hunk_start {
            result.push(old_lines[old_idx].to_string());
            old_idx += 1;
        }

        // Apply hunk lines.
        for diff_line in &hunk.lines {
            match diff_line {
                DiffLine::Context(text) => {
                    if old_idx >= old_lines.len() {
                        anyhow::bail!(
                            "context mismatch: expected line '{}' at position {} but file has only {} lines",
                            text,
                            old_idx + 1,
                            old_lines.len()
                        );
                    }
                    if old_lines[old_idx] != text.as_str() {
                        anyhow::bail!(
                            "context mismatch at line {}: expected '{}', found '{}'",
                            old_idx + 1,
                            text,
                            old_lines[old_idx]
                        );
                    }
                    result.push(text.clone());
                    old_idx += 1;
                }
                DiffLine::Remove(text) => {
                    if old_idx >= old_lines.len() {
                        anyhow::bail!(
                            "remove mismatch: expected line '{}' at position {} but file ended",
                            text,
                            old_idx + 1,
                        );
                    }
                    if old_lines[old_idx] != text.as_str() {
                        anyhow::bail!(
                            "remove mismatch at line {}: expected '{}', found '{}'",
                            old_idx + 1,
                            text,
                            old_lines[old_idx]
                        );
                    }
                    old_idx += 1;
                }
                DiffLine::Add(text) => {
                    result.push(text.clone());
                }
            }
        }
    }

    // Copy remaining lines after the last hunk.
    while old_idx < old_lines.len() {
        result.push(old_lines[old_idx].to_string());
        old_idx += 1;
    }

    // Preserve trailing newline if original had one (or if it's a new file).
    let mut output = result.join("\n");
    if original.is_empty() || original.ends_with('\n') {
        output.push('\n');
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn single_file_add_line() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        fs::write(&file, "line1\nline2\nline3\n").unwrap();

        let diff = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,3 +1,4 @@
 line1
+inserted
 line2
 line3
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "line1\ninserted\nline2\nline3\n");
    }

    #[test]
    fn single_file_remove_line() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        fs::write(&file, "line1\nline2\nline3\n").unwrap();

        let diff = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,3 +1,2 @@
 line1
-line2
 line3
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "line1\nline3\n");
    }

    #[test]
    fn multi_hunk() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("code.rs");
        fs::write(&file, "a\nb\nc\nd\ne\nf\ng\n").unwrap();

        let diff = "\
--- a/code.rs
+++ b/code.rs
@@ -1,3 +1,3 @@
 a
-b
+B
 c
@@ -5,3 +5,3 @@
 e
-f
+F
 g
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "a\nB\nc\nd\ne\nF\ng\n");
    }

    #[test]
    fn new_file() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("new.txt");
        assert!(!file.exists());

        let diff = "\
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "hello\nworld\n");
    }

    #[test]
    fn context_mismatch_rejected() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        fs::write(&file, "actual\nline2\n").unwrap();

        let diff = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,2 +1,2 @@
 expected
-line2
+replaced
";
        let err = apply_patch(tmp.path(), diff).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("context mismatch"),
            "expected context mismatch error, got: {msg}"
        );
    }

    #[test]
    fn malformed_diff_rejected() {
        let tmp = TempDir::new().unwrap();
        let err = apply_patch(tmp.path(), "this is not a diff").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("no file patches found"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn atomic_write_verified() {
        let tmp = TempDir::new().unwrap();
        let file1 = tmp.path().join("ok.txt");
        let file2 = tmp.path().join("fail.txt");
        fs::write(&file1, "aaa\nbbb\n").unwrap();
        fs::write(&file2, "xxx\nyyy\n").unwrap();

        // First file patch is valid, second has a context mismatch.
        let diff = "\
--- a/ok.txt
+++ b/ok.txt
@@ -1,2 +1,2 @@
 aaa
-bbb
+ccc
--- a/fail.txt
+++ b/fail.txt
@@ -1,2 +1,2 @@
 WRONG_CONTEXT
-yyy
+zzz
";
        let err = apply_patch(tmp.path(), diff).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("mismatch"), "expected mismatch error: {msg}");

        // Because we validate all patches before writing, the first file
        // should be unchanged.
        let content1 = fs::read_to_string(&file1).unwrap();
        assert_eq!(content1, "aaa\nbbb\n", "first file should be unchanged");

        let content2 = fs::read_to_string(&file2).unwrap();
        assert_eq!(content2, "xxx\nyyy\n", "second file should be unchanged");
    }

    #[test]
    fn edit_applies_patch() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        fs::write(&file, "line1\nline2\n").unwrap();

        let diff = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,2 +1,2 @@
 line1
-line2
+edited
";
        edit(&file, diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "line1\nedited\n");
    }

    #[test]
    fn apply_whole_files_writes_single_file() {
        let tmp = TempDir::new().unwrap();
        let files = vec![("src/lib.rs".to_string(), "pub fn hello() {}\n".to_string())];
        let written = apply_whole_files(tmp.path(), files).unwrap();
        assert_eq!(written, vec!["src/lib.rs".to_string()]);
        let got = fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap();
        assert_eq!(got, "pub fn hello() {}\n");
    }

    #[test]
    fn apply_whole_files_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let files = vec![("a/b/c/d.rs".to_string(), "fn x() {}\n".to_string())];
        apply_whole_files(tmp.path(), files).unwrap();
        assert!(tmp.path().join("a/b/c/d.rs").exists());
    }

    #[test]
    fn apply_whole_files_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("src/lib.rs");
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(&file, "OLD\n").unwrap();

        let files = vec![("src/lib.rs".to_string(), "NEW\n".to_string())];
        apply_whole_files(tmp.path(), files).unwrap();
        let got = fs::read_to_string(&file).unwrap();
        assert_eq!(got, "NEW\n");
    }

    #[test]
    fn apply_whole_files_no_tmp_residue() {
        // After a successful run, no `.newt-coder-tmp` files should
        // remain anywhere under the workspace.
        let tmp = TempDir::new().unwrap();
        let files = vec![
            ("a.rs".to_string(), "fn a() {}".to_string()),
            ("b.rs".to_string(), "fn b() {}".to_string()),
        ];
        apply_whole_files(tmp.path(), files).unwrap();

        for entry in fs::read_dir(tmp.path()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(
                !name.ends_with(".newt-coder-tmp"),
                "leftover temp file: {name}"
            );
        }
    }

    #[test]
    fn apply_whole_files_accepts_str_slice_values() {
        // Ergonomics: callers should be able to pass &str without
        // .to_string()-ing every value.
        let tmp = TempDir::new().unwrap();
        let files: Vec<(&str, &str)> = vec![("hello.txt", "hi\n")];
        let written = apply_whole_files(tmp.path(), files).unwrap();
        assert_eq!(written, vec!["hello.txt".to_string()]);
        let got = fs::read_to_string(tmp.path().join("hello.txt")).unwrap();
        assert_eq!(got, "hi\n");
    }

    #[test]
    fn edit_error_propagated() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("hello.txt");
        fs::write(&file, "aaa\n").unwrap();

        let diff = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,1 +1,1 @@
 WRONG
";
        let err = edit(&file, diff).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("mismatch"), "expected mismatch error: {msg}");
    }
}
