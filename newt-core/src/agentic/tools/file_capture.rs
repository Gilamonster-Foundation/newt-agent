//! Local observations around one file operation; no journal or persisted schema.
//!
//! The real-file tests below ground the pure absent/present/unavailable and
//! observed-before/verified-after decisions in actual file objects and bytes.

use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;

use crate::caveats::Scope;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TextSnapshot {
    Absent,
    Present(String),
    Unavailable(&'static str),
}

fn open_for_scope(scope: &Scope<String>, path: &Path, nofollow: bool) -> io::Result<File> {
    if !super::tui_permits_path(scope, &path.to_string_lossy()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "scope does not permit this read",
        ));
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let full = path.to_string_lossy();
        match super::object_bound_target(scope, &full) {
            Some(Some((root, relative))) => {
                crate::fs_cap::WorkspaceDir::open_granted_file(Path::new(root), &relative, nofollow)
            }
            Some(None) => super::open_regular_file(path, nofollow),
            None => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "scope does not permit this read",
            )),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // Diagnostic physical containment, not an atomic capability on these
        // platforms. Do not turn the known lexical mutation residual into a
        // new preimage disclosure through the receipt.
        if let Scope::Only(roots) = scope {
            if !roots.iter().any(|root| {
                super::artifact_path_is_physically_within_workspace(Path::new(root), path)
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "physical path is outside the read scope",
                ));
            }
        }
        super::open_regular_file(path, nofollow)
    }
}

pub(super) fn capture(scope: &Scope<String>, path: &Path) -> TextSnapshot {
    if !super::tui_permits_path(scope, &path.to_string_lossy()) {
        return TextSnapshot::Unavailable("fs_read was not granted");
    }
    let mut file = match open_for_scope(scope, path, true) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return TextSnapshot::Absent,
        Err(_) => {
            return TextSnapshot::Unavailable(
                "the file could not be read as an authorized regular file",
            )
        }
    };
    let before = file.metadata().ok();
    let limit = super::file_change::MAX_VERSION_BYTES as u64;
    if before
        .as_ref()
        .is_some_and(|metadata| metadata.len() > limit)
    {
        return TextSnapshot::Unavailable("receipt capture limit exceeded (256 KiB per version)");
    }
    // Metadata can become stale while reading. Read at most one byte beyond
    // the budget, and never pass a truncated version to the diff engine.
    let mut bytes = Vec::new();
    if (&mut file).take(limit + 1).read_to_end(&mut bytes).is_err() {
        return TextSnapshot::Unavailable("the file could not be read as UTF-8 text");
    }
    if bytes.len() as u64 > limit {
        return TextSnapshot::Unavailable("receipt capture limit exceeded (256 KiB per version)");
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return TextSnapshot::Unavailable("the file could not be read as UTF-8 text");
    };
    let after = file.metadata().ok();
    match (before, after) {
        (Some(before), Some(after))
            if before.len() == after.len()
                && after.len() == text.len() as u64
                && before.modified().ok() == after.modified().ok() =>
        {
            TextSnapshot::Present(text)
        }
        _ => TextSnapshot::Unavailable("the file changed while being read"),
    }
}

/// A failed or partial operation still has an observed result. Never build its
/// displayed patch from the requested bytes in place of this postimage.
pub(super) fn receipt(path: &str, before: &TextSnapshot, after: &TextSnapshot) -> Receipt {
    use TextSnapshot::{Absent, Present, Unavailable};
    let (old, new) = match (before, after) {
        (Unavailable(reason), _) | (_, Unavailable(reason)) => {
            return Receipt::plain(format!("\n\nfile-change receipt unavailable: {reason}"));
        }
        (Absent, Absent) => {
            return Receipt::plain("\n\nNo file was present before or after the operation.".into())
        }
        (Absent, Present(after)) => (None, Some(after.as_str())),
        (Present(before), Absent) => (Some(before.as_str()), None),
        (Present(before), Present(after)) => (Some(before.as_str()), Some(after.as_str())),
    };
    match super::file_change::from_versions(path, old, new) {
        Ok(model) => {
            let unchanged = old.is_some() && old == new;
            let text = format!(
                "\n\n{}",
                super::file_change::receipt_from_model(&model, unchanged)
            );
            let headline = format!(
                "\n\n{}",
                super::file_change::receipt_headline(&model, unchanged)
            );
            let captured = (!unchanged).then(|| CapturedChange {
                model,
                path: path.into(),
                before: old.map(str::to_owned),
                after: new.map(str::to_owned),
            });
            Receipt {
                text,
                headline,
                captured,
            }
        }
        Err(error) => Receipt::plain(format!("\n\nfile-change receipt unavailable: {error}")),
    }
}

