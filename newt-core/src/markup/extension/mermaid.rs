//! **`mermaid` — the first registrant.**
//!
//! Holds the info string, the budgets, and the pre-scan. It renders NOTHING:
//! `newt-core` has no diagram renderer and should not grow one, so
//! [`Mermaid::render`] returns `None` and every presentation from this crate
//! falls back to source. That is the correct answer for the plain and headless
//! tiers, which have no graphics at all.
//!
//! The value it carries is the part a second implementation would otherwise
//! duplicate: **what the info string is, what the bounds are, and how the
//! source is measured.** E0b registers a browser-side renderer for the same
//! key and reuses [`BUDGETS`] and [`measure`] rather than inventing a second
//! set — which is the whole reason the measurement is a function here instead
//! of a detail inside a renderer.
//!
//! ## Why the pre-scan is a token count and not a parse
//!
//! Budgets exist to bound work before it is done, so the thing that decides
//! must be cheaper than the thing it guards. A scan that counted accurately by
//! parsing Mermaid would be the expensive step it is supposed to prevent — and
//! it would be a second Mermaid grammar in the tree, which the epic deletes
//! duplicates to avoid. Counting arrows and declaration lines is coarse, and
//! deliberately so: it is a *bound*, not an analysis, and a source that slips
//! under it is still held by the output and time budgets afterwards.
//!
//! Nothing here interprets what the source MEANS. It counts bytes and tokens.

use super::{Budgets, Enhancement, Extension, Shape, SupportLevel};

/// The fence info string this claims.
pub const INFO: &str = "mermaid";

/// The bounds a mermaid block runs under.
///
/// Sized to be generous for a document and mean for a denial-of-service: the
/// largest diagram in this repo's own docs is under 2 KiB, and a reader cannot
/// follow a hundred-node graph on a phone anyway — the fallback is more useful
/// to them than a render would be.
pub const BUDGETS: Budgets = Budgets {
    source_bytes: 16 * 1024,
    nodes: 256,
    edges: 512,
    depth: 32,
    output_bytes: 256 * 1024,
    // 250 ms. A budget on the RESULT: a pure handler cannot be preempted, so
    // exceeding this discards the output rather than interrupting the work.
    // Stated because a reader would otherwise assume it bounds latency.
    time_nanos: 250_000_000,
};

/// The arrow forms Mermaid uses for an edge, longest first so `-->` is not
/// counted as `--`.
const EDGES: &[&str] = &[
    "<-->", "-.->", "==>", "-->", "---", "-.-", "===", "--x", "--o", "->", "~~~",
];

/// Count a source's shape, cheaply and without parsing.
///
/// * **edges** — arrow tokens, longest-match-first so one arrow counts once.
/// * **nodes** — lines that declare something rather than continue it: a
///   non-empty line that is not a directive and not pure punctuation.
/// * **depth** — the deepest indentation, in levels of two spaces, which is
///   how `subgraph` nesting shows up without knowing what `subgraph` means.
#[must_use]
pub fn measure(source: &str) -> Shape {
    let mut edges = 0usize;
    let mut nodes = 0usize;
    let mut depth = 0usize;

    for line in source.lines() {
        let indent = line.len() - line.trim_start().len();
        depth = depth.max(indent / 2);

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }

        // Longest-match-first, consuming as we go, so `<-->` is one edge and
        // not also a `-->` and a `--`.
        let mut rest = trimmed;
        let mut found_edge = false;
        'scan: while !rest.is_empty() {
            for token in EDGES {
                if let Some(tail) = rest.strip_prefix(token) {
                    edges += 1;
                    found_edge = true;
                    rest = tail;
                    continue 'scan;
                }
            }
            let mut chars = rest.chars();
            chars.next();
            rest = chars.as_str();
        }

        // A line with an edge declares the nodes on either side of it; a line
        // without one declares at most itself. Coarse on purpose — see the
        // module docs.
        if found_edge {
            nodes += 2;
        } else if trimmed.chars().any(char::is_alphanumeric) {
            nodes += 1;
        }
    }

    Shape {
        nodes,
        edges,
        depth,
    }
}

/// The `mermaid` registrant.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mermaid;

/// The registrant, as the registry holds it.
pub static MERMAID: Mermaid = Mermaid;

