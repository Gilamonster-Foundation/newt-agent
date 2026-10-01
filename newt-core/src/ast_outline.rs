//! Rust-only tree-sitter outline for aged `read_file` results (#2557).
//!
//! Compiled in only behind the `ast` feature (vendors C via `tree-sitter` +
//! `tree-sitter-rust`, off by default). `prune::one_line_summary` calls
//! [`outline_rust`] instead of the plain one-liner when it can, and falls
//! back to the one-liner otherwise — never to a regex approximation.

use std::sync::OnceLock;
use tree_sitter::{Node, Parser};
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

/// Item-start keywords a top-level Rust definition can begin with — matched
/// against the REAL parse tree's own token kinds (#2638 review round 8),
/// never against raw text. Config, not logic: a new item kind is one array
/// entry.
const ITEM_KEYWORDS: &[&str] = &[
    "fn", "struct", "enum", "impl", "trait", "mod", "type", "static", "const",
];
const VISIBILITY_PREFIXES: &[&str] = &["pub(crate)", "pub(super)", "pub(self)", "pub"];

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

/// The outermost node covering the parse's unrecovered tail: `source_file`'s
/// own last top-level child, when it reports an error, or the root itself
/// when the WHOLE file collapsed into one `ERROR` node (a deep-enough page
/// cut makes tree-sitter give up on recovering any earlier structure at
/// all and fold complete siblings the tags query already found into it
/// too — #2638 fix item 1).
fn final_incomplete_node(root: Node<'_>) -> Option<Node<'_>> {
    let candidate = if root.kind() == "source_file" {
        let mut cursor = root.walk();
        root.children(&mut cursor).last()?
    } else {
        root
    };
    candidate.has_error().then_some(candidate)
}

/// A node tree-sitter could not parse completely: an `ERROR` node, a
/// `MISSING` placeholder, or any node containing one. Everything else is a
/// COMPLETE item (a `function_item`, a `const_item`, …) whatever the tags
/// query says about it — tree-sitter-rust's bundled `tags.scm` tags no
/// `const`/`static` at all, so "untagged" never means "incomplete" (#2638
/// review round 9).
fn is_incomplete(node: Node<'_>) -> bool {
    node.is_error() || node.is_missing() || node.has_error()
}

/// Every byte offset within `node`'s INCOMPLETE regions where an unnamed
/// keyword token (`fn`, `struct`, …) is IMMEDIATELY followed by a sibling
/// `identifier` node, exactly as tree-sitter itself parsed it — the only
/// evidence #2638 review round 8 accepts for a recovered (open-ended)
/// definition. A hand-rolled text scanner can't tell a real keyword from
/// one embedded in a string/char literal (`let marker = '"'; let _ =
/// r#"\nfn imaginary(`); the real parser can, because that text never
/// becomes separate `fn`/`identifier` tokens in the first place — it stays
/// inside whatever comment/string/char-literal token swallowed it (verified
/// by walking the actual tree: the raw-string case above never produces an
/// unnamed `fn` node at all, only an `identifier` node whose *text* happens
/// to read "fn").
///
/// A complete subtree is never descended into (round 9): a deep enough page
/// cut folds the fragment's complete earlier siblings — and the cut body's
/// own complete local items — into the same `ERROR` node as the cut
/// definition's loose tokens, and a complete `const_item` there is a
/// complete item, not evidence of a cut.
fn keyword_identifier_starts(node: Node<'_>) -> Vec<usize> {
    let mut hits = Vec::new();
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else {
            continue;
        };
        if !child.is_named()
            && ITEM_KEYWORDS.contains(&child.kind())
            && node
                .child(i + 1)
                .is_some_and(|next| next.kind() == "identifier")
        {
            hits.push(child.start_byte());
        }
        if is_incomplete(child) {
            hits.extend(keyword_identifier_starts(child));
        }
    }
    hits
}

