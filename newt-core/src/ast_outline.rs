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
    /// True when this definition's real end lies past what was parsed: the
    /// page cut it mid-body, leaving an unbalanced brace count that folds
    /// the rest into one tree-sitter `ERROR` node with no `is_definition`
    /// tag of its own (#2638 fix item 1). `end_line` is then just the last
    /// line seen, not the real end.
    pub open_ended: bool,
}

/// Item-start keywords a top-level Rust definition can begin with, once any
/// `pub`/`async`/`unsafe` modifier prefix is stripped — config, not logic,
/// so a new item kind is one array entry (#2638).
const ITEM_KEYWORDS: &[&str] = &[
    "fn", "struct", "enum", "impl", "trait", "mod", "type", "static", "const",
];
const VISIBILITY_PREFIXES: &[&str] = &["pub(crate)", "pub(super)", "pub(self)", "pub"];
const MODIFIER_PREFIXES: &[&str] = &["async", "unsafe"];

/// Does `line` (a single source line, already trimmed of leading fragment
/// noise) look like the start of a top-level item, once its `pub`/`async`/
/// `unsafe` modifiers are peeled off?
fn looks_like_definition_start(line: &str) -> bool {
    let mut rest = line.trim_start();
    loop {
        let before = rest;
        for prefix in VISIBILITY_PREFIXES.iter().chain(MODIFIER_PREFIXES) {
            if let Some(r) = rest.strip_prefix(prefix) {
                rest = r.trim_start();
                break;
            }
        }
        if rest == before {
            break;
        }
    }
    ITEM_KEYWORDS.iter().any(|kw| {
        rest.strip_prefix(kw)
            .is_some_and(|after| after.starts_with([' ', '(', '!', '<']))
    })
}

/// The mod name in `header` (e.g. `"pub(crate) mod foo"` -> `"foo"`), or
/// `None` when `header` isn't a `mod` declaration — used to collapse runs
/// of consecutive `mod` lines (#2638 fix item 3).
fn mod_name(header: &str) -> Option<&str> {
    let mut rest = header.trim_start();
    loop {
        let before = rest;
        for prefix in VISIBILITY_PREFIXES {
            if let Some(r) = rest.strip_prefix(prefix) {
                rest = r.trim_start();
                break;
            }
        }
        if rest == before {
            break;
        }
    }
    rest.strip_prefix("mod ").map(str::trim)
}

/// #2638 fix item 2: a page whose first line lands mid-body (mid-function,
/// mid-block) confuses tree-sitter's error recovery into folding the
/// leading fragment AND the next real definition into one `ERROR` node,
/// silently dropping that definition (`cap_exit_progress_handoff` was
/// missed this way at an `offset=6000` page). Skip forward to the first
/// line that looks like a top-level item start before parsing at all —
/// comments/blank lines dropped this way carry no tag anyway, so a
/// well-aligned page (first line already a real item) is unaffected; a
/// genuinely mid-body page loses only its unparseable leading fragment.
fn skip_to_first_definition_start(source: &str, first_line: usize) -> (&str, usize) {
    let mut byte = 0;
    for (i, line) in source.lines().enumerate() {
        if looks_like_definition_start(line) {
            return (&source[byte..], first_line + i);
        }
        byte += line.len() + 1; // `lines()` strips the newline it splits on
    }
    (source, first_line)
}

/// #2638 fix item 1: the page-cut counterpart of
/// [`skip_to_first_definition_start`] — a definition whose CLOSE never
/// arrives inside this fragment (page cut mid-body) leaves an unbalanced
/// brace count that, once deep enough, makes tree-sitter give up on
/// recovering ANY valid structure and fold the WHOLE parse (including
/// earlier, complete siblings the tags query already found) into one
/// `ERROR` — so walking the raw tree for a trailing `ERROR` node can't tell
/// "the cut definition" from "everything before it" apart. Scanning
/// `source`'s own lines instead sidesteps the tree entirely: any line that
/// looks like a definition start but isn't already one of `entries`' own
/// start lines is one tags recovery lost, and the last such line in the
/// fragment is the one the page actually cut.
fn trailing_truncated_definition(
    source: &str,
    first_line: usize,
    entries: &[OutlineEntry],
) -> Option<OutlineEntry> {
    // Only the FIRST untagged definition-start after the last real entry is
    // the one the page cut — a local `const`/`static` inside that cut
    // definition's own body can also match `looks_like_definition_start`
    // (it's the same grammar shape at statement position), and one of
    // those sitting later in the fragment must not shadow the real gap.
    let tagged: std::collections::HashSet<usize> = entries.iter().map(|e| e.start_line).collect();
    let total_lines = source.lines().count();
    let last_line = first_line + total_lines.saturating_sub(1);
    let after = entries.last().map_or(first_line, |e| e.end_line + 1);
    source
        .lines()
        .enumerate()
        .map(|(i, line)| (first_line + i, line))
        .find(|(abs_line, line)| {
            *abs_line >= after && looks_like_definition_start(line) && !tagged.contains(abs_line)
        })
        .map(|(start_line, line)| OutlineEntry {
            start_line,
            end_line: last_line,
            header: line.trim().trim_end_matches(['{', ';']).trim().to_string(),
            open_ended: true,
        })
}

