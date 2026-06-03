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

// ── Pluggable applier backends ───────────────────────────────────────────────

/// A patch-application backend.
///
/// The default is [`FuzzyApplier`] — the in-house unified-diff parser with
/// `git apply -C`-style fuzzy hunk location (tolerant of the off-by-N line
/// numbers and whitespace drift weak local models emit, while still
/// rejecting genuinely-wrong and ambiguous hunks). It is the backend the
/// worker-agent bake-off validated, so it stays the default.
///
/// Alternative backends select at runtime via `NEWT_PATCH_APPLIER`:
///
/// - `fuzzy` (default) — [`FuzzyApplier`].
/// - `diffy` — [`DiffyApplier`], the strict pure-Rust `diffy` crate.
///   Requires the `applier-diffy` cargo feature; without it,
///   `NEWT_PATCH_APPLIER=diffy` warns and falls back to fuzzy.
///
/// Future backends are just more `impl PatchApplier`: a `gix` (gitoxide)
/// applier once `gix-apply` publishes, or a content-addressed
/// applier. The seam exists so the choice of *how* a patch is applied is a
/// swappable instrument, not baked into every call site.
pub trait PatchApplier: Send + Sync {
    /// Validate and apply `diff` rooted at `root`. All-or-nothing: a
    /// multi-file diff either applies fully or leaves the tree untouched.
    fn apply(&self, root: &Path, diff: &str) -> anyhow::Result<()>;
    /// Short stable name for logs/diagnostics (`"fuzzy"`, `"diffy"`).
    fn name(&self) -> &'static str;
}

/// The default in-house fuzzy applier (see [`PatchApplier`]).
pub struct FuzzyApplier;

impl PatchApplier for FuzzyApplier {
    fn apply(&self, root: &Path, diff: &str) -> anyhow::Result<()> {
        fuzzy_apply_patch(root, diff)
    }
    fn name(&self) -> &'static str {
        "fuzzy"
    }
}

/// Strict `diffy`-backed applier. Only compiled with the `applier-diffy`
/// feature; selectable at runtime via `NEWT_PATCH_APPLIER=diffy`.
#[cfg(feature = "applier-diffy")]
pub struct DiffyApplier;

#[cfg(feature = "applier-diffy")]
impl PatchApplier for DiffyApplier {
    fn apply(&self, root: &Path, diff: &str) -> anyhow::Result<()> {
        diffy_apply_patch(root, diff)
    }
    fn name(&self) -> &'static str {
        "diffy"
    }
}

/// Select the applier backend from the `NEWT_PATCH_APPLIER` env var,
/// defaulting to [`FuzzyApplier`].
pub fn applier_from_env() -> Box<dyn PatchApplier> {
    match std::env::var("NEWT_PATCH_APPLIER").ok().as_deref() {
        Some("diffy") => diffy_applier(),
        Some("fuzzy") | None => Box::new(FuzzyApplier),
        Some(other) => {
            tracing::warn!(applier = %other, "unknown NEWT_PATCH_APPLIER; using fuzzy");
            Box::new(FuzzyApplier)
        }
    }
}

#[cfg(feature = "applier-diffy")]
fn diffy_applier() -> Box<dyn PatchApplier> {
    Box::new(DiffyApplier)
}

#[cfg(not(feature = "applier-diffy"))]
fn diffy_applier() -> Box<dyn PatchApplier> {
    tracing::warn!(
        "NEWT_PATCH_APPLIER=diffy requested but the `applier-diffy` feature is \
         not compiled in; falling back to the fuzzy applier"
    );
    Box::new(FuzzyApplier)
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Apply a unified diff to files under `root` using the
/// [`applier_from_env`]-selected backend (fuzzy by default).
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
    applier_from_env().apply(root, diff)
}

