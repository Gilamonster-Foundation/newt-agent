//! Inline styling: the styled-cell model, ANSI (SGR) emission, and
//! whitespace-preserving word wrap.
//!
//! A rendered logical line is a `Vec<Cell>` — one `(char, Style)` per source
//! `char`. Wrapping operates on cells so a style change *inside* a word (e.g.
//! `un**bold**ed`) never inserts a spurious space, while real whitespace stays
//! a break opportunity. Every styled run is reset (`ESC[0m`) before the line's
//! newline, so a scrolled line is self-contained and copy-paste-safe — the
//! plain-scroller contract (no SGR ever crosses a line boundary).

use super::width::ch_width;
use crossterm::style::Color as CtColor;

/// SGR full reset — clears all attributes *and* color (`ESC[0m`).
pub(super) const RESET: &str = "\x1b[0m";

/// The absolute style of a single cell. Leaf-level: between a pulldown-cmark
/// `Start`/`End` pair the style is constant, so we snapshot it per `char`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Inline code / code span — rendered dim, overriding `color`.
    pub code: bool,
    pub color: Option<CtColor>,
}

/// One source `char` carrying its absolute style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Cell {
    pub ch: char,
    pub style: Style,
}

/// SGR foreground escape for a crossterm color. Only the variants the renderer
/// actually uses are encoded (the newt RGB hues); anything else is a no-op so
/// callers never have to branch.
pub(super) fn sgr_fg(c: CtColor) -> String {
    match c {
        CtColor::Rgb { r, g, b } => format!("\x1b[38;2;{r};{g};{b}m"),
        _ => String::new(),
    }
}

/// The SGR "open" sequence for a style (empty for the default style, so plain
/// text carries no escapes at all — golden-friendly and minimal).
pub(super) fn open(style: Style) -> String {
    let mut s = String::new();
    if style.bold {
        s.push_str("\x1b[1m");
    }
    if style.italic {
        s.push_str("\x1b[3m");
    }
    if style.underline {
        s.push_str("\x1b[4m");
    }
    if style.strike {
        s.push_str("\x1b[9m");
    }
    let color = if style.code {
        Some(super::emitter::FADE)
    } else {
        style.color
    };
    if let Some(c) = color {
        s.push_str(&sgr_fg(c));
    }
    s
}

/// Render one physical line of cells to an ANSI string. Consecutive cells of
/// equal style are coalesced into a single `open … RESET` run; default-styled
/// runs are emitted bare.
pub(super) fn render_cells(cells: &[Cell]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < cells.len() {
        let style = cells[i].style;
        let mut text = String::new();
        while i < cells.len() && cells[i].style == style {
            text.push(cells[i].ch);
            i += 1;
        }
        let o = open(style);
        if o.is_empty() {
            out.push_str(&text);
        } else {
            out.push_str(&o);
            out.push_str(&text);
            out.push_str(RESET);
        }
    }
    out
}

/// Split a logical line into words (maximal runs of non-space cells). Runs of
/// spaces collapse — Markdown folds inline whitespace, and wrap re-inserts a
/// single separating space between kept words.
fn words(cells: &[Cell]) -> Vec<Vec<Cell>> {
    let mut words: Vec<Vec<Cell>> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    for &c in cells {
        if c.ch == ' ' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Greedy word-wrap a logical line into physical lines that each fit `budget`
/// display columns. A word wider than `budget` overflows on its own line (the
/// terminal soft-wraps it) rather than being split mid-grapheme. Always returns
/// at least one (possibly empty) physical line.
pub(super) fn wrap_cells(cells: &[Cell], budget: usize) -> Vec<Vec<Cell>> {
    let budget = budget.max(1);
    let words = words(cells);
    if words.is_empty() {
        return vec![Vec::new()];
    }
    let space = Cell {
        ch: ' ',
        style: Style::default(),
    };
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut cur_w = 0usize;
    for w in words {
        let ww: usize = w.iter().map(|c| ch_width(c.ch)).sum();
        if cur.is_empty() {
            cur = w;
            cur_w = ww;
        } else if cur_w + 1 + ww <= budget {
            cur.push(space);
            cur.extend(w);
            cur_w += 1 + ww;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur = w;
            cur_w = ww;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}
