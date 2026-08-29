//! **A pure, server-side flowchart renderer.**
//!
//! E0b's answer to a question C3b left open: can the web cockpit draw diagrams
//! again *without* weakening the Content-Security-Policy it shipped?
//!
//! The browser path could not. Mermaid 11.15.0 has no `cspNonce`, themes its
//! output through a `<style>` element scoped to a per-render id — so no hash
//! and no static stylesheet can admit it — and when that stylesheet is blocked
//! the diagram draws black-on-black: unreadable, not merely unstyled.
//!
//! Rendering here instead removes the problem rather than working around it.
//! There is no client runtime to nonce, no stylesheet to admit, and the output
//! uses SVG **presentation attributes** only, which no `style-src*` directive
//! governs. The strict policy is untouched — and the relaxation C3b needed for
//! Mermaid's per-node `style=` attributes is no longer needed by diagrams at
//! all.
//!
//! ## What it does not do
//!
//! It renders a SUBSET: `flowchart`/`graph` with `TD`/`LR`, node declarations,
//! and simple edges. Subgraphs, styling directives, click handlers, and every
//! other diagram type decline — and declining is a first-class answer, because
//! E0a's contract turns it into the readable source. A renderer that guessed
//! at syntax it half-understood would draw a picture that is not what the
//! author wrote, which is worse than showing them what they wrote.
//!
//! Purity is structural: text in, string out, no I/O, and nothing the diagram
//! says is ever evaluated. `newt-core/tests/extension_purity.rs` holds it.

pub mod layout;
pub mod parse;
pub mod svg;

/// Render `source` to SVG plus its accessible text, or decline.
///
/// `None` means "show the source" — an unparseable diagram, one outside the
/// subset, or a cyclic graph this layout cannot rank honestly.
#[must_use]
pub fn render(source: &str) -> Option<(String, String)> {
    let graph = parse::parse(source)?;
    let placed = layout::layout(&graph)?;
    Some((svg::render(&graph, &placed), svg::accessible_text(&graph)))
}

#[cfg(test)]
mod readability {
    use super::*;

    /// Every node box, as the layout placed it.
    fn boxes(source: &str) -> Vec<(String, f64, f64, f64, f64)> {
        let graph = parse::parse(source).expect("parses");
        let placed = layout::layout(&graph).expect("lays out");
        placed
            .placed
            .iter()
            .map(|p| (graph.nodes[p.node].label.clone(), p.x, p.y, p.w, p.h))
            .collect()
    }

    const SAMPLE: &str =
        "flowchart TD\n  A[Harness] --> B[Markdown]\n  B --> C[Mobile GUI]\n  A --> C";