/// The in-house fuzzy applier implementation (the default backend).
fn fuzzy_apply_patch(root: &Path, diff: &str) -> anyhow::Result<()> {
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

// ── Strict `diffy` backend (feature = "applier-diffy") ───────────────────────

/// Strict applier built on the `diffy` crate. Splits a (possibly
/// multi-file) unified diff into per-file sections and applies each with
/// `diffy::apply`. Validates every file before writing (atomic
/// temp-then-rename), matching the fuzzy applier's all-or-nothing
/// semantics. Unlike the fuzzy backend, `diffy` is strict about hunk
/// headers/context — the point of offering it as an option.
#[cfg(feature = "applier-diffy")]
fn diffy_apply_patch(root: &Path, diff: &str) -> anyhow::Result<()> {
    let sections = split_file_sections(diff);
    if sections.is_empty() {
        anyhow::bail!("no file patches found in diff");
    }

    let mut results: Vec<(std::path::PathBuf, String)> = Vec::new();
    for section in &sections {
        let patch = diffy::Patch::from_str(section)
            .map_err(|e| anyhow::anyhow!("diffy parse error: {e}"))?;
        let plus = section
            .lines()
            .find(|l| l.starts_with("+++ "))
            .ok_or_else(|| anyhow::anyhow!("diff section missing +++ header"))?;
        let rel = extract_path(plus)?;
        let file_path = root.join(&rel);
        let original = if file_path.exists() {
            std::fs::read_to_string(&file_path)?
        } else {
            String::new()
        };
        let patched = diffy::apply(&original, &patch)
            .map_err(|e| anyhow::anyhow!("diffy rejected patch for {rel}: {e}"))?;
        results.push((file_path, patched));
    }

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

/// Split a unified diff into per-file sections, each beginning at a
/// `--- ` header line. Any preamble before the first `--- ` (a
/// `diff --git` line, prose) is dropped.
#[cfg(feature = "applier-diffy")]
fn split_file_sections(diff: &str) -> Vec<String> {
    let mut sections: Vec<String> = Vec::new();
    let mut cur: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("--- ") {
            if let Some(prev) = cur.take() {
                sections.push(prev);
            }
            cur = Some(String::new());
        }
        if let Some(buf) = cur.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(last) = cur {
        sections.push(last);
    }
    sections
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

/// How far (in lines) to search on either side of the hunk header's
/// hinted `old_start` when the hunk doesn't apply exactly there. Mirrors
/// the bounded window of `patch --fuzz` / `git apply -C`: weak local
/// models routinely emit hunks whose line numbers are off by a handful
/// of lines, but a hunk that doesn't match anywhere within this window
/// is genuinely wrong and must still be rejected.
const SEARCH_WINDOW: usize = 64;

/// Whitespace-tolerance level used when comparing a hunk's
/// context/remove lines against the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuzzLevel {
    /// Lines must match byte-for-byte.
    Exact,
    /// Lines match after stripping trailing whitespace from both sides.
    TrailingWs,
    /// Lines match after a full `trim()` (leading + trailing) of both.
    Trim,
}

/// Compare a hunk's expected line against the file's actual line under
/// the given fuzz level.
fn lines_match(expected: &str, actual: &str, level: FuzzLevel) -> bool {
    match level {
        FuzzLevel::Exact => expected == actual,
        FuzzLevel::TrailingWs => expected.trim_end() == actual.trim_end(),
        FuzzLevel::Trim => expected.trim() == actual.trim(),
    }
}

/// The "old side" lines a hunk consumes from the original file, in
/// order: every context (` `) and remove (`-`) line. Add (`+`) lines
/// contribute nothing to the old side. This is the run we search for.
fn hunk_old_side(hunk: &Hunk) -> Vec<&str> {
    hunk.lines
        .iter()
        .filter_map(|l| match l {
            DiffLine::Context(t) | DiffLine::Remove(t) => Some(t.as_str()),
            DiffLine::Add(_) => None,
        })
        .collect()
}

/// Does the hunk's old-side run match the file starting at `at` under
/// `level`? `at + old_side.len()` must be within bounds.
fn old_side_matches_at(old_side: &[&str], old_lines: &[&str], at: usize, level: FuzzLevel) -> bool {
    if at + old_side.len() > old_lines.len() {
        return false;
    }
    old_side
        .iter()
        .zip(&old_lines[at..at + old_side.len()])
        .all(|(exp, act)| lines_match(exp, act, level))
}

/// Locate where a hunk actually applies in `old_lines`.
///
/// Search strategy (mirrors `patch --fuzz` / `git apply -C`):
///
/// 1. Start from the header's hinted `old_start` (converted to 0-based),
///    clamped into the file. Walk offsets outward in the order
///    `0, +1, -1, +2, -2, …` up to ±[`SEARCH_WINDOW`] lines, clamped to
///    the file bounds. The **first** offset whose old-side run matches
///    exactly wins — an exact match at the hint is therefore always
///    preferred, preserving the previous behavior for clean diffs.
/// 2. If no offset matches exactly, retry the same outward walk with
///    trailing-whitespace tolerance, then with full `trim()` tolerance.
///    A whitespace-fuzzed match is only accepted when it is
///    **unambiguous**: exactly one position in the whole file matches at
///    that level. (At the `Exact` level we don't require uniqueness —
///    an exact run is its own justification, just like `patch`.)
///
/// Returns the 0-based start index and the fuzz level used, or `None` if
/// nothing matches even with whitespace tolerance within the window.
fn locate_hunk(hunk: &Hunk, old_lines: &[&str]) -> Option<(usize, FuzzLevel)> {
    let old_side = hunk_old_side(hunk);

    // A pure-insertion hunk (no context, no removes) carries no anchor;
    // it applies at its hinted position verbatim.
    if old_side.is_empty() {
        let hint = hunk.old_start.saturating_sub(1).min(old_lines.len());
        return Some((hint, FuzzLevel::Exact));
    }

    let hint = if hunk.old_start == 0 {
        0
    } else {
        (hunk.old_start - 1).min(old_lines.len())
    };

    // Candidate offsets, outward from the hint: 0, +1, -1, +2, -2, …
    let candidate_positions = || {
        std::iter::once(hint).chain((1..=SEARCH_WINDOW).flat_map(move |d| {
            let up = hint.checked_add(d);
            let down = hint.checked_sub(d);
            [up, down].into_iter().flatten()
        }))
    };

    // Pass 1: exact. First match wins (prefers the hint).
    for at in candidate_positions() {
        if old_side_matches_at(&old_side, old_lines, at, FuzzLevel::Exact) {
            return Some((at, FuzzLevel::Exact));
        }
    }

    // Passes 2 & 3: whitespace-tolerant, but only accept an *unambiguous*
    // match (exactly one position in the file matches at that level).
    for level in [FuzzLevel::TrailingWs, FuzzLevel::Trim] {
        let mut found: Option<usize> = None;
        let mut count = 0usize;
        // Scan the whole file (not just the window) to judge ambiguity,
        // but only *accept* a position that's also inside the window.
        for at in 0..=old_lines.len().saturating_sub(old_side.len()) {
            if old_side_matches_at(&old_side, old_lines, at, level) {
                count += 1;
                let within_window = at.abs_diff(hint) <= SEARCH_WINDOW;
                if within_window && found.is_none() {
                    found = Some(at);
                }
            }
        }
        if count == 1 {
            if let Some(at) = found {
                return Some((at, level));
            }
        }
    }

    None
}

fn apply_hunks(original: &str, hunks: &[Hunk]) -> anyhow::Result<String> {
    let old_lines: Vec<&str> = if original.is_empty() {
        Vec::new()
    } else {
        original.lines().collect()
    };

    let mut result = Vec::new();
    let mut old_idx: usize = 0; // 0-based index into old_lines

    for hunk in hunks {
        // Find where this hunk actually applies. We no longer trust the
        // header's `old_start` blindly — weak local models emit hunks
        // with slightly-off line numbers and fuzzy context. `locate_hunk`
        // searches outward from the hint and, failing an exact match,
        // retries with whitespace tolerance (accepting only unambiguous
        // matches). It returns `None` only when nothing matches even
        // fuzzily within the window — that is a genuine context mismatch.
        let (hunk_start, _fuzz) = locate_hunk(hunk, &old_lines).ok_or_else(|| {
            // Build a message that still contains "context mismatch" so
            // downstream code/tests that grep for it keep working, while
            // surfacing the first anchor line we failed to place.
            let first_anchor = hunk
                .lines
                .iter()
                .find_map(|l| match l {
                    DiffLine::Context(t) | DiffLine::Remove(t) => Some(t.clone()),
                    DiffLine::Add(_) => None,
                })
                .unwrap_or_default();
            anyhow::anyhow!(
                "context mismatch: hunk near line {} does not match the file \
                 (no location found within {} lines even with whitespace \
                 tolerance); first expected context/remove line was '{}'",
                hunk.old_start,
                SEARCH_WINDOW,
                first_anchor
            )
        })?;

        // `locate_hunk` clamps to file bounds, so this can't exceed len,
        // but guard defensively (and to keep the old error wording for
        // out-of-range starts on empty/short files).
        if hunk_start > old_lines.len() {
            anyhow::bail!(
                "hunk starts at line {} but file only has {} lines",
                hunk.old_start,
                old_lines.len()
            );
        }

        // The located start may be before or after where we currently
        // are; if a previous hunk already advanced past it we cannot
        // rewind (hunks must apply in order). Copy forward to the start.
        if hunk_start < old_idx {
            anyhow::bail!(
                "context mismatch: hunk near line {} resolves to line {} which \
                 overlaps an already-applied hunk",
                hunk.old_start,
                hunk_start + 1
            );
        }
        while old_idx < hunk_start {
            result.push(old_lines[old_idx].to_string());
            old_idx += 1;
        }

        // Apply hunk lines at the located position. Because `locate_hunk`
        // already verified the old-side run matches here (exactly or
        // fuzzily), we copy the *file's* line for context/remove anchors
        // rather than the hunk's text — so whitespace-fuzzed context does
        // not corrupt the surrounding lines.
        for diff_line in &hunk.lines {
            match diff_line {
                DiffLine::Context(_) => {
                    // Invariant from locate_hunk: in bounds and matching.
                    result.push(old_lines[old_idx].to_string());
                    old_idx += 1;
                }
                DiffLine::Remove(_) => {
                    // Invariant from locate_hunk: in bounds and matching.
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

    // ── Fuzzy hunk matching (weak-model tolerance) ──────────────────────────

    #[test]
    fn hunk_old_start_off_by_plus_two_still_applies() {
        // The model emitted a hunk whose header points at line 3 but the
        // anchored context actually lives at line 1 (off by +2). A strict
        // matcher would reject it; the fuzzy locator must find the real
        // position and apply there.
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        fs::write(&file, "alpha\nbeta\ngamma\ndelta\n").unwrap();

        let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -3,2 +3,3 @@
 alpha
+INSERTED
 beta
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "alpha\nINSERTED\nbeta\ngamma\ndelta\n");
    }

    #[test]
    fn hunk_old_start_off_by_minus_two_still_applies() {
        // Symmetric case: header hints a later line than reality.
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        fs::write(&file, "a\nb\nc\nTARGET\nd\ne\n").unwrap();

        // Header says line 2, real anchor is line 4 -> offset -2 from hint.
        let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -6,1 +6,2 @@
 TARGET
+AFTER
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "a\nb\nc\nTARGET\nAFTER\nd\ne\n");
    }

    #[test]
    fn hunk_with_trailing_whitespace_context_applies() {
        // The model's context line carries trailing whitespace the real
        // file lacks. Exact match fails; trailing-ws fuzz must rescue it,
        // and the surrounding file line must NOT be corrupted with the
        // model's stray whitespace.
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        fs::write(&file, "pub fn hello() {\n    body();\n}\n").unwrap();

        // Note the trailing spaces after the context lines below.
        let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,4 @@
 pub fn hello() {
+    setup();
     body();
 }
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(
            result, "pub fn hello() {\n    setup();\n    body();\n}\n",
            "context must come from the file, not the fuzzed hunk text"
        );
    }

    #[test]
    fn hunk_with_leading_whitespace_diff_applies_via_trim() {
        // Indentation drift: the model's context lines are indented
        // differently than the file. Only full-trim fuzz can match.
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        fs::write(&file, "fn main() {\n    let x = 1;\n}\n").unwrap();

        let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,4 @@
 fn main() {
 let x = 1;
+    let y = 2;
 }
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        // The file's own indentation is preserved for the matched line.
        assert_eq!(result, "fn main() {\n    let x = 1;\n    let y = 2;\n}\n");
    }

    #[test]
    fn genuinely_non_matching_hunk_still_rejected() {
        // Safety property the ACP worker relies on: a hunk whose context
        // matches nothing in the file (even fuzzily) must be rejected.
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        fs::write(&file, "fn main() { println!(\"hello\"); }\n").unwrap();

        let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -1,1 +1,1 @@
 fn TOTALLY_WRONG() {
-old
+new
";
        let err = apply_patch(tmp.path(), diff).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("context mismatch"),
            "expected context mismatch error, got: {msg}"
        );
        // File must be untouched.
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "fn main() { println!(\"hello\"); }\n"
        );
    }

    #[test]
    fn exact_match_preferred_over_later_fuzzy_match() {
        // When the hint lands on an exact match, that location wins even
        // if a whitespace-fuzzy match exists elsewhere. Guards against the
        // locator drifting to the wrong (but fuzzily-similar) block.
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        fs::write(&file, "marker\nmarker \nother\n").unwrap();

        // Exact "marker" is at line 1; a trailing-ws variant is at line 2.
        let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -1,1 +1,2 @@
 marker
+ADDED
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "marker\nADDED\nmarker \nother\n");
    }

    #[test]
    fn ambiguous_whitespace_fuzz_is_rejected() {
        // If a whitespace-fuzzed anchor matches more than one place in the
        // file, we refuse rather than guess. (No exact match exists here:
        // the hunk context has a trailing space the file lines lack.)
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        fs::write(&file, "dup\nfiller\ndup\n").unwrap();

        // The context line carries a trailing space the file's "dup" lines
        // lack, so NO exact match exists. Both "dup" lines then match only
        // after whitespace trimming → two candidates → ambiguous → reject.
        // (The trailing space is kept explicit in the `" dup "` literal so a
        // formatter can't silently strip it.)
        let ctx = " dup "; // diff context marker + "dup" + trailing space
        let diff = format!("--- a/lib.rs\n+++ b/lib.rs\n@@ -1,1 +1,2 @@\n{ctx}\n+ADDED\n");
        let err = apply_patch(tmp.path(), &diff).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("context mismatch"),
            "ambiguous fuzzy match must be rejected, got: {msg}"
        );
    }

    #[test]
    fn clean_diff_with_correct_line_numbers_unaffected() {
        // Regression guard: a perfectly-formed diff still applies at its
        // header position with no fuzz.
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();

        let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -2,2 +2,2 @@
 two
-three
+THREE
";
        apply_patch(tmp.path(), diff).unwrap();
        let result = fs::read_to_string(&file).unwrap();
        assert_eq!(result, "one\ntwo\nTHREE\nfour\n");
    }
}