impl Extension for Mermaid {
    fn info(&self) -> &'static str {
        INFO
    }

    fn budgets(&self) -> Budgets {
        BUDGETS
    }

    fn measure(&self, source: &str) -> Shape {
        measure(source)
    }

    /// Always `None`. `newt-core` renders no diagrams.
    ///
    /// Not a stub awaiting an implementation — a statement that this crate has
    /// no graphics tier. A surface that CAN draw registers its own renderer
    /// for [`INFO`] over these same budgets; a surface that cannot gets the
    /// source, which is the right answer and not a missing feature.
    fn render(&self, _source: &str, _level: SupportLevel) -> Option<Enhancement> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markup::extension::{Capabilities, FallbackReason, Registry, SupportLevel};

    struct Frozen;
    impl crate::markup::extension::Stopwatch for Frozen {
        fn read_nanos(&self) -> u128 {
            0
        }
    }

    fn graphics() -> Capabilities {
        Capabilities {
            highest: SupportLevel::Graphics,
        }
    }

    /// `newt-core` renders no diagrams, so a registered mermaid block still
    /// presents as source — and says that is because the handler had nothing,
    /// not because something failed.
    #[test]
    fn newt_core_presents_mermaid_as_source_and_says_why() {
        let reg = Registry::new().with(&MERMAID);
        let src = "graph TD\n  A --> B";
        let p = reg.present("mermaid", src, graphics(), &Frozen);
        assert_eq!(p.source(), src);
        assert!(p.enhancement().is_none(), "this crate draws nothing");
        assert_eq!(p.fallback(), Some(FallbackReason::HandlerDeclined));
    }

    /// The pre-scan counts arrows longest-first, so one arrow is one edge.
    #[test]
    fn the_scan_counts_each_arrow_once() {
        for (src, edges) in [
            ("A --> B", 1),
            ("A <--> B", 1),
            ("A -.-> B", 1),
            ("A --- B", 1),
            ("A ==> B", 1),
            ("A --> B --> C", 2),
            ("A --> B\nB --> C", 2),
            ("plain text", 0),
        ] {
            assert_eq!(measure(src).edges, edges, "edges in {src:?}");
        }
    }

    /// Indentation is the depth signal, in two-space levels.
    #[test]
    fn the_scan_reports_nesting_depth() {
        assert_eq!(measure("a").depth, 0);
        assert_eq!(measure("  a").depth, 1);
        assert_eq!(measure("a\n    b\n  c").depth, 2);
    }

    /// Comments and blank lines declare nothing.
    #[test]
    fn comments_and_blanks_are_not_nodes() {
        assert_eq!(measure("%% a comment\n\n   \n").nodes, 0);
        assert_eq!(measure("%% c\nA --> B").nodes, 2);
    }

    /// **Adversarial corpus.** Not a handful of examples: the shapes an
    /// author reaches for when they want the scan to be wrong. None may
    /// panic, none may hang, and each must stay inside the declared budgets
    /// or be refused BY them — never rendered, and never lost.
    #[test]
    fn the_adversarial_corpus_is_bounded_and_never_loses_source() {
        let corpus: Vec<(&str, String)> = vec![
            ("empty", String::new()),
            ("only whitespace", "   \n\t\n  ".into()),
            ("no newline at all", "A-->B".repeat(50)),
            ("one enormous line", format!("A{}B", "-".repeat(40_000))),
            ("arrow soup", "-->".repeat(20_000)),
            ("nested arrows", "<-->-.->==>---".repeat(5_000)),
            (
                "deep indentation",
                (0..500)
                    .map(|i| format!("{}n\n", " ".repeat(i * 2)))
                    .collect(),
            ),
            (
                "many nodes",
                (0..5_000).map(|i| format!("n{i}\n")).collect(),
            ),
            ("crlf line endings", "A --> B\r\nB --> C\r\n".repeat(100)),
            (
                "unicode identifiers",
                "日本 --> 中文\nこれ --> それ".repeat(200),
            ),
            (
                "combining marks",
                "e\u{0301}\u{0301}\u{0301} --> b".repeat(500),
            ),
            ("rtl override", "A \u{202E}--> B".repeat(500)),
            ("nul and control bytes", "A\u{0}-->\u{7}B".repeat(500)),
            ("lone surrogate escape text", "A --> \\ud800".repeat(500)),
            (
                "comment that never ends",
                format!("%%{}", "x".repeat(30_000)),
            ),
            (
                "html in a label",
                "A[<script>alert(1)</script>] --> B".repeat(300),
            ),
            ("markdown fence inside", "```\nA --> B\n```".repeat(500)),
            ("giant single token", "n".repeat(60_000)),
            ("only comments", "%% c\n".repeat(10_000)),
            ("tabs as indentation", "\t\t\tA --> B\n".repeat(500)),
        ];

        let reg = Registry::new().with(&MERMAID);
        for (what, src) in &corpus {
            // The scan terminates and reports something sane.
            let shape = measure(src);
            assert!(shape.nodes <= src.len() + 1, "[{what}] nodes exceed bytes");
            assert!(shape.edges <= src.len() + 1, "[{what}] edges exceed bytes");

            // And the contract holds: source out, always, whatever happened.
            let p = reg.present("mermaid", src, graphics(), &Frozen);
            assert_eq!(p.source(), src, "[{what}] source was lost");
            assert!(
                p.enhancement().is_none(),
                "[{what}] this crate draws nothing"
            );
            assert!(p.fallback().is_some(), "[{what}] a fallback must say why");
        }
    }

    /// A source past the byte budget is refused before the scan is even
    /// consulted — the bound is on WORK, not just on the verdict.
    #[test]
    fn an_oversized_source_is_refused_by_the_byte_budget() {
        let reg = Registry::new().with(&MERMAID);
        let huge = "A --> B\n".repeat(BUDGETS.source_bytes);
        let p = reg.present("mermaid", &huge, graphics(), &Frozen);
        assert_eq!(
            p.fallback(),
            Some(FallbackReason::OverBudget(
                crate::markup::extension::Budget::SourceBytes
            ))
        );
        assert_eq!(p.source(), huge, "even refused, the source comes back");
    }
}