/// Outline the definitions in `source`, a Rust source fragment whose first
/// line is `first_line` (1-based) in the original file. `None` when the
/// fragment yields no tags at all (e.g. the tags query can't be built) — a
/// caller falls back to the plain one-liner, never to a regex parser.
pub fn outline_rust(source: &str, first_line: usize) -> Option<Vec<OutlineEntry>> {
    let (source, first_line) = skip_to_first_definition_start(source, first_line);
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
                open_ended: false,
            }
        })
        .collect();
    entries.sort_by_key(|e| e.start_line);
    entries.dedup();
    if let Some(entry) = trailing_truncated_definition(source, first_line, &entries) {
        entries.push(entry);
        entries.sort_by_key(|e| e.start_line);
    }
    Some(entries)
}

/// A `mod` run collapses once it is at least this long (#2638 fix item 3).
const MOD_RUN_MIN: usize = 2;
/// Names shown before a collapsed `mod` run is truncated with `…`.
const MOD_NAME_CAP: usize = 6;

/// Render entries as the model-facing outline body: one indented
/// `span\theader` line per definition, spans as `N`, `N-M`, or `N-…` for an
/// [`OutlineEntry::open_ended`] one. A run of `MOD_RUN_MIN`+ consecutive
/// `mod` declarations collapses to one `<first>-<last> mod ×N (a, b, …)`
/// line (#2638 fix item 3) so ~60 boilerplate `mod x;` lines don't drown
/// out the page's real definitions.
pub fn render_outline(entries: &[OutlineEntry]) -> String {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        if let Some(first_name) = mod_name(&entries[i].header) {
            let mut names = vec![first_name];
            let mut j = i + 1;
            while j < entries.len() {
                let Some(name) = mod_name(&entries[j].header) else {
                    break;
                };
                names.push(name);
                j += 1;
            }
            if names.len() >= MOD_RUN_MIN {
                let first = entries[i].start_line;
                let last = entries[j - 1].start_line;
                let shown = names
                    .iter()
                    .take(MOD_NAME_CAP)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = if names.len() > MOD_NAME_CAP {
                    ", …"
                } else {
                    ""
                };
                lines.push(format!(
                    "    {first}-{last}\tmod ×{} ({shown}{more})",
                    names.len()
                ));
                i = j;
                continue;
            }
        }
        let e = &entries[i];
        let span = if e.open_ended {
            format!("{}-…", e.start_line)
        } else if e.start_line == e.end_line {
            e.start_line.to_string()
        } else {
            format!("{}-{}", e.start_line, e.end_line)
        };
        lines.push(format!("    {span}\t{}", e.header));
        i += 1;
    }
    lines.join("\n")
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
                open_ended: false,
            },
            OutlineEntry {
                start_line: 405,
                end_line: 750,
                header: "fn compact_responses_input(…)".to_string(),
                open_ended: false,
            },
        ];
        let rendered = render_outline(&entries);
        assert_eq!(
            rendered,
            "    16\tmod attempt_capture\n    405-750\tfn compact_responses_input(…)"
        );
    }

    #[test]
    fn render_formats_an_open_ended_entry() {
        let entries = vec![OutlineEntry {
            start_line: 6579,
            end_line: 6768,
            header: "async fn openai_chat_complete_with_prompt_and_artifacts(".to_string(),
            open_ended: true,
        }];
        assert_eq!(
            render_outline(&entries),
            "    6579-…\tasync fn openai_chat_complete_with_prompt_and_artifacts("
        );
    }

    /// #2638 fix item 1, red before the fix: a definition whose closing
    /// brace never arrives in this fragment (the page cut it) must still
    /// appear, open-ended, rather than being dropped entirely — tags-based
    /// recovery folds it into one `ERROR` node with no `is_definition` tag.
    #[test]
    fn a_page_cut_mid_definition_still_yields_an_open_ended_entry() {
        let src = "fn foo() {\n    let x = 1;\n    if x {\n        do_thing();\n";
        let entries = outline_rust(src, 1).expect("must still report something");
        let entry = entries
            .iter()
            .find(|e| e.header.contains("fn foo"))
            .expect("the cut definition must not be dropped");
        assert!(entry.open_ended, "a page-cut definition must be open-ended");
        assert_eq!(entry.start_line, 1);
    }

    /// #2638 fix item 2, red before the fix: a page whose first line is a
    /// mid-body fragment (no leading definition) must not swallow the NEXT
    /// real definition via tree-sitter error recovery.
    #[test]
    fn a_mid_body_page_start_still_tags_the_next_definition() {
        let src =
            "    do_thing();\n    another();\n}\n\nfn after_the_cut(y: u32) -> u32 {\n    y\n}\n";
        let entries = outline_rust(src, 6000).expect("valid rust after the fragment");
        let entry = entries
            .iter()
            .find(|e| e.header.starts_with("fn after_the_cut"))
            .expect("the fn right after the mid-body start must still be tagged");
        assert!(!entry.open_ended);
    }

    /// #2638 fix item 3, red before the fix: a run of consecutive `mod`
    /// declarations renders as one collapsed line, not N boilerplate ones.
    #[test]
    fn a_mod_run_collapses_into_one_entry() {
        let src: String = (0..10).map(|i| format!("mod m{i:02};\n")).collect();
        let entries = outline_rust(&src, 1).expect("valid rust");
        assert_eq!(entries.len(), 10, "each mod is still its own tagged entry");
        let rendered = render_outline(&entries);
        assert_eq!(
            rendered.lines().count(),
            1,
            "the run must collapse to one line"
        );
        assert!(rendered.contains("1-10\tmod ×10"), "got: {rendered}");
        assert!(
            rendered.contains('…'),
            "names past the cap are truncated: {rendered}"
        );
    }
}
