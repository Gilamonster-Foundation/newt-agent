//! Parse the model's raw reply into a structured `Emission`.
//!
//! Three shapes (matching the failure-mode taxonomy in
//! `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`):
//!
//! - [`Emission::WholeFiles`] — one or more `FILE: <path>\n…\nEND-FILE`
//!   blocks. The S5 directive's preferred shape.
//! - [`Emission::UnifiedDiff`] — a unified diff (fenced or not). Legacy
//!   path; useful when a model ignores the directive but still lands a
//!   valid hunk.
//! - [`Emission::Prose`] — nothing structured was found. The model
//!   emitted text only. Failure mode T0a in the taxonomy.
//!
//! The parser is **permissive**. Real Ollama replies often arrive
//! wrapped in stray ` ``` ` fences or with a leading "Here's the
//! updated file:" preamble — `strip_outer_fences` peels a single
//! enclosing fence and `try_parse_whole_files` is tolerant of
//! interleaved blank lines. We'd rather extract a valid emission
//! from a slightly-malformed reply than crash on it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Emission {
    /// Map of relative-path -> full file contents.
    WholeFiles(BTreeMap<String, String>),
    /// Raw unified-diff text.
    UnifiedDiff(String),
    /// Plain prose; no structured emission detected.
    Prose(String),
}

impl Emission {
    /// Wire-stable label used in `TaskReply.emission_shape`. Sourced
    /// from `plugins_protocol::emission_shape` so producers and
    /// consumers can't drift.
    pub fn shape_label(&self) -> &'static str {
        match self {
            Self::WholeFiles(_) => plugins_protocol::emission_shape::WHOLE_FILES,
            Self::UnifiedDiff(_) => plugins_protocol::emission_shape::UNIFIED_DIFF,
            Self::Prose(_) => plugins_protocol::emission_shape::PROSE,
        }
    }
}

/// Parse `raw` into an [`Emission`]. Permissive: peels a single
/// enclosing ` ``` ` fence before deciding, then tries each shape in
/// preference order (whole-files > unified-diff > prose).
pub fn normalize_emission(raw: &str) -> Result<Emission> {
    let stripped = strip_outer_fences(raw);

    if let Some(files) = try_parse_whole_files(&stripped) {
        if !files.is_empty() {
            return Ok(Emission::WholeFiles(files));
        }
    }

    if let Some(diff) = try_parse_unified_diff(&stripped) {
        return Ok(Emission::UnifiedDiff(diff));
    }

    Ok(Emission::Prose(stripped))
}

/// If the entire reply is wrapped in a single ` ``` ` block (with or
/// without a language tag), peel it. Otherwise return `raw` trimmed.
fn strip_outer_fences(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        // Skip an optional language tag on the same line.
        let after_tag = match rest.find('\n') {
            Some(nl) => &rest[nl + 1..],
            None => rest,
        };
        let body = after_tag
            .strip_suffix("```")
            .or_else(|| after_tag.strip_suffix("```\n"))
            .unwrap_or(after_tag);
        return body.trim_end_matches('\n').to_string();
    }
    trimmed.to_string()
}

/// Try to parse one or more `FILE: <path>\n…\nEND-FILE` blocks.
/// Returns `None` if no `FILE:` header is found at all; otherwise
/// returns whatever it could extract (last block is allowed to omit
/// `END-FILE` — some models miss the trailing marker).
///
/// Defends against the model *restating* the `FILE:` marker as the
/// first line of a block's body (e.g. the directive's `FILE: <path>`
/// header followed immediately by `FILE: <path>` again, then the real
/// contents — frequently after `strip_outer_fences` peels a code
/// fence). Such a leaked marker, if treated as content, would write a
/// file whose first line is `FILE: …`, poisoning the apply step. We
/// skip at most one leaked marker per block: a `FILE:` line that
/// arrives while the current block's body is still empty (modulo a
/// single blank line) re-targets the same/new path instead of being
/// appended as content.
fn try_parse_whole_files(body: &str) -> Option<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    let mut cur_path: Option<String> = None;
    let mut cur_buf = String::new();
    let mut saw_header = false;
    // True until the current block has accumulated real content. While
    // true, a `FILE:` line is a leaked-marker restatement, not content.
    let mut block_body_empty = true;

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("FILE: ") {
            saw_header = true;
            // If we are still at the very start of the current block
            // (no real content yet), this `FILE:` line is a leaked
            // restatement of the marker. Re-target the path and drop
            // any held-back blank instead of flushing an empty file.
            if cur_path.is_some() && block_body_empty {
                cur_buf.clear();
                cur_path = Some(rest.trim().to_string());
                block_body_empty = true;
                continue;
            }
            if let Some(path) = cur_path.take() {
                files.insert(path, cur_buf.trim_end_matches('\n').to_string());
                cur_buf.clear();
            }
            cur_path = Some(rest.trim().to_string());
            block_body_empty = true;
            continue;
        }
        if line.trim() == "END-FILE" {
            if let Some(path) = cur_path.take() {
                files.insert(path, cur_buf.trim_end_matches('\n').to_string());
                cur_buf.clear();
            }
            block_body_empty = true;
            continue;
        }
        if cur_path.is_some() {
            // A single leading blank line does not count as real
            // content yet — it may precede a leaked marker.
            if !(block_body_empty && line.trim().is_empty()) {
                block_body_empty = false;
            }
            cur_buf.push_str(line);
            cur_buf.push('\n');
        }
    }

    if let Some(path) = cur_path {
        files.insert(path, cur_buf.trim_end_matches('\n').to_string());
    }

    if !saw_header {
        return None;
    }
    Some(files)
}