struct CapturedChange {
    model: newtui::diff::ChangeSet,
    path: String,
    before: Option<String>,
    after: Option<String>,
}

pub(super) struct Receipt {
    text: String,
    /// The model-facing first line of `text` (`\n\nModified (+1 -1)`).
    headline: String,
    captured: Option<CapturedChange>,
}

impl Receipt {
    fn plain(text: String) -> Self {
        Self {
            headline: text.clone(),
            text,
            captured: None,
        }
    }

    /// A SUCCESSFUL mutation: the model gets `prefix + headline + suffix`
    /// ("wrote x (3 lines)\n\nModified (+1 -1)"); the operator's display
    /// keeps the full receipt and its styled diff. The model wrote the change,
    /// so echoing it back only costs context — a 1,497-line new file's echo
    /// spilled and cost a round to read back (live 2026-09-23). Failures keep
    /// [`Self::present`]: the observed state after a failed or partial
    /// operation is information the model does not already have.
    pub(super) fn present_success(
        self,
        prefix: String,
        suffix: &str,
        presentation: &mut dyn super::ToolPresentation,
    ) -> String {
        let model = format!("{prefix}{}{suffix}", self.headline);
        let full = self.present(prefix, suffix, presentation);
        presentation.display_source(full);
        model
    }

    /// Record the receipt's exact byte range while assembling this result.
    /// This one-shot hint follows the existing result; it is not retained data.
    pub(super) fn present(
        self,
        prefix: String,
        suffix: &str,
        presentation: &mut dyn super::ToolPresentation,
    ) -> String {
        let range = prefix.len()..prefix.len() + self.text.len();
        let output = format!("{prefix}{}{suffix}", self.text);
        if let Some(captured) = self.captured {
            presentation.file_change(std::sync::Arc::new(
                crate::agentic::FileChangePresentation::new(
                    captured.model,
                    captured.path,
                    captured.before,
                    captured.after,
                    self.text,
                    range,
                ),
            ));
        }
        present(output, presentation)
    }
}

pub(super) fn matches(scope: &Scope<String>, path: &Path, expected: &[u8]) -> bool {
    open_for_scope(scope, path, false)
        .and_then(|file| super::file_contents_match(file, expected))
        .unwrap_or(false)
}

/// Use the established untrusted-display policy; tabs additionally get an
/// explicit marker in file reviews instead of moving the terminal cursor.
pub(super) fn display_text(text: &str) -> std::borrow::Cow<'_, str> {
    let safe = crate::notes_scan::neutralize_for_display(text);
    if safe.contains('\t') {
        std::borrow::Cow::Owned(safe.replace('\t', "<U+0009>"))
    } else {
        safe
    }
}

/// Keep the raw canonical receipt in the returned String for observation and
/// model use. Only the established terminal presentation override is changed.
pub(super) fn present(output: String, presentation: &mut dyn super::ToolPresentation) -> String {
    if output
        .chars()
        .any(|ch| ch == '\t' || crate::notes_scan::display_hazard_name(ch).is_some())
    {
        let visible = display_text(&output);
        presentation.override_result(format!(
            "{visible}\n\n[file-change display escapes control characters; this view is not a raw patch.]"
        ));
    }
    output
}

pub(super) fn verified_after(
    observed: &TextSnapshot,
    scope: &Scope<String>,
    path: &Path,
    expected: Option<&[u8]>,
) -> bool {
    match (observed, expected) {
        (TextSnapshot::Present(actual), Some(expected)) if actual.as_bytes() != expected => {
            return false;
        }
        (TextSnapshot::Present(_), None) | (TextSnapshot::Absent, Some(_)) => return false,
        _ => {}
    }
    match expected {
        Some(expected) => matches(scope, path, expected),
        None => absent(scope, path),
    }
}

pub(super) fn current_preimage(
    observed: &TextSnapshot,
    scope: &Scope<String>,
    path: &Path,
    expected: &[u8],
) -> bool {
    match observed {
        TextSnapshot::Absent => return false,
        TextSnapshot::Present(actual) if actual.as_bytes() != expected => return false,
        _ => {}
    }
    matches(scope, path, expected)
}

