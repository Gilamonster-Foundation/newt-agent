//! Text projection of an already captured, verified file operation.
//!
//! No filesystem or persistence lives here. A missing version means proven
//! absence; an unreadable or unauthorized preimage must not call this seam.

use std::fmt::{self, Write as _};

use newtui::diff::{ChangeSet, FileKind, ParseError};

// These bound the additional snapshots, line matching, and rendered receipt.
// Larger operations remain allowed; their receipt explicitly becomes unavailable.
pub(super) const MAX_VERSION_BYTES: usize = 256 * 1024;
const MAX_DIFF_LINES: usize = 4096;

#[derive(Debug)]
pub(super) enum ReceiptError {
    NoVersions,
    Limit,
    PatchHeaders,
    Parse(ParseError),
}

impl fmt::Display for ReceiptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoVersions => formatter.write_str("neither file version exists"),
            Self::Limit => formatter.write_str(
                "receipt limit exceeded (256 KiB per version, 4096 lines across both versions)",
            ),
            Self::PatchHeaders => {
                formatter.write_str("diff engine returned unexpected file headers")
            }
            Self::Parse(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ReceiptError {}

pub(super) fn from_versions(
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<ChangeSet, ReceiptError> {
    if before.is_none() && after.is_none() {
        return Err(ReceiptError::NoVersions);
    }
    let versions = [before.unwrap_or(""), after.unwrap_or("")];
    if versions.iter().any(|text| text.len() > MAX_VERSION_BYTES)
        || versions
            .iter()
            .map(|text| text.split_inclusive('\n').count())
            .sum::<usize>()
            > MAX_DIFF_LINES
    {
        return Err(ReceiptError::Limit);
    }
    let label = quoted_path(path);
    let old_label = if before.is_some() {
        &label
    } else {
        "/dev/null"
    };
    let new_label = if after.is_some() { &label } else { "/dev/null" };
    let mut options = diffy::DiffOptions::new();
    options
        .set_original_filename("before")
        .set_modified_filename("after");
    let patch = options.create_patch(before.unwrap_or(""), after.unwrap_or(""));
    // Diffy quotes filenames itself, and its escaping differs from Git C
    // quoting. Use fixed engine labels and replace only their known headers;
    // source hunks are preserved byte-for-byte and our labels are quoted once.
    let engine_text = patch.to_string();
    let hunks = engine_text
        .strip_prefix("--- before\n+++ after\n")
        .ok_or(ReceiptError::PatchHeaders)?;
    let unified = format!("--- {old_label}\n+++ {new_label}\n{hunks}");
    // An empty textual delta still distinguishes creation/deletion of an empty
    // file from an unchanged existing file. Diffy retains these file headers.
    newtui::diff::from_unified(&unified).map_err(ReceiptError::Parse)
}

pub(super) fn receipt(
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<String, ReceiptError> {
    let model = from_versions(path, before, after)?;
    let file = &model.files()[0];
    let kind = if before.is_some() && before == after {
        "No content change"
    } else {
        match file.kind() {
            FileKind::Added => "Added",
            FileKind::Deleted => "Deleted",
            _ => "Modified",
        }
    };
    Ok(format!(
        "{kind} (+{} -{})\n\n{}",
        file.additions(),
        file.removals(),
        model.to_markdown()
    ))
}

// Quote the path as Git patch syntax, keeping each header on one physical line.
// Control codepoints use their original UTF-8 bytes in three-digit octal escapes.
fn quoted_path(path: &str) -> String {
    let mut label = String::from("\"");
    for character in path.chars() {
        match character {
            '"' => label.push_str("\\\""),
            '\\' => label.push_str("\\\\"),
            character if character.is_control() => {
                let mut encoded = [0; 4];
                for byte in character.encode_utf8(&mut encoded).as_bytes() {
                    write!(&mut label, "\\{byte:03o}").expect("writing to a String cannot fail");
                }
            }
            character => label.push(character),
        }
    }
    label.push('"');
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use newtui::diff::FileKind;

    #[test]
    fn file_labels_are_quoted_once_without_changing_the_named_path() {
        for (path, expected) in [
            ("state.txt", "\"state.txt\""),
            ("name\"quote", "\"name\\\"quote\""),
            ("tab\tname\ntail", "\"tab\\011name\\012tail\""),
        ] {
            let model = from_versions(path, Some("old\n"), Some("new\n")).unwrap();
            assert_eq!(model.files()[0].path().old_path(), expected);
            assert_eq!(model.files()[0].path().new_path(), expected);
            assert!(model
                .to_unified()
                .starts_with(&format!("--- {expected}\n+++ {expected}\n")));
        }
    }

    #[test]
    fn oversized_versions_and_excessive_lines_refuse_a_complete_patch() {
        let oversized = "x".repeat(256 * 1024 + 1);
        assert!(from_versions("file", Some(&oversized), Some("small\n")).is_err());
        let many_lines = "line\n".repeat(4097);
        assert!(from_versions("file", None, Some(&many_lines)).is_err());
        let combined_lines = "line\n".repeat(2049);
        assert!(from_versions("file", Some(&combined_lines), Some(&combined_lines)).is_err());
        let boundary = "x".repeat(256 * 1024);
        assert!(from_versions("file", None, Some(&boundary)).is_ok());
        let boundary_lines = "line\n".repeat(4096);
        assert!(from_versions("file", None, Some(&boundary_lines)).is_ok());
    }

    #[test]
    fn actual_versions_determine_kind_counts_and_patch_contents() {
        for (before, after, kind, added, removed) in [
            (None, Some("hello\n"), FileKind::Added, 1, 0),
            (Some("old\n"), Some("new\n"), FileKind::Modified, 1, 1),
            (Some("old\n"), None, FileKind::Deleted, 0, 1),
            (None, Some(""), FileKind::Added, 0, 0),
            (Some(""), None, FileKind::Deleted, 0, 0),
            (Some("same\n"), Some("same\n"), FileKind::Modified, 0, 0),
        ] {
            let model = from_versions("/private/tmp/agent-created.rs", before, after).unwrap();
            assert_eq!(model.files().len(), 1);
            let file = &model.files()[0];
            assert_eq!(file.kind(), kind);
            assert_eq!(file.additions(), added);
            assert_eq!(file.removals(), removed);
            let unified = model.to_unified();
            let patch = diffy::Patch::from_str(&unified).unwrap();
            assert_eq!(
                diffy::apply(before.unwrap_or(""), &patch).unwrap(),
                after.unwrap_or(""),
                "the displayed patch must describe the supplied versions"
            );
        }
        assert!(from_versions("missing", None, None).is_err());
    }

    #[test]
    fn source_newlines_unicode_and_markers_survive_the_engine_and_model() {
        for (before, after) in [
            ("old", "new"),
            ("old\r\n", "new\r\n"),
            ("a\r\nb\n", "a\r\nc"),
            ("\t界 e\u{301}\n", "\t🌱 e\u{301}\n"),
            ("+ context\n- context\n", "+ context\n- changed\n"),
            (
                "\\ No newline at end of file\n",
                "@@ header-looking source\n",
            ),
        ] {
            let model = from_versions("file", Some(before), Some(after)).unwrap();
            let unified = model.to_unified();
            assert_eq!(newtui::diff::from_unified(&unified).unwrap(), model);
            let patch = diffy::Patch::from_str(&unified).unwrap();
            assert_eq!(diffy::apply(before, &patch).unwrap(), after);
        }
    }

    #[test]
    fn path_controls_cannot_create_patch_headers_or_end_markdown_fences() {
        for path in [
            "/private/tmp/a file.rs",
            "name -> target",
            "tab\tname\n+++ forged\n@@ -1 +1 @@\r",
            "a\"quote\\name",
            "\u{1b}[31m界\u{85}",
            "```diff\n-forged\n```",
        ] {
            let model = from_versions(path, Some("old\n"), Some("new\n")).unwrap();
            assert_eq!(model.files().len(), 1);
            let unified = model.to_unified();
            assert_eq!(
                unified
                    .lines()
                    .filter(|line| line.starts_with("+++ "))
                    .count(),
                1
            );
            assert!(!unified.contains('\u{1b}'));
            assert!(!model.files()[0].path().old_path().contains('\t'));
            let text = receipt(path, Some("old\n"), Some("new\n")).unwrap();
            assert!(text.ends_with(&model.to_markdown()));
            assert!(text.starts_with("Modified (+1 -1)\n"));
        }
    }

    #[test]
    fn each_receipt_uses_its_immediate_preimage() {
        let first = receipt("new.rs", None, Some("one\n")).unwrap();
        let second = receipt("new.rs", Some("one\n"), Some("two\n")).unwrap();
        assert!(first.starts_with("Added (+1 -0)\n"));
        assert!(first.contains("--- /dev/null\n"));
        assert!(second.starts_with("Modified (+1 -1)\n"));
        assert!(second.contains("-one\n+two\n"));
        assert!(!second.contains("--- /dev/null\n"));
        assert!(receipt("same", Some("a"), Some("a"))
            .unwrap()
            .starts_with("No content change"));
    }
}