    /// **The diagram's contrast is the PAGE's contrast, by construction.**
    ///
    /// This is the assertion the black-on-black regression needed and did not
    /// have. C3b's acceptance test asserted a diagram was PRESENT; the theme
    /// that would have coloured it was blocked, both fill and text fell back
    /// to the UA default black, and the test stayed green over an unreadable
    /// picture.
    ///
    /// There is no theme here to block. Every stroke and glyph is
    /// `currentColor` and every node interior is `fill="none"`, so the diagram
    /// inherits the page's own foreground on the page's own background — it
    /// cannot disagree with the page, in either theme, and it cannot be
    /// invisible unless the page itself is.
    #[test]
    fn contrast_is_the_pages_own_and_no_colour_is_hardcoded() {
        let (svg, _) = render(SAMPLE).expect("renders");

        // Every ink-bearing attribute is the page's own colour…
        for (attr, count) in [("stroke=\"currentColor\"", 3), ("fill=\"currentColor\"", 3)] {
            assert!(
                svg.matches(attr).count() >= count,
                "expected at least {count} × {attr} in:\n{svg}"
            );
        }
        // …node interiors are transparent, so text sits on the page…
        assert!(svg.contains(r#"fill="none""#), "boxes must not be filled");

        // …and NOTHING names a colour of its own. A hardcoded hex would be a
        // colour that can disagree with the theme, which is the whole defect.
        for forbidden in ["#", "rgb(", "hsl(", "black", "white", "red", "blue"] {
            assert!(
                !svg.contains(forbidden),
                "a hardcoded colour ({forbidden}) can disagree with the page: {svg}"
            );
        }
    }

    /// **Anti-vacuous twin.** The check above is mostly `!contains`, which an
    /// EMPTY svg satisfies. This pins that the detector sees a hardcoded
    /// colour when one is really there, and that the sample really renders.
    #[test]
    fn the_contrast_check_would_notice_a_hardcoded_colour() {
        let (svg, _) = render(SAMPLE).expect("renders");
        assert!(svg.len() > 200, "the sample must really render: {svg}");
        let seeded = svg.replace(r#"fill="none""#, r##"fill="#000000""##);
        assert!(
            seeded.contains('#'),
            "the check cannot see a hardcoded colour even when one is present"
        );
    }

    /// **Every label fits inside its own box.** Text wider than its box is
    /// text a reader cannot read, and it is invisible to any test that only
    /// asks whether the diagram exists.
    #[test]
    fn every_label_fits_inside_its_box() {
        for source in [
            SAMPLE,
            "flowchart LR\n  A[a] --> B[a much longer label than the first]",
            "flowchart TD\n  X[\u{65e5}\u{672c}\u{8a9e}] --> Y[ok]",
        ] {
            for (label, _x, _y, w, h) in boxes(source) {
                let needed = label.chars().count() as f64 * 8.0;
                assert!(
                    needed <= w,
                    "label {label:?} needs {needed} but its box is {w}"
                );
                assert!(h >= 20.0, "box too short to hold a line of text");
            }
        }
    }

    /// **No two boxes overlap.** Overlapping boxes hide each other's text,
    /// which again passes a presence check and fails a reader.
    #[test]
    fn no_two_boxes_overlap() {
        for source in [
            SAMPLE,
            "flowchart LR\n  A --> B\n  A --> C\n  B --> D\n  C --> D",
            "flowchart TD\n  A --> B\n  A --> C\n  A --> D\n  A --> E",
        ] {
            let placed = boxes(source);
            for (i, a) in placed.iter().enumerate() {
                for b in placed.iter().skip(i + 1) {
                    let apart = a.1 + a.3 <= b.1
                        || b.1 + b.3 <= a.1
                        || a.2 + a.4 <= b.2
                        || b.2 + b.4 <= a.2;
                    assert!(apart, "{:?} overlaps {:?}", a.0, b.0);
                }
            }
        }
    }

    /// Everything the author wrote is in the picture — no node and no edge is
    /// silently dropped, which would be a diagram that is not what they wrote.
    #[test]
    fn every_node_and_edge_reaches_the_output() {
        let (svg, alt) = render(SAMPLE).expect("renders");
        for label in ["Harness", "Markdown", "Mobile GUI"] {
            assert!(svg.contains(label), "node {label} missing from svg");
            assert!(alt.contains(label), "node {label} missing from alt text");
        }
        assert_eq!(svg.matches("<rect").count(), 3, "one box per node");
        assert_eq!(svg.matches("<line").count(), 3, "one line per edge");
        assert_eq!(svg.matches("<polygon").count(), 3, "one arrowhead per edge");
    }

    /// Author text is escaped on the way into the SVG. The web sanitizer is a
    /// backstop, not the plan — a renderer that emitted raw author text and
    /// relied on someone downstream would be the "trusted because we produced
    /// it" mistake E0a's contract names.
    #[test]
    fn author_text_is_escaped_before_it_reaches_the_svg() {
        let (svg, alt) =
            render("flowchart TD\n  A[<script>alert(1)</script>] --> B[\"quoted\" & 'apostrophe']")
                .expect("renders");
        assert!(!svg.contains("<script>"), "raw script tag in svg: {svg}");
        assert!(
            svg.contains("&lt;script&gt;"),
            "escaped form missing: {svg}"
        );
        assert!(!svg.contains("alert(1)</"), "unescaped close tag: {svg}");
        // The accessible text is a plain string, so it carries the raw label —
        // it is escaped by the SVG writer when embedded, and asserting that is
        // what stops the two paths drifting.
        assert!(alt.contains("script"), "alt keeps the text: {alt}");
        assert!(svg.contains("&amp;"), "ampersand escaped: {svg}");
        assert!(
            svg.contains("&#39;") || svg.contains("&quot;"),
            "quotes escaped"
        );
    }

    /// A cyclic graph declines rather than being drawn with guessed ranks.
    #[test]
    fn a_cycle_declines_rather_than_guessing() {
        assert!(render("flowchart TD\n  A --> B\n  B --> A").is_none());
        assert!(render("flowchart TD\n  A --> B\n  B --> C\n  C --> A").is_none());
        // …and the acyclic near-miss still renders, or the above passes for
        // the wrong reason.
        assert!(render("flowchart TD\n  A --> B\n  A --> C\n  B --> C").is_some());
    }

    /// The layout is stable: the same source renders identically every time.
    /// A diagram that moved between renders would be its own readability
    /// problem, and it would make every assertion above flaky.
    #[test]
    fn rendering_is_deterministic() {
        let once = render(SAMPLE).expect("renders");
        for _ in 0..8 {
            assert_eq!(render(SAMPLE).expect("renders"), once);
        }
    }
}
