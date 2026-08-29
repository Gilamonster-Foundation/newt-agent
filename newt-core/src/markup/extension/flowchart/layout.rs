//! Placing the graph — pure geometry, no rendering.
//!
//! A layered layout: rank by longest path from a source, order within a rank
//! by first appearance, and space on a fixed grid. Deliberately simple and
//! deliberately not dagre: the goal is a diagram a person can read, not one
//! that matches Mermaid's output. Where the simple algorithm would produce
//! something unreadable — a cycle it cannot rank — it DECLINES, and E0a turns
//! that into the source.

use super::parse::{Direction, Graph};

/// Character width at the chosen font size, in user units. Monospace on the
/// page, so a character count is an honest width estimate rather than a guess.
const CHAR_W: f64 = 8.0;
/// Padding inside a node box.
const PAD: f64 = 12.0;
/// Node box height.
pub const NODE_H: f64 = 36.0;
/// Gap between ranks.
const RANK_GAP: f64 = 56.0;
/// Gap between nodes within a rank.
const WITHIN_GAP: f64 = 24.0;
/// Margin around the whole drawing.
const MARGIN: f64 = 12.0;

/// A placed node.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    /// Index into `Graph::nodes`.
    pub node: usize,
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Box width, sized to the label.
    pub w: f64,
    /// Box height.
    pub h: f64,
}

impl Placed {
    /// Centre point, for edge endpoints.
    #[must_use]
    pub fn centre(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// A laid-out diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// Every node, placed.
    pub placed: Vec<Placed>,
    /// Overall width.
    pub width: f64,
    /// Overall height.
    pub height: f64,
}

/// Lay `graph` out, or decline.
///
/// Declines a cyclic graph: ranking it needs a feedback-arc heuristic, and a
/// wrong guess draws edges that cross backwards through the picture. The
/// source is more useful than that.
#[must_use]
pub fn layout(graph: &Graph) -> Option<Layout> {
    let ranks = rank(graph)?;
    let deepest = ranks.iter().copied().max().unwrap_or(0);

    // Group by rank, preserving first-seen order within each.
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); deepest + 1];
    for (node, rank) in ranks.iter().copied().enumerate() {
        rows[rank].push(node);
    }

    let width_of =
        |node: usize| graph.nodes[node].label.chars().count() as f64 * CHAR_W + PAD * 2.0;

    // Extent of each rank ALONG the rank, so ranks can be centred against each
    // other and the drawing has no ragged edge.
    let along: Vec<f64> = rows
        .iter()
        .map(|row| {
            let boxes: f64 = match graph.direction {
                Direction::Down => row.iter().map(|n| width_of(*n)).sum(),
                Direction::Right => row.len() as f64 * NODE_H,
            };
            boxes + WITHIN_GAP * (row.len().saturating_sub(1)) as f64
        })
        .collect();
    let widest = along.iter().copied().fold(0.0_f64, f64::max);

    let mut placed = Vec::new();
    let mut across = MARGIN;
    for (rank, row) in rows.iter().enumerate() {
        let mut cursor = MARGIN + (widest - along[rank]) / 2.0;
        let mut deepest_in_rank = 0.0_f64;
        for node in row.iter().copied() {
            let w = width_of(node);
            let (x, y, bw, bh) = match graph.direction {
                Direction::Down => (cursor, across, w, NODE_H),
                Direction::Right => (across, cursor, w, NODE_H),
            };
            placed.push(Placed {
                node,
                x,
                y,
                w: bw,
                h: bh,
            });
            match graph.direction {
                Direction::Down => {
                    cursor += bw + WITHIN_GAP;
                    deepest_in_rank = deepest_in_rank.max(bh);
                }
                Direction::Right => {
                    cursor += bh + WITHIN_GAP;
                    deepest_in_rank = deepest_in_rank.max(bw);
                }
            }
        }
        across += deepest_in_rank + RANK_GAP;
    }
    placed.sort_by_key(|p| p.node);

    let right = placed.iter().map(|p| p.x + p.w).fold(0.0_f64, f64::max);
    let bottom = placed.iter().map(|p| p.y + p.h).fold(0.0_f64, f64::max);
    Some(Layout {
        placed,
        width: right + MARGIN,
        height: bottom + MARGIN,
    })
}

/// Longest-path rank per node, or `None` if the graph has a cycle.
fn rank(graph: &Graph) -> Option<Vec<usize>> {
    let n = graph.nodes.len();
    let mut incoming = vec![0usize; n];
    for edge in &graph.edges {
        incoming[edge.to] += 1;
    }
    // Kahn's algorithm, taking ready nodes in first-seen order so the layout
    // is stable across runs — a diagram that moved between renders would be
    // its own readability problem.
    let mut ready: Vec<usize> = (0..n).filter(|i| incoming[*i] == 0).collect();
    let mut rank = vec![0usize; n];
    let mut settled = 0usize;
    while let Some(node) = ready.first().copied() {
        ready.remove(0);
        settled += 1;
        for edge in graph.edges.iter().filter(|e| e.from == node) {
            rank[edge.to] = rank[edge.to].max(rank[node] + 1);
            incoming[edge.to] -= 1;
            if incoming[edge.to] == 0 {
                ready.push(edge.to);
            }
        }
    }
    (settled == n).then_some(rank)
}
