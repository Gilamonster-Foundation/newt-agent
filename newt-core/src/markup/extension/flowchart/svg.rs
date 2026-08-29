//! Emitting SVG — presentation attributes only, and no CSS anywhere.
//!
//! ## Why this has no CSP surface at all
//!
//! C3b measured the browser path: Mermaid themes its output with a `<style>`
//! element scoped to a per-render id, which a strict `style-src-elem` blocks,
//! and 49 per-node `style=` attributes, which `style-src-attr` governs. Both
//! are CSS.
//!
//! Nothing here emits CSS. `fill`, `stroke`, `stroke-width`, `font-size` and
//! `text-anchor` are SVG **presentation attributes** — XML attributes with
//! their own defaulting rules, not the `style` content attribute — so no
//! `style-src*` directive applies to them. A diagram drawn this way needs no
//! nonce, no hash, and no relaxation.
//!
//! ## Why it cannot render black-on-black
//!
//! Every stroke and every glyph is `currentColor`, and node interiors are
//! `fill="none"`. The diagram therefore inherits the page's own foreground
//! colour on the page's own background, in whichever theme is active. C3b's
//! failure was a themed diagram whose theme was blocked, leaving both fill and
//! text at the UA default black; there is no theme here to block, and no
//! colour that can disagree with the page.
//!
//! `readability::contrast_is_the_pages_own` holds that, and it is a stronger
//! claim than "a diagram is present" — which is exactly the assertion that let
//! the black-on-black regression through.

use super::layout::{Layout, NODE_H};
use super::parse::{Direction, Graph};
use std::fmt::Write as _;

/// The colour every stroke and glyph uses.
pub const INK: &str = "currentColor";

/// Escape text for an XML text node or attribute value.
///
/// Applied to every author-derived string without exception. The sanitizer on
/// the web side is the backstop, not the plan: a renderer that emitted
/// unescaped author text and relied on someone downstream to fix it would be
/// the "trusted because we produced it" mistake E0a's contract names.
fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The adjacent text a non-visual reader is given.
///
/// Required by E0a's `Enhancement::graphics`, and built from the same model
/// the picture is, so the two cannot describe different diagrams.
#[must_use]
pub fn accessible_text(graph: &Graph) -> String {
    let mut out = String::from("Flowchart. ");
    for edge in &graph.edges {
        let from = &graph.nodes[edge.from].label;
        let to = &graph.nodes[edge.to].label;
        match &edge.label {
            Some(label) => {
                let _ = write!(out, "{from} {label} {to}. ");
            }
            None => {
                let _ = write!(out, "{from} to {to}. ");
            }
        }
    }
    if graph.edges.is_empty() {
        let names: Vec<&str> = graph.nodes.iter().map(|n| n.label.as_str()).collect();
        let _ = write!(out, "Nodes: {}.", names.join(", "));
    }
    out.trim_end().to_string()
}

/// Render `graph` at `layout` to SVG.
#[must_use]
pub fn render(graph: &Graph, layout: &Layout) -> String {
    let mut out = String::new();
    let alt = xml(&accessible_text(graph));
    let _ = write!(
        out,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w:.0} {h:.0}" width="{w:.0}" height="{h:.0}" role="img" aria-label="{alt}"><title>{alt}</title>"#,
        w = layout.width,
        h = layout.height,
    );

    // Edges first, so a box is never drawn over by its own connector.
    for edge in &graph.edges {
        let from = &layout.placed[edge.from];
        let to = &layout.placed[edge.to];
        let (x1, y1, x2, y2) = endpoints(graph.direction, from, to);
        let _ = write!(
            out,
            r#"<line x1="{x1:.1}" y1="{y1:.1}" x2="{x2:.1}" y2="{y2:.1}" stroke="{INK}" stroke-width="1.5"/>"#
        );
        arrowhead(&mut out, graph.direction, x2, y2);
        if let Some(label) = &edge.label {
            let _ = write!(
                out,
                r#"<text x="{x:.1}" y="{y:.1}" fill="{INK}" font-size="11" text-anchor="middle">{}</text>"#,
                xml(label),
                x = f64::midpoint(x1, x2),
                y = f64::midpoint(y1, y2) - 4.0,
            );
        }
    }

    for placed in &layout.placed {
        let label = xml(&graph.nodes[placed.node].label);
        let (cx, cy) = placed.centre();
        let _ = write!(
            out,
            r#"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="4" fill="none" stroke="{INK}" stroke-width="1.5"/>"#,
            x = placed.x,
            y = placed.y,
            w = placed.w,
            h = placed.h,
        );
        let _ = write!(
            out,
            r#"<text x="{cx:.1}" y="{y:.1}" fill="{INK}" font-size="13" text-anchor="middle">{label}</text>"#,
            y = cy + 4.5,
        );
    }
    out.push_str("</svg>");
    out
}

/// Where an edge leaves one box and meets the next, on the flow axis.
fn endpoints(
    direction: Direction,
    from: &super::layout::Placed,
    to: &super::layout::Placed,
) -> (f64, f64, f64, f64) {
    let (fx, fy) = from.centre();
    let (tx, ty) = to.centre();
    match direction {
        Direction::Down => (fx, from.y + from.h, tx, to.y),
        Direction::Right => (from.x + from.w, fy, to.x, ty),
    }
}

/// A filled triangle at the edge's head. `fill` is a presentation attribute,
/// so this is still CSS-free.
fn arrowhead(out: &mut String, direction: Direction, x: f64, y: f64) {
    const S: f64 = 5.0;
    let points = match direction {
        Direction::Down => format!(
            "{x:.1},{y:.1} {:.1},{:.1} {:.1},{:.1}",
            x - S,
            y - S,
            x + S,
            y - S
        ),
        Direction::Right => format!(
            "{x:.1},{y:.1} {:.1},{:.1} {:.1},{:.1}",
            x - S,
            y - S,
            x - S,
            y + S
        ),
    };
    let _ = write!(out, r#"<polygon points="{points}" fill="{INK}"/>"#);
    let _ = NODE_H;
}
