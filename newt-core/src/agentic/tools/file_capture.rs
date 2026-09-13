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
    #[cfg(target_os = "linux")]
    {
        let full = path.to_string_lossy();
        match super::object_bound_target(scope, &full) {
            Some(Some((root, relative))) => {
                let directory =
                    crate::fs_cap::WorkspaceDir::open_root(Path::new(root)).map_err(|error| {
                        io::Error::other(format!("could not open authorized root: {error}"))
                    })?;
                directory.open_regular(&relative, nofollow)
            }
            Some(None) => super::open_regular_file(path, nofollow),
            None => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "scope does not permit this read",
            )),
        }
    }
    #[cfg(not(target_os = "linux"))]
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
pub(super) fn receipt(path: &str, before: &TextSnapshot, after: &TextSnapshot) -> String {
    use TextSnapshot::{Absent, Present, Unavailable};
    let (old, new) = match (before, after) {
        (Unavailable(reason), _) | (_, Unavailable(reason)) => {
            return format!("\n\nfile-change receipt unavailable: {reason}");
        }
        (Absent, Absent) => return "\n\nNo file was present before or after the operation.".into(),
        (Absent, Present(after)) => (None, Some(after.as_str())),
        (Present(before), Absent) => (Some(before.as_str()), None),
        (Present(before), Present(after)) => (Some(before.as_str()), Some(after.as_str())),
    };
    match super::file_change::receipt(path, old, new) {
        Ok(receipt) => format!("\n\n{receipt}"),
        Err(error) => format!("\n\nfile-change receipt unavailable: {error}"),
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
            #[cfg(target_os = "linux")]
            if super::is_fs_containment_denied(&error) {
                return Err(super::denied_fs_result("fs_write", label));
            }
            Err(format!("error reading {label}: {error}"))
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
        let output = receipt("file", &before, &TextSnapshot::Present("small\n".into()));
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
            &receipt("file", &before, &after),
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
        assert!(matches(&scoped, &root.path().join("link"), b"inside\n"));
    }
}