/// The page-cut counterpart of tags-based recovery (#2638 fix item 1): a
/// definition whose CLOSE never arrives inside this fragment leaves an
/// unbalanced brace count with no `is_definition` tag of its own. Recovery
/// is granted ONLY on parser evidence — a keyword token immediately
/// followed by an identifier node inside the parse's final incomplete
/// region, AFTER the last tagged entry's end line, and exactly ONE such
/// pair — never on text that merely looks like one. No evidence, or more
/// than one incomplete definition the tree can't rank, means no recovered
/// entry: the caller declines to the existing generic fallback rather than
/// guess (review round 9).
fn trailing_truncated_definition(
    source: &str,
    first_line: usize,
    entries: &[OutlineEntry],
) -> Option<OutlineEntry> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("tree-sitter-rust's grammar must load");
    let tree = parser.parse(source, None)?;
    let target = final_incomplete_node(tree.root_node())?;
    let line_of = |byte: usize| source[..byte.min(source.len())].matches('\n').count();
    // Recovery boundary: the cut item starts AFTER the last tagged entry
    // ENDS. Everything up to there parsed as complete items, tagged or not.
    let last_tagged_end = entries.iter().map(|e| e.end_line).max().unwrap_or(0);
    let candidates: Vec<usize> = keyword_identifier_starts(target)
        .into_iter()
        .filter(|&b| first_line + line_of(b) > last_tagged_end)
        .collect();
    // Two incomplete definitions in one region (a cut `mod tests {` whose
    // cut `fn` is also incomplete) are flat siblings under the same `ERROR`;
    // the tree doesn't say which one the page cut, so neither is reported.
    let &[start_byte] = candidates.as_slice() else {
        return None;
    };
    let total_lines = source.lines().count();
    let last_line = first_line + total_lines.saturating_sub(1);
    let line_start = source[..start_byte].rfind('\n').map_or(0, |p| p + 1);
    let header = source[line_start..]
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches(['{', ';'])
        .trim()
        .to_string();
    Some(OutlineEntry {
        start_line: first_line + line_of(start_byte),
        end_line: last_line,
        header,
        open_ended: true,
    })
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
/// behind a closed-looking collapsed span. Only SINGLE-LINE declarations
/// (`mod x;`) collapse; a multi-line inline module (`mod x { ... }` spanning
/// more than one line) always keeps its own entry and real extent, since a
/// collapsed run's rendered end is its LAST member's `start_line`, which
/// would truncate an inline module's body (#2638 review round 8, item 2).
pub fn render_outline(entries: &[OutlineEntry]) -> String {
    let is_collapsible = |e: &OutlineEntry| !e.open_ended && e.start_line == e.end_line;
    let mut lines = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        if is_collapsible(&entries[i]) {
            if let Some(first_name) = mod_name(&entries[i].header) {
                let mut names = vec![first_name];
                let mut j = i + 1;
                while j < entries.len() && is_collapsible(&entries[j]) {
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

    /// #2638 review round 8, P2 negative control: the review's counter-
    /// example — a real `fn`, then a char literal containing `"`, then an
    /// unterminated raw string whose body merely LOOKS like a `fn` header.
    /// A hand-rolled scanner mistakes the char literal's `"` for a string
    /// opener and the raw string's opening `"` for its closer, exposing
    /// `imaginary` outside its own exclusion. The real parser never
    /// tokenizes `fn`/`imaginary` as a keyword+identifier pair here at all
    /// (confirmed by inspecting the tree: that region parses as an
    /// `identifier` node whose *text* happens to read "fn", not an unnamed
    /// `fn` keyword token), so no recovered entry should ever appear.
    #[test]
    fn a_definition_inside_an_unterminated_raw_string_after_a_char_literal_is_not_invented() {
        let src = "fn real() {}\nlet marker = '\"';\nlet _ = r#\"\nfn imaginary(\n";
        let entries = outline_rust(src, 1).expect("valid rust prefix");
        assert!(
            !entries.iter().any(|e| e.header.contains("imaginary")),
            "no parser evidence exists for `imaginary`: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.header.contains("real")),
            "the real fn must still appear: {entries:?}"
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

    /// #2638 review round 9, P2 control (measured red): a COMPLETE but
    /// untagged `const` and `static` (tree-sitter-rust's bundled `tags.scm`
    /// tags neither) sit before the genuine page-cut `fn` in the SAME error
    /// region — a cut this deep collapses the whole fragment into one root
    /// `ERROR` node whose children are the complete `const_item` /
    /// `static_item` and then the cut fn's loose `fn`/`identifier` tokens
    /// (shape confirmed by dumping the real tree). "Untagged" is not
    /// "incomplete": the recovered entry must be the fn, never the const.
    #[test]
    fn a_complete_untagged_const_and_static_before_the_cut_fn_are_not_recovered() {
        let src = "fn real() {}\nconst X: u32 = 1;\nstatic Y: u32 = 2;\nfn cut() {\n    if a {\n        if b {\n            do_thing();\n";
        let entries = outline_rust(src, 1).expect("valid rust prefix");
        let open: Vec<&OutlineEntry> = entries.iter().filter(|e| e.open_ended).collect();
        assert_eq!(
            open.len(),
            1,
            "exactly one recovered entry, the cut fn: {entries:?}"
        );
        assert!(
            open[0].header.contains("fn cut") && open[0].start_line == 4,
            "the recovered entry is the cut fn at line 4, not a complete earlier item: {entries:?}"
        );
        assert!(
            !entries
                .iter()
                .any(|e| e.header.contains("const X") || e.header.contains("static Y")),
            "a complete untagged const/static is never a recovered entry: {entries:?}"
        );
        let real = entries
            .iter()
            .find(|e| e.header.contains("fn real"))
            .expect("the complete tagged fn still appears");
        assert!(!real.open_ended);
    }

    /// #2638 review round 9 positive (the local-constant-after-cut case the
    /// real `agentic/mod.rs` offset-6000 page exercises at scale): a cut
    /// fn's own unparsed body can contain a complete local `const` — a
    /// complete subtree inside the error region. It is never recovery
    /// evidence and never shadows the cut fn, which is still reported.
    #[test]
    fn a_local_const_inside_the_cut_body_does_not_shadow_the_cut_fn() {
        let src = "fn real() {}\n\nfn cut() {\n    const CAP: usize = 3;\n    let x = CAP;\n";
        let entries = outline_rust(src, 1).expect("valid rust prefix");
        let entry = entries
            .iter()
            .find(|e| e.open_ended)
            .expect("the cut fn must still be recovered");
        assert!(entry.header.contains("fn cut"), "got: {entries:?}");
        assert_eq!(entry.start_line, 3);
        assert!(
            !entries.iter().any(|e| e.header.contains("const CAP")),
            "a local const inside the cut body is not an entry: {entries:?}"
        );
    }

    /// #2638 review round 9 (measured red): two INCOMPLETE definitions in
    /// one error region — a page cut inside an inline `mod tests {` block,
    /// mid-way through its first fn — leave the tree unable to say which
    /// one the page cut (both are loose `mod`/`fn` + `identifier` token
    /// pairs, flat under the same `ERROR`, and both lie after the last
    /// tagged entry). Recovery declines rather than guessing; the caller's
    /// generic fallback takes the page.
    #[test]
    fn two_incomplete_definitions_in_one_error_region_decline_recovery() {
        let src = "fn real() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n    fn cut() {\n        if a {\n            do_thing();\n";
        let entries = outline_rust(src, 1).expect("valid rust prefix");
        assert!(
            !entries.iter().any(|e| e.open_ended),
            "ambiguous recovery must decline, not guess an entry: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.header.contains("fn real")),
            "the complete tagged fn still appears: {entries:?}"
        );
    }

    /// #2638 review round 9 (measured red): the same cut `mod tests {`, but
    /// with a COMPLETE fn between the module header and the cut fn. That fn
    /// is tagged, so the recovery boundary (after the last tagged entry's
    /// end) moves past the `mod` token pair, and the cut fn after it is the
    /// single remaining candidate — recovered, while the `mod` is not
    /// reported (the old `.min()` guessed `mod tests` here).
    #[test]
    fn a_complete_tagged_fn_inside_the_cut_mod_moves_the_boundary_past_the_mod() {
        let src = "fn real() {}\n\n#[cfg(test)]\nmod tests {\n    fn done() {}\n    fn cut() {\n        if a {\n            do_thing();\n";
        let entries = outline_rust(src, 1).expect("valid rust prefix");
        let open: Vec<&OutlineEntry> = entries.iter().filter(|e| e.open_ended).collect();
        assert_eq!(open.len(), 1, "exactly one recovered entry: {entries:?}");
        assert!(
            open[0].header.contains("fn cut") && open[0].start_line == 6,
            "the cut fn after the boundary is recovered: {entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e.header.contains("mod tests")),
            "the mod before the boundary is not guessed: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e.header.contains("fn done")),
            "complete fns inside the region stay tagged: {entries:?}"
        );
    }

    /// #2638 review round 8, P2: a run of MULTI-LINE inline modules must
    /// never collapse — the collapsed span's end is its last member's
    /// `start_line`, which would truncate an inline module's own body.
    /// Only single-line `mod x;` declarations collapse.
    #[test]
    fn a_multiline_mod_run_never_collapses() {
        let entries = vec![
            OutlineEntry {
                start_line: 1,
                end_line: 3,
                header: "mod a".to_string(),
                open_ended: false,
            },
            OutlineEntry {
                start_line: 4,
                end_line: 6,
                header: "mod b".to_string(),
                open_ended: false,
            },
        ];
        let rendered = render_outline(&entries);
        assert_eq!(
            rendered, "    1-3\tmod a\n    4-6\tmod b",
            "multi-line mods keep their own extents, never collapsed: {rendered}"
        );
    }
}