pub(super) fn read_for_edit(
    scope: &Scope<String>,
    path: &Path,
    label: &str,
) -> Result<String, String> {
    let read = (|| {
        let mut file = open_for_scope(scope, path, false)?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok::<_, io::Error>(text)
    })();
    match read {
        Ok(text) => Ok(text),
        Err(error) => {
            if error.kind() == io::ErrorKind::PermissionDenied {
                return Err(super::denied_fs_result("fs_write", label));
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if super::is_fs_containment_denied(&error) {
                return Err(super::denied_fs_result("fs_write", label));
            }
            Err(format!("error: reading {label}: {error}"))
        }
    }
}

pub(super) fn failure(output: String, receipt: &str) -> String {
    let prefix = if output.starts_with("error:") || output.starts_with("capability denied:") {
        ""
    } else {
        "error: "
    };
    format!("{prefix}{output}\nObserved file state after the failed operation:{receipt}")
}

pub(super) fn absent(scope: &Scope<String>, path: &Path) -> bool {
    matches!(open_for_scope(scope, path, true), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

/// Preserve final-link mutation policy, but refuse actual FIFO/device targets
/// before the legacy shrink/edit reads or writes could block on them.
pub(super) fn regular_target(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) => metadata.is_file(),
        // Unknown type is not proof of a FIFO/device. Preserve the authorized
        // open/write path's error handling instead of inventing a type claim.
        Err(_) => true,
    }
}

/// Lines `start..=end` (1-based) of `text`, byte-exact, or a refusal naming
/// the file's length.
fn line_range<'a>(text: &'a str, start: usize, end: usize) -> Result<(usize, usize), String> {
    let lines: Vec<&'a str> = text.split_inclusive('\n').collect();
    if start == 0 || start > end || end > lines.len() {
        return Err(format!(
            "error: lines {start}-{end} are not a range of this file ({} lines)",
            lines.len()
        ));
    }
    let from: usize = lines[..start - 1].iter().map(|l| l.len()).sum();
    let to: usize = from + lines[start - 1..end].iter().map(|l| l.len()).sum::<usize>();
    Ok((from, to))
}

/// `header` followed by lines `start..=end` of `source`, copied byte for byte.
/// Moving code this way means the model never retypes it: live 2026-09-23 a
/// retyped 1,263-line move fabricated symbols and was deleted twice.
pub(super) fn copy_line_range(
    header: &str,
    source: &str,
    start: usize,
    end: usize,
) -> Result<String, String> {
    let (from, to) = line_range(source, start, end)?;
    let mut out = header.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&source[from..to]);
    Ok(out)
}

