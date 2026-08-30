//! **`mermaid` — the first registrant.**
//!
//! Holds the info string, the budgets, the pre-scan, and — since E0b (#1869)
//! — the render itself, through the pure [`super::flowchart`] renderer.
//!
//! E0a said this crate should not grow a renderer. E0b is why it did: the
//! BROWSER cannot draw under the strict CSP C3b shipped, so drawing moved to
//! where there is no CSS to block. A surface that cannot present graphics
//! still gets source, because it asks for a level this declines to exceed.
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
/// * **nodes** — the endpoints an edge joins, plus any line that declares
///   something on its own. A line is split on its arrow tokens and each
///   remaining run that contains a letter or digit counts once, so `A-->B`
///   is two and `A-->B-->C` is three. An arrow with nothing on either side
///   declares nothing and counts zero.
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
        // Walk the line once, splitting on arrows. A run between two arrows
        // (or between an arrow and an end of line) is an endpoint if it
        // contains a letter or digit — the same "declares something rather
        // than punctuation" test the whole line used to get, applied per
        // endpoint instead.
        let mut rest = trimmed;
        let mut endpoint_has_content = false;
        'scan: while !rest.is_empty() {
            for token in EDGES {
                if let Some(tail) = rest.strip_prefix(token) {
                    edges += 1;
                    if endpoint_has_content {
                        nodes += 1;
                        endpoint_has_content = false;
                    }
                    rest = tail;
                    continue 'scan;
                }
            }
            let mut chars = rest.chars();
            if chars.next().is_some_and(char::is_alphanumeric) {
                endpoint_has_content = true;
            }
            rest = chars.as_str();
        }
        // The run after the last arrow, or the whole line if it had none.
        if endpoint_has_content {
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

    /// Render the flowchart subset, or decline.
    ///
    /// E0b (#1869): `newt-core` grew a renderer after all, and deliberately —
    /// a PURE one. C3b measured that the browser path cannot draw under the
    /// strict CSP (no `cspNonce`, a per-render-scoped stylesheet no hash can
    /// admit, and a blocked theme rendering black-on-black), so drawing moved
    /// here where there is no CSS to block.
    ///
    /// Declines anything outside the subset, and declining is a first-class
    /// answer: E0a's contract turns it into the readable source.
    fn render(&self, source: &str, level: SupportLevel) -> Option<Enhancement> {
        if level < SupportLevel::Graphics {
            return None;
        }
        let (svg, alt) = super::flowchart::render(source)?;
        Enhancement::graphics(svg, alt)
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

    /// A supported flowchart renders; the source rides along regardless.
    #[test]
    fn a_supported_flowchart_renders_and_keeps_its_source() {
        let reg = Registry::new().with(&MERMAID);
        let src = "flowchart TD\n  A[Start] --> B[Finish]";
        let p = reg.present("mermaid", src, graphics(), &Frozen);
        assert_eq!(p.source(), src, "the source rides along even when drawn");
        let e = p.enhancement().expect("this subset renders");
        assert_eq!(e.level(), SupportLevel::Graphics);
        assert!(e.payload().starts_with("<svg"), "payload: {}", e.payload());
        assert!(
            e.accessible_text().contains("Start"),
            "alt: {}",
            e.accessible_text()
        );
    }

    /// Syntax outside the subset declines, and declining is source — not an
    /// error, and never a half-drawn picture.
    #[test]
    fn syntax_outside_the_subset_falls_back_to_source() {
        let reg = Registry::new().with(&MERMAID);
        for src in [
            "sequenceDiagram\n  A->>B: hi",
            "flowchart TD\n  subgraph one\n    A --> B\n  end",
            "flowchart TD\n  A --> B\n  click A callback",
            "flowchart XX\n  A --> B",
            "gantt\n  title X",
            "not a diagram at all",
        ] {
            let p = reg.present("mermaid", src, graphics(), &Frozen);
            assert!(p.enhancement().is_none(), "should decline: {src:?}");
            assert_eq!(p.source(), src, "and keep the source: {src:?}");
            assert_eq!(p.fallback(), Some(FallbackReason::HandlerDeclined));
        }
    }

    /// A surface that cannot draw gets source even for a diagram that would
    /// have rendered — the C3b runtime case, where the page's own policy
    /// blocks graphics.
    #[test]
    fn a_source_only_surface_gets_source_for_a_renderable_diagram() {
        let reg = Registry::new().with(&MERMAID);
        let src = "flowchart TD\n  A --> B";
        let p = reg.present("mermaid", src, Capabilities::source_only(), &Frozen);
        assert!(p.enhancement().is_none());
        assert_eq!(p.source(), src);
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

#[cfg(test)]
mod e0a_shape_scan {
    use super::measure;

    /// **#1956, pinned.** The fuzz seed that found this is not reproducible
    /// by design, so the minimal input is nailed down here where no seed can
    /// lose it.
    ///
    /// `"->"` is an arrow with nothing on either side. It joins no
    /// endpoints, so it declares no nodes. The scan used to credit it TWO —
    /// which is not merely a miscount: `Budgets` are applied to this
    /// `Shape`, so two bytes of attacker-chosen input bought two nodes of
    /// budget, the cheapest possible way to spend someone else's ceiling.
    #[test]
    fn an_arrow_with_no_endpoints_declares_no_nodes() {
        let shape = measure("->");
        assert_eq!(shape.nodes, 0, "an arrow joining nothing declared nodes");
        assert_eq!(shape.edges, 1, "the arrow itself is still an edge");
    }

    /// The twin, and the reason the fix is not "return 0 more often": real
    /// endpoints must still be counted, or the budget stops measuring
    /// anything and every diagram passes.
    #[test]
    fn real_endpoints_are_still_counted() {
        assert_eq!(measure("A->B").nodes, 2);
        assert_eq!(measure("graph TD").nodes, 1);
    }

    /// The same bug in the other direction, which #1956 did not name: the
    /// old scan credited a flat `2` to any line containing an arrow, so a
    /// chain UNDER-counted. Three endpoints are three nodes.
    #[test]
    fn a_chain_counts_every_endpoint_not_two() {
        assert_eq!(measure("A->B->C").nodes, 3, "a chain under-counted");
        assert_eq!(measure("A-->B\nC-->D").nodes, 4);
    }

    /// Punctuation declares nothing, before or after an arrow. This is the
    /// rule that keeps `"->"` at zero from being a special case.
    #[test]
    fn punctuation_is_not_an_endpoint() {
        assert_eq!(measure("!!!").nodes, 0);
        assert_eq!(measure("!!!->???").nodes, 0);
        assert_eq!(measure("!!!->B").nodes, 1);
    }

    /// The bound the fuzz property now asserts, checked here on the cases
    /// that make it tight — so a reader can see WHY it is `edges + lines`
    /// and not something looser.
    #[test]
    fn nodes_are_bounded_by_edges_plus_lines() {
        for src in ["->", "A->B", "A->B->C", "graph TD", "!!!", "A-->B\nC-->D"] {
            let shape = measure(src);
            assert!(
                shape.nodes <= shape.edges + src.lines().count(),
                "{src:?}: {} nodes exceeds {} edges + {} lines",
                shape.nodes,
                shape.edges,
                src.lines().count()
            );
        }
    }
}
