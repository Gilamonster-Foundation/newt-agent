//! GFM table rendering (Step 25.2): box-drawing layout with display-width
//! column fit and per-column alignment.
//!
//! Borders are dim (`FADE`); header cells are bold. Column widths are the max
//! *display* width of their cells (CJK/emoji counted correctly via
//! `unicode-width`), shrunk to fit the wrap budget by shaving the widest column
//! first, with overflowing cells truncated to a trailing `…`. Like the rest of
//! the renderer this runs color-on only (color-off returns source verbatim a
//! layer up), so there is no ASCII-border fallback path here.

use super::emitter::FADE;
use super::inline::{render_cells, sgr_fg, Cell, Style, RESET};
use super::width::ch_width;
use pulldown_cmark::Alignment;

/// Accumulates a table's cells as the parser walks it; rendered on `End(Table)`.
pub(super) struct TableBuilder {
    pub aligns: Vec<Alignment>,
    pub header: Vec<Vec<Cell>>,
    pub rows: Vec<Vec<Vec<Cell>>>,
    /// Cells of the row currently being parsed.
    pub cur_row: Vec<Vec<Cell>>,
}

impl TableBuilder {
    pub(super) fn new(aligns: Vec<Alignment>) -> Self {
        Self {
            aligns,
            header: Vec::new(),
            rows: Vec::new(),
            cur_row: Vec::new(),
        }
    }
}

/// Display width of a cell's content.
fn vis_width(cells: &[Cell]) -> usize {
    cells.iter().map(|c| ch_width(c.ch)).sum()
}

/// Wrap a border fragment in the dim hue.
fn dim(s: &str) -> String {
    format!("{}{s}{RESET}", sgr_fg(FADE))
}

/// Truncate cells to `width` display columns, appending `…` when cut. Returns
/// the (possibly shortened) cells and their resulting visible width.
fn truncate(cells: &[Cell], width: usize) -> (Vec<Cell>, usize) {
    let total = vis_width(cells);
    if total <= width {
        return (cells.to_vec(), total);
    }
    let cap = width.saturating_sub(1);
    let mut out = Vec::new();
    let mut w = 0;
    for &c in cells {
        let cw = ch_width(c.ch);
        if w + cw > cap {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push(Cell {
        ch: '…',
        style: Style::default(),
    });
    (out, w + 1)
}

/// Render one cell's padded, aligned, styled content (no surrounding borders).
fn render_cell(cells: &[Cell], width: usize, align: Alignment, header: bool) -> String {
    let (mut content, vis) = truncate(cells, width);
    if header {
        for c in &mut content {
            c.style.bold = true;
        }
    }
    let pad = width.saturating_sub(vis);
    let (lp, rp) = match align {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        Alignment::Left | Alignment::None => (0, pad),
    };
    format!(
        "{}{}{}",
        " ".repeat(lp),
        render_cells(&content),
        " ".repeat(rp)
    )
}

/// Render one full row: `│ c1 │ c2 │` with dim borders and plain cell padding.
fn render_row(row: &[Vec<Cell>], widths: &[usize], aligns: &[Alignment], header: bool) -> String {
    let empty: Vec<Cell> = Vec::new();
    let mut s = String::new();
    for (i, &w) in widths.iter().enumerate() {
        s.push_str(&dim("│"));
        s.push(' ');
        let cells = row.get(i).unwrap_or(&empty);
        let align = aligns.get(i).copied().unwrap_or(Alignment::None);
        s.push_str(&render_cell(cells, w, align, header));
        s.push(' ');
    }
    s.push_str(&dim("│"));
    s
}

/// Shrink columns to fit `avail` total content columns by shaving the widest
/// column (above a 1-col floor) until it fits or no column can shrink further.
fn shrink(natural: &[usize], avail: usize) -> Vec<usize> {
    let mut widths = natural.to_vec();
    let mut sum: usize = widths.iter().sum();
    let min = 1usize;
    while sum > avail {
        let target = widths
            .iter()
            .enumerate()
            .filter(|(_, &w)| w > min)
            .max_by_key(|(_, &w)| w)
            .map(|(i, _)| i);
        match target {
            Some(i) => {
                widths[i] -= 1;
                sum -= 1;
            }
            None => break,
        }
    }
    widths
}

/// Render a complete table to styled lines (no container prefix — the caller
/// prepends blockquote bars / list indent). `budget` is the display width
/// available for the whole table.
pub(super) fn render_table(
    aligns: &[Alignment],
    header: &[Vec<Cell>],
    rows: &[Vec<Vec<Cell>>],
    budget: usize,
) -> Vec<String> {
    let ncols = aligns
        .len()
        .max(header.len())
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if ncols == 0 {
        return Vec::new();
    }

    // Natural (unconstrained) column widths.
    let mut natural = vec![0usize; ncols];
    for row in std::iter::once(header).chain(rows.iter().map(Vec::as_slice)) {
        for (i, cell) in row.iter().enumerate() {
            if i < ncols {
                natural[i] = natural[i].max(vis_width(cell));
            }
        }
    }

    // Overhead = one border per gap/edge (ncols+1) + 2 pad spaces per column.
    let overhead = 3 * ncols + 1;
    let avail = budget.saturating_sub(overhead).max(ncols);
    let widths = shrink(&natural, avail);

    let rule = |left: &str, mid: &str, right: &str| -> String {
        let mut s = String::from(left);
        for (i, &w) in widths.iter().enumerate() {
            s.push_str(&"─".repeat(w + 2));
            s.push_str(if i + 1 < widths.len() { mid } else { right });
        }
        dim(&s)
    };

    let mut out = Vec::with_capacity(rows.len() + 4);
    out.push(rule("┌", "┬", "┐"));
    out.push(render_row(header, &widths, aligns, true));
    out.push(rule("├", "┼", "┤"));
    for r in rows {
        out.push(render_row(r, &widths, aligns, false));
    }
    out.push(rule("└", "┴", "┘"));
    out
}