/// Detect a unified diff by header pattern. A real diff has the
/// `--- ` / `+++ ` header pair and at least one `@@ ` hunk header.
fn try_parse_unified_diff(body: &str) -> Option<String> {
    let has_minus = body.starts_with("--- ") || body.contains("\n--- ");
    let has_plus = body.contains("\n+++ ");
    let has_hunk = body.contains("\n@@ ") || body.contains("@@ -");
    if has_minus && has_plus && has_hunk {
        Some(body.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_whole_file_block() {
        let raw = "FILE: src/lib.rs\npub fn hello() {}\nEND-FILE\n";
        let em = normalize_emission(raw).unwrap();
        match em {
            Emission::WholeFiles(files) => {
                assert_eq!(files.len(), 1);
                assert_eq!(files.get("src/lib.rs").unwrap(), "pub fn hello() {}");
            }
            other => panic!("expected WholeFiles, got {other:?}"),
        }
    }

    #[test]
    fn parses_multi_file_whole_file_block() {
        let raw = "\
FILE: a.rs
pub fn a() {}
END-FILE

FILE: b.rs
pub fn b() {}
END-FILE
";
        let em = normalize_emission(raw).unwrap();
        match em {
            Emission::WholeFiles(files) => {
                assert_eq!(files.len(), 2);
                assert_eq!(files.get("a.rs").unwrap(), "pub fn a() {}");
                assert_eq!(files.get("b.rs").unwrap(), "pub fn b() {}");
            }
            other => panic!("expected WholeFiles, got {other:?}"),
        }
    }

    #[test]
    fn handles_outer_code_fence_around_whole_files() {
        let raw = "```\nFILE: a.rs\npub fn x() {}\nEND-FILE\n```";
        let em = normalize_emission(raw).unwrap();
        if let Emission::WholeFiles(files) = em {
            assert_eq!(files.get("a.rs").unwrap(), "pub fn x() {}");
        } else {
            panic!("expected whole files");
        }
    }

    #[test]
    fn handles_outer_code_fence_with_language_tag() {
        let raw = "```rust\nFILE: a.rs\npub fn x() {}\nEND-FILE\n```";
        let em = normalize_emission(raw).unwrap();
        if let Emission::WholeFiles(files) = em {
            assert_eq!(files.get("a.rs").unwrap(), "pub fn x() {}");
        } else {
            panic!("expected whole files");
        }
    }

    #[test]
    fn tolerates_missing_trailing_end_file() {
        // Some models drop the trailing END-FILE marker.
        let raw = "FILE: src/lib.rs\npub fn hello() {}\n";
        let em = normalize_emission(raw).unwrap();
        if let Emission::WholeFiles(files) = em {
            assert_eq!(files.get("src/lib.rs").unwrap(), "pub fn hello() {}");
        } else {
            panic!("expected whole files");
        }
    }

    #[test]
    fn parses_unified_diff_when_no_whole_files() {
        let raw = "\
--- a/foo.rs
+++ b/foo.rs
@@ -1 +1 @@
-old
+new
";
        let em = normalize_emission(raw).unwrap();
        assert!(matches!(em, Emission::UnifiedDiff(_)));
    }

    #[test]
    fn falls_back_to_prose_on_plain_text() {
        let raw = "I've updated the file successfully.";
        let em = normalize_emission(raw).unwrap();
        assert!(matches!(em, Emission::Prose(_)));
    }

    #[test]
    fn shape_labels_match_wire_constants() {
        let whole = Emission::WholeFiles(BTreeMap::new());
        assert_eq!(whole.shape_label(), "whole_files");
        let diff = Emission::UnifiedDiff(String::new());
        assert_eq!(diff.shape_label(), "unified_diff");
        let prose = Emission::Prose(String::new());
        assert_eq!(prose.shape_label(), "prose");
    }

    #[test]
    fn empty_input_is_prose() {
        let em = normalize_emission("").unwrap();
        match em {
            Emission::Prose(s) => assert!(s.is_empty()),
            other => panic!("expected empty prose, got {other:?}"),
        }
    }

    #[test]
    fn whole_files_preferred_over_diff_when_both_present() {
        // A model that emits a FILE: block AND a stray diff fragment
        // should be classified as whole-files (the directive wins).
        let raw = "\
FILE: src/lib.rs
pub fn hello() {}
END-FILE
--- a/foo
+++ b/foo
@@ -1 +1 @@
-x
+y
";
        let em = normalize_emission(raw).unwrap();
        assert!(matches!(em, Emission::WholeFiles(_)));
    }

    #[test]
    fn strips_leaked_file_marker_restated_in_body() {
        // Failures 3 & 4: the model restates the `FILE:` marker as the
        // first body line (commonly inside a fence that gets peeled).
        // The marker must NOT leak into the file contents.
        let raw = "FILE: src/lib.rs\nFILE: src/lib.rs\npub fn add(a: i32, b: i32) -> i32 { a + b }\nEND-FILE\n";
        let em = normalize_emission(raw).unwrap();
        match em {
            Emission::WholeFiles(files) => {
                assert_eq!(files.len(), 1);
                assert_eq!(
                    files.get("src/lib.rs").unwrap(),
                    "pub fn add(a: i32, b: i32) -> i32 { a + b }"
                );
            }
            other => panic!("expected WholeFiles, got {other:?}"),
        }
    }

    #[test]
    fn strips_leaked_marker_inside_peeled_fence() {
        // Whole reply wrapped in a fence; after peeling, the body opens
        // with a leaked `FILE:` restatement.
        let raw = "```rust\nFILE: src/lib.rs\nFILE: src/lib.rs\npub fn a() {}\n```";
        let em = normalize_emission(raw).unwrap();
        if let Emission::WholeFiles(files) = em {
            assert_eq!(files.get("src/lib.rs").unwrap(), "pub fn a() {}");
        } else {
            panic!("expected whole files");
        }
    }

    #[test]
    fn strips_leaked_marker_after_leading_blank() {
        let raw = "FILE: src/lib.rs\n\nFILE: src/lib.rs\npub fn a() {}\nEND-FILE\n";
        let em = normalize_emission(raw).unwrap();
        if let Emission::WholeFiles(files) = em {
            assert_eq!(files.get("src/lib.rs").unwrap(), "pub fn a() {}");
        } else {
            panic!("expected whole files");
        }
    }

    #[test]
    fn does_not_strip_second_file_block_as_leaked_marker() {
        // Two genuinely distinct files, each with real content, must
        // both survive — the leaked-marker skip only fires while a
        // block's body is still empty.
        let raw = "\
FILE: a.rs
pub fn a() {}
FILE: b.rs
pub fn b() {}
";
        let em = normalize_emission(raw).unwrap();
        match em {
            Emission::WholeFiles(files) => {
                assert_eq!(files.len(), 2);
                assert_eq!(files.get("a.rs").unwrap(), "pub fn a() {}");
                assert_eq!(files.get("b.rs").unwrap(), "pub fn b() {}");
            }
            other => panic!("expected two WholeFiles, got {other:?}"),
        }
    }

    #[test]
    fn parsed_leaked_marker_body_is_applyable() {
        // End-to-end: a leaked-marker reply parses to clean contents
        // whose first line is real code, so the writer's shape guards
        // accept it.
        let raw = "FILE: src/lib.rs\nFILE: src/lib.rs\npub fn add() {}\nEND-FILE\n";
        let em = normalize_emission(raw).unwrap();
        if let Emission::WholeFiles(files) = em {
            let contents = files.get("src/lib.rs").unwrap();
            let first = contents.lines().find(|l| !l.trim().is_empty()).unwrap();
            assert!(
                !first.trim_start().starts_with("FILE:"),
                "marker leaked: {first}"
            );
        } else {
            panic!("expected whole files");
        }
    }
}
