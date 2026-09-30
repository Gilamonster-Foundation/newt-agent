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

/// Byte ranges of every line comment, block comment (nestable), string, and
/// raw string in `source`, found by a dedicated lexical scan — #2638 fix
/// item 2. This deliberately does NOT reuse the tree-sitter parse: an
/// unparseable tail (a page cut mid-body) folds into one un-tokenized
/// `ERROR` node that drops the comment/string token's identity entirely
/// (confirmed by inspecting the tree directly — a `let _ = r#"..` fragment
/// with no valid enclosing item produces `(ERROR (ERROR (identifier))
/// (identifier))`, no `raw_string_literal` node at all), so the parse tree
/// cannot be trusted to report every comment/string span under exactly the
/// truncated input this exists to guard. An unterminated block comment or
/// raw string extends to EOF, matching the real scanner's own behavior.
fn comment_or_string_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    let b = source.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                let start = i;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                ranges.push(start..i);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                let mut depth = 1usize;
                while i < b.len() && depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                ranges.push(start..i);
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' && i + 1 < b.len() {
                        2
                    } else {
                        1
                    };
                }
                i = (i + 1).min(b.len());
                ranges.push(start..i);
            }
            b'r' if matches!(b.get(i + 1), Some(b'"') | Some(b'#')) => {
                let start = i;
                let mut j = i + 1;
                let mut hashes = 0usize;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) != Some(&b'"') {
                    i += 1; // an identifier starting with `r`, not a raw string
                    continue;
                }
                j += 1;
                loop {
                    if j >= b.len() {
                        break;
                    }
                    if b[j] == b'"' {
                        let mut k = j + 1;
                        let mut closing = 0usize;
                        while closing < hashes && b.get(k) == Some(&b'#') {
                            closing += 1;
                            k += 1;
                        }
                        if closing == hashes {
                            j = k;
                            break;
                        }
                    }
                    j += 1;
                }
                ranges.push(start..j.min(b.len()));
                i = j.min(b.len());
            }
            _ => i += 1,
        }
    }
    ranges
}