/// `text` with lines `start..=end` replaced by `replacement` (empty deletes
/// them), every other byte untouched — so removing a large block does not
/// require quoting it back verbatim as an `old_string`.
pub(super) fn replace_line_range(
    text: &str,
    start: usize,
    end: usize,
    replacement: &str,
) -> Result<String, String> {
    let (from, to) = line_range(text, start, end)?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..from]);
    out.push_str(replacement);
    if !replacement.is_empty() && !replacement.ends_with('\n') && to < text.len() {
        out.push('\n');
    }
    out.push_str(&text[to..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unavailable_metadata_does_not_claim_a_nonregular_file_type() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("loop");
        std::os::unix::fs::symlink("loop", &path).unwrap();
        assert!(std::fs::metadata(&path).is_err());
        assert!(regular_target(&path));
        assert!(!regular_target(directory.path()));
    }

    #[test]
    fn oversized_snapshots_are_unavailable_instead_of_partial_diffs() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("file");
        // A sparse file witnesses the allocation boundary without allocating
        // its contents in this test or asking Diffy to process it.
        std::fs::File::create(&path)
            .unwrap()
            .set_len(256 * 1024 + 1)
            .unwrap();
        let before = capture(&Scope::All, &path);
        assert!(matches!(before, TextSnapshot::Unavailable(_)), "{before:?}");
        let output = receipt("file", &before, &TextSnapshot::Present("small\n".into())).text;
        assert!(output.contains("unavailable"), "{output}");
        assert!(output.contains("limit"), "{output}");
        assert!(!output.contains("```diff"), "{output}");
    }

    #[test]
    fn a_later_matching_file_does_not_validate_a_different_observed_postimage() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "old\n").unwrap();
        let before = capture(&Scope::All, &path);
        std::fs::write(&path, "partial\n").unwrap();
        let after = capture(&Scope::All, &path);
        std::fs::write(&path, "requested\n").unwrap();
        assert!(!verified_after(
            &after,
            &Scope::All,
            &path,
            Some(b"requested\n")
        ));
        let output = failure(
            "error: injected write failure".into(),
            &receipt("file", &before, &after).text,
        );
        assert!(output.contains("-old\n+partial\n"), "{output}");
        assert!(!output.contains("+requested"), "{output}");
    }

    #[test]
    fn a_stale_captured_preimage_is_refused_even_if_the_file_changes_back() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, "changed\n").unwrap();
        let before = capture(&Scope::All, &path);
        std::fs::write(&path, "initial\n").unwrap();
        assert!(!current_preimage(&before, &Scope::All, &path, b"initial\n"));
        std::fs::write(&path, "newer\n").unwrap();
        assert!(!current_preimage(&before, &Scope::All, &path, b"changed\n"));
    }

    #[test]
    fn snapshots_separate_absence_from_denial_and_invalid_utf8() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("file");
        assert_eq!(capture(&Scope::All, &path), TextSnapshot::Absent);
        assert!(matches!(
            capture(&Scope::none(), &path),
            TextSnapshot::Unavailable(_)
        ));
        std::fs::write(&path, [0xff]).unwrap();
        assert!(matches!(
            capture(&Scope::All, &path),
            TextSnapshot::Unavailable(_)
        ));
        std::fs::write(&path, "\t界\r\nno trailing newline").unwrap();
        assert_eq!(
            capture(&Scope::All, &path),
            TextSnapshot::Present("\t界\r\nno trailing newline".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn physical_read_scopes_and_final_links_do_not_disclose_outside_contents() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::write(outside.path().join("file"), "outside\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let scoped = Scope::only([root.path().to_string_lossy().into_owned()]);
        assert!(matches!(
            capture(&scoped, &root.path().join("escape/file")),
            TextSnapshot::Unavailable(_)
        ));
        std::fs::write(root.path().join("inside"), "inside\n").unwrap();
        std::os::unix::fs::symlink("inside", root.path().join("link")).unwrap();
        assert!(matches!(
            capture(&scoped, &root.path().join("link")),
            TextSnapshot::Unavailable(_)
        ));
        // macOS's descriptor walk conservatively refuses even in-tree links
        // (fs_cap module docs); elsewhere verification follows a contained link.
        assert_eq!(
            matches(&scoped, &root.path().join("link"), b"inside\n"),
            !cfg!(target_os = "macos")
        );
    }
}

/// Pure tests for moving code by line range — no filesystem.
#[cfg(test)]
mod line_range_tests {
    use super::{copy_line_range, replace_line_range};

    const SRC: &str = "l1\nl2\nl3\nl4\nl5\n";

    #[test]
    fn a_copied_range_is_byte_exact_and_follows_the_typed_header() {
        assert_eq!(
            copy_line_range("use x;\n", SRC, 2, 4).unwrap(),
            "use x;\nl2\nl3\nl4\n"
        );
        assert_eq!(copy_line_range("", SRC, 5, 5).unwrap(), "l5\n");
    }

    #[test]
    fn a_header_without_a_trailing_newline_still_starts_the_copy_on_its_own_line() {
        assert_eq!(
            copy_line_range("use x;", SRC, 1, 1).unwrap(),
            "use x;\nl1\n"
        );
    }

    #[test]
    fn a_replaced_range_keeps_every_other_line_byte_exact() {
        assert_eq!(
            replace_line_range(SRC, 2, 4, "mod m;\n").unwrap(),
            "l1\nmod m;\nl5\n"
        );
        // Deletion: an empty replacement removes the lines cleanly.
        assert_eq!(replace_line_range(SRC, 2, 4, "").unwrap(), "l1\nl5\n");
        // A replacement missing its newline does not glue onto the next line.
        assert_eq!(
            replace_line_range(SRC, 2, 4, "mod m;").unwrap(),
            "l1\nmod m;\nl5\n"
        );
    }

    #[test]
    fn a_bad_range_is_refused_with_the_file_length() {
        for (start, end) in [(0, 2), (3, 2), (4, 9)] {
            let err = replace_line_range(SRC, start, end, "").unwrap_err();
            assert!(err.contains("5 lines"), "{err}");
            let err = copy_line_range("", SRC, start, end).unwrap_err();
            assert!(err.contains("5 lines"), "{err}");
        }
    }
}
