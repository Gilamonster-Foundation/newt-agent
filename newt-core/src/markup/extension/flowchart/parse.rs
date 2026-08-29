//! Parsing the flowchart subset — text in, model out, nothing executed.
//!
//! A deliberately SMALL subset: `flowchart`/`graph` with a direction, node
//! declarations, and edges. Anything else returns `None`, which E0a's contract
//! turns into the source fallback. Declining is a first-class answer here, so
//! the parser never guesses at syntax it does not know — a half-understood
//! diagram drawn confidently is the failure mode this whole epic is about.

/// Which way the graph flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Top to bottom.
    Down,
    /// Left to right.
    Right,
}

/// One node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The identifier the source used.
    pub id: String,
    /// The text to draw. Defaults to the id when undeclared.
    pub label: String,
}

/// One directed edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// Index into [`Graph::nodes`].
    pub from: usize,
    /// Index into [`Graph::nodes`].
    pub to: usize,
    /// The edge's own label, if the source gave one.
    pub label: Option<String>,
}

/// A parsed flowchart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Graph {
    /// Flow direction.
    pub direction: Direction,
    /// Nodes in first-seen order, which is also their layout tie-break.
    pub nodes: Vec<Node>,
    /// Edges in source order.
    pub edges: Vec<Edge>,
}

/// The arrow forms this subset understands, longest first so `-->` is not read
/// as `--`. Kept in sync with `super::super::mermaid::EDGES` by
/// `the_parser_and_the_scan_agree_on_what_an_edge_is`.
const ARROWS: &[&str] = &["-->", "---", "==>", "-.->"];

/// Parse `source`, or decline.
///
/// Returns `None` for anything outside the subset — an unknown header, an
/// unknown arrow, a line that parses to nothing. `None` is not an error; it is
/// the contract's way of saying "show the source", and it is why this parser
/// can afford to be strict.
#[must_use]
pub fn parse(source: &str) -> Option<Graph> {
    let mut lines = source
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));

    let direction = header(lines.next()?)?;
    let mut graph = Graph {
        direction,
        nodes: Vec::new(),
        edges: Vec::new(),
    };

    for line in lines {
        // `subgraph`, `click`, `style`, `classDef` and friends are outside the
        // subset. Declining the whole diagram is deliberate: rendering the
        // parts we understood and silently dropping a `subgraph` would draw a
        // picture that is not what the author wrote.
        if !parse_line(line, &mut graph) {
            return None;
        }
    }
    (!graph.nodes.is_empty()).then_some(graph)
}

/// `flowchart TD` / `graph LR` and the aliases. Anything else declines.
fn header(line: &str) -> Option<Direction> {
    let mut parts = line.split_whitespace();
    let kind = parts.next()?;
    if !matches!(kind, "flowchart" | "graph") {
        return None;
    }
    match parts.next()? {
        "TD" | "TB" => Some(Direction::Down),
        "LR" => Some(Direction::Right),
        _ => None,
    }
}

/// One statement. `false` means "outside the subset", which declines the whole
/// diagram.
fn parse_line(line: &str, graph: &mut Graph) -> bool {
    let Some((arrow, at)) = ARROWS
        .iter()
        .filter_map(|a| line.find(a).map(|i| (*a, i)))
        .min_by_key(|(_, i)| *i)
    else {
        // No arrow: a bare node declaration, `A[label]`.
        return match node_decl(line) {
            Some((id, label)) => {
                intern(graph, &id, label);
                true
            }
            None => false,
        };
    };

    let (left, rest) = line.split_at(at);
    let rest = &rest[arrow.len()..];
    // `A -->|label| B`
    let (edge_label, right) = match rest.trim_start().strip_prefix('|') {
        Some(tail) => match tail.split_once('|') {
            Some((label, right)) => (Some(label.trim().to_string()), right),
            None => return false,
        },
        None => (None, rest),
    };

    let (Some((lid, llab)), Some((rid, rlab))) = (node_decl(left.trim()), node_decl(right.trim()))
    else {
        return false;
    };
    let from = intern(graph, &lid, llab);
    let to = intern(graph, &rid, rlab);
    graph.edges.push(Edge {
        from,
        to,
        label: edge_label,
    });
    true
}

/// `A`, `A[text]`, `A(text)`, `A{text}` → (id, optional label).
fn node_decl(text: &str) -> Option<(String, Option<String>)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    for (open, close) in [('[', ']'), ('(', ')'), ('{', '}')] {
        if let Some(i) = text.find(open) {
            let id = text[..i].trim();
            let rest = &text[i + 1..];
            let label = rest.strip_suffix(close)?;
            return valid_id(id).then(|| (id.to_string(), Some(label.trim().to_string())));
        }
    }
    valid_id(text).then(|| (text.to_string(), None))
}

/// Identifiers are alphanumerics, `_` and `-`. Anything else declines rather
/// than being sanitized into something that renders — the source is a better
/// answer than a guess.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Find or add a node, upgrading its label if this mention declared one.
fn intern(graph: &mut Graph, id: &str, label: Option<String>) -> usize {
    let at = graph
        .nodes
        .iter()
        .position(|n| n.id == id)
        .unwrap_or_else(|| {
            graph.nodes.push(Node {
                id: id.to_string(),
                label: id.to_string(),
            });
            graph.nodes.len() - 1
        });
    if let Some(label) = label {
        if !label.is_empty() {
            graph.nodes[at].label = label;
        }
    }
    at
}