/// The page-cut counterpart of tags-based recovery (#2638 fix item 1): a
/// definition whose CLOSE never arrives inside this fragment (page cut
/// mid-body) leaves an unbalanced brace count that, once deep enough, makes
/// tree-sitter give up on recovering ANY valid structure and fold the WHOLE
/// parse (including earlier, complete siblings the tags query already
/// found) into one `ERROR` — so walking the raw tree for a trailing `ERROR`
/// node can't tell "the cut definition" from "everything before it" apart.
/// Scanning `source`'s own lines instead sidesteps the tree entirely: any
/// line that looks like a definition start but isn't already one of
/// `entries`' own start lines is one tags recovery lost, and the last such
/// line in the fragment is the one the page actually cut — UNLESS that line
/// sits inside a comment or string, per an actual parse (#2638 fix item 2):
/// text there was never a definition, tagged or not.
fn trailing_truncated_definition(
    source: &str,
    first_line: usize,
    entries: &[OutlineEntry],
) -> Option<OutlineEntry> {
    // Only the FIRST untagged, non-lexical definition-start after the last
    // real entry is the one the page cut — a local `const`/`static` inside
    // that cut definition's own body can also match
    // `looks_like_definition_start` (it's the same grammar shape at
    // statement position), and one of those sitting later in the fragment
    // must not shadow the real gap.
    let tagged: std::collections::HashSet<usize> = entries.iter().map(|e| e.start_line).collect();
    let excluded = comment_or_string_ranges(source);
    let total_lines = source.lines().count();
    let last_line = first_line + total_lines.saturating_sub(1);
    let after = entries.last().map_or(first_line, |e| e.end_line + 1);
    let mut byte = 0;
    for (i, line) in source.split_inclusive('\n').enumerate() {
        let abs_line = first_line + i;
        let start_byte = byte;
        byte += line.len();
        if abs_line < after || tagged.contains(&abs_line) {
            continue;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if !looks_like_definition_start(trimmed) {
            continue;
        }
        if excluded.iter().any(|r| r.contains(&start_byte)) {
            continue;
        }
        return Some(OutlineEntry {
            start_line: abs_line,
            end_line: last_line,
            header: trimmed
                .trim()
                .trim_end_matches(['{', ';'])
                .trim()
                .to_string(),
            open_ended: true,
        });
    }
    None
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
/// [`OutlineEntry::open_ended`] one. A run of `MOD_RUN_MIN`+ consecutive,
/// COMPLETE `mod` declarations collapses to one `<first>-<last> mod ×N (a,
/// b, …)` line (#2638 fix item 3) so ~60 boilerplate `mod x;` lines don't
/// drown out the page's real definitions — an `open_ended` mod (the page cut
/// it) never joins a run, so its incomplete-extent marker is never hidden
/// behind a closed-looking collapsed span.
pub fn render_outline(entries: &[OutlineEntry]) -> String {
    let mut lines = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        if !entries[i].open_ended {
            if let Some(first_name) = mod_name(&entries[i].header) {
                let mut names = vec![first_name];
                let mut j = i + 1;
                while j < entries.len() && !entries[j].open_ended {
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

    /// #2638 fix item 2: a page whose first line is a mid-body fragment (no
    /// leading definition) must not swallow the NEXT real definition via
    /// tree-sitter error recovery. This is a green regression pin, not an
    /// isolating red — no lexical preprocessing is applied any more (the
    /// review found none was needed: mutating it away left this exact
    /// fragment still green, because `tree-sitter-tags`' own error recovery
    /// already tags the next definition on a short fragment). Kept so a
    /// real regression here still fails a test.
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

    /// #2638 review round 7, P1: CRLF line endings, once skipped by
    /// [`skip_to_first_definition_start`]'s `line.len() + 1` byte count (that
    /// undercounted by one byte per CRLF line, since `lines()` strips both
    /// `\r` and `\n`), could walk the byte cursor into the middle of a
    /// multibyte character and panic on `&source[byte..]`. That function is
    /// gone now, but [`trailing_truncated_definition`]'s own byte-tracked
    /// scan carries the same hazard, so this pins it directly: a CRLF file
    /// with a multibyte comment must not panic, and the real definition's
    /// span must land on the correct (LF-normalized-line) boundaries.
    #[test]
    fn crlf_and_multibyte_input_does_not_panic_and_spans_correctly() {
        let src = "\r\n\r\n//\u{e9}\r\nfn f() {}\r\n";
        let entries = outline_rust(src, 1).expect("valid rust despite CRLF");
        let entry = entries
            .iter()
            .find(|e| e.header.starts_with("fn f"))
            .expect("fn f must still be tagged");
        assert_eq!((entry.start_line, entry.end_line), (4, 4));
    }

    /// #2638 review round 7, P2: text inside a comment must never be
    /// reported as a definition, tagged or recovered.
    #[test]
    fn a_definition_inside_a_closed_comment_is_not_invented() {
        let src = "/*\nfn imaginary() {}\n*/\nfn real() {}\n";
        let entries = outline_rust(src, 1).expect("valid rust");
        assert!(
            !entries.iter().any(|e| e.header.contains("imaginary")),
            "a commented-out fn must not appear: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.header.contains("real")),
            "the real fn must still appear: {entries:?}"
        );
    }

    /// #2638 review round 7, P2: a page cut mid-trailing-block-comment, whose
    /// unterminated text happens to contain `fn x(`, must not surface an
    /// open-ended entry — there is no definition there, cut or otherwise.
    #[test]
    fn a_page_cut_trailing_block_comment_yields_no_open_ended_entry() {
        let src = "fn real() {}\n\n/*\nfn x(\n";
        let entries = outline_rust(src, 1).expect("valid rust prefix");
        assert!(
            !entries.iter().any(|e| e.open_ended),
            "a comment fragment must never become an open-ended entry: {entries:?}"
        );
    }

    /// #2638 review round 7, P2: same hazard for a page cut mid-raw-string —
    /// text that merely LOOKS like `fn x(` at line-start, entirely inside an
    /// unterminated raw string, must not be reported at all (not even under
    /// the enclosing `let` line, which is not itself a top-level item and so
    /// tags nothing).
    #[test]
    fn a_page_cut_trailing_raw_string_yields_no_open_ended_entry() {
        let src = "fn real() {}\n\nlet _ = r#\"\nfn x(\n";
        let entries = outline_rust(src, 1).expect("valid rust prefix");
        assert!(
            !entries.iter().any(|e| e.header.contains("fn x")),
            "text inside an unterminated raw string must not be reported as a definition: {entries:?}"
        );
    }

    /// #2638 review round 7, P2 negative control (page-cut positive):
    /// unrelated to comments/strings, a genuine page-cut mid-function must
    /// still surface open-ended — the exclusion added for comments/strings
    /// must not blanket-suppress every trailing recovery.
    #[test]
    fn a_genuine_page_cut_mid_body_still_yields_an_open_ended_entry() {
        let src = "fn real() {}\n\nfn cut_here() {\n    do_thing();\n";
        let entries = outline_rust(src, 1).expect("valid rust prefix");
        let entry = entries
            .iter()
            .find(|e| e.header.contains("cut_here"))
            .expect("the cut definition must still be reported");
        assert!(entry.open_ended);
    }

    /// #2638 review round 7, P2 regression: the approved page-cut positive at
    /// real scale — a 2,300-line fn cut by the page boundary — must still be
    /// reported open-ended (this is the shape of the real
    /// `openai_chat_complete_with_prompt_and_artifacts` cut at line 6579).
    #[test]
    fn a_large_page_cut_definition_is_still_open_ended() {
        let mut src = String::from("fn huge() {\n");
        for i in 0..2300 {
            src.push_str(&format!("    let x{i} = {i};\n"));
        }
        let entries = outline_rust(&src, 6579).expect("valid rust prefix");
        let entry = entries
            .iter()
            .find(|e| e.header.contains("fn huge"))
            .expect("the large cut definition must not be dropped");
        assert!(entry.open_ended);
        assert_eq!(entry.start_line, 6579);
    }

    /// #2638 review round 7, P2 regression: a mixed complete/cut `mod` run
    /// must not collapse the cut module into a closed-looking span — the
    /// collapse must stop before the open-ended entry, preserving its own
    /// `N-…` marker.
    #[test]
    fn a_mixed_complete_and_cut_mod_run_does_not_collapse_the_cut_entry() {
        let entries = vec![
            OutlineEntry {
                start_line: 1,
                end_line: 1,
                header: "mod a".to_string(),
                open_ended: false,
            },
            OutlineEntry {
                start_line: 2,
                end_line: 2,
                header: "mod b".to_string(),
                open_ended: false,
            },
            OutlineEntry {
                start_line: 3,
                end_line: 3,
                header: "mod c".to_string(),
                open_ended: true,
            },
        ];
        let rendered = render_outline(&entries);
        assert!(
            rendered.contains("1-2\tmod ×2 (a, b)"),
            "the complete run still collapses: {rendered}"
        );
        assert!(
            rendered.contains("3-…\tmod c"),
            "the cut mod keeps its own open-ended marker: {rendered}"
        );
    }
}
