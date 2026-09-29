//! Rust-only tree-sitter outline for aged `read_file` results (#2557).
//!
//! Compiled in only behind the `ast` feature (vendors C via `tree-sitter` +
//! `tree-sitter-rust`, off by default). `prune::one_line_summary` calls
//! [`outline_rust`] instead of the plain one-liner when it can, and falls
//! back to the one-liner otherwise — never to a regex approximation.

use std::sync::OnceLock;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

fn config() -> &'static TagsConfiguration {
    static CONFIG: OnceLock<TagsConfiguration> = OnceLock::new();
    CONFIG.get_or_init(|| {
        TagsConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(),
            tree_sitter_rust::TAGS_QUERY,
            "",
        )
        .expect("tree-sitter-rust's bundled tags.scm must compile")
    })
}

/// One definition found in a Rust source fragment: absolute 1-based,
/// inclusive line span in the ORIGINAL file (`first_line`-shifted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    pub start_line: usize,
    pub end_line: usize,
    /// The definition's opening line, trimmed of its trailing `{`/`;`.
    pub header: String,
}

/// Outline the definitions in `source`, a Rust source fragment whose first
/// line is `first_line` (1-based) in the original file. `None` when the
/// fragment yields no tags at all (e.g. the tags query can't be built) — a
/// caller falls back to the plain one-liner, never to a regex parser.
pub fn outline_rust(source: &str, first_line: usize) -> Option<Vec<OutlineEntry>> {
    let mut ctx = TagsContext::new();
    let (tags, _) = ctx.generate_tags(config(), source.as_bytes(), None).ok()?;
    // `Tag::span` is the SPAN OF THE NAME NODE ONLY (tags.rs computes it from
    // `name_node.start_position()..end_position()`), not the definition —
    // the caller wants the definition's full line range, so it comes from
    // `Tag::range` (the tagged node's byte range) via a newline count instead.
    let line_of = |byte: usize| source[..byte.min(source.len())].matches('\n').count();
    let mut entries: Vec<OutlineEntry> = tags
        .filter_map(Result::ok)
        .filter(|t| t.is_definition)
        .map(|t| {
            let header = source
                .get(t.range.clone())
                .and_then(|s| s.lines().next())
                .unwrap_or_default()
                .trim_end_matches(['{', ';'])
                .trim()
                .to_string();
            OutlineEntry {
                start_line: first_line + line_of(t.range.start),
                end_line: first_line + line_of(t.range.end.saturating_sub(1)),
                header,
            }
        })
        .collect();
    entries.sort_by_key(|e| e.start_line);
    entries.dedup();
    Some(entries)
}

/// Render entries as the model-facing outline body: one indented
/// `span\theader` line per definition, spans as `N` or `N-M`.
pub fn render_outline(entries: &[OutlineEntry]) -> String {
    entries
        .iter()
        .map(|e| {
            let span = if e.start_line == e.end_line {
                e.start_line.to_string()
            } else {
                format!("{}-{}", e.start_line, e.end_line)
            };
            format!("    {span}\t{}", e.header)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
mod attempt_capture;

fn compact_responses_input(x: u32) -> u32 {
    x + 1
}

pub(crate) enum ResponsesCompaction {
    Kept,
    Dropped,
}
";

    #[test]
    fn spans_match_the_source_lines() {
        let entries = outline_rust(SRC, 1).expect("valid rust");
        let mod_entry = entries
            .iter()
            .find(|e| e.header.contains("attempt_capture"))
            .expect("mod definition found");
        assert_eq!((mod_entry.start_line, mod_entry.end_line), (1, 1));
        assert_eq!(
            mod_entry.header,
            "mod attempt_capture;".trim_end_matches(';')
        );

        let fn_entry = entries
            .iter()
            .find(|e| e.header.starts_with("fn compact_responses_input"))
            .expect("fn definition found");
        assert_eq!((fn_entry.start_line, fn_entry.end_line), (3, 5));

        let enum_entry = entries
            .iter()
            .find(|e| e.header.contains("ResponsesCompaction"))
            .expect("enum definition found");
        assert_eq!((enum_entry.start_line, enum_entry.end_line), (7, 10));
    }

    #[test]
    fn absolute_lines_shift_by_first_line() {
        // Same fragment, read starting at line 100 of the real file (an
        // `offset=100` read_file call) — spans shift by the same amount.
        let entries = outline_rust(SRC, 100).expect("valid rust");
        let fn_entry = entries
            .iter()
            .find(|e| e.header.starts_with("fn compact_responses_input"))
            .expect("fn definition found");
        assert_eq!((fn_entry.start_line, fn_entry.end_line), (102, 104));
    }

    #[test]
    fn render_formats_ranges_and_points() {
        let entries = vec![
            OutlineEntry {
                start_line: 16,
                end_line: 16,
                header: "mod attempt_capture".to_string(),
            },
            OutlineEntry {
                start_line: 405,
                end_line: 750,
                header: "fn compact_responses_input(…)".to_string(),
            },
        ];
        let rendered = render_outline(&entries);
        assert_eq!(
            rendered,
            "    16\tmod attempt_capture\n    405-750\tfn compact_responses_input(…)"
        );
    }
}
