//! **The ONE table algorithm** — tabular data in, a GFM pipe table out
//! (D3a of epic #1803, #1874).
//!
//! ## Why a Markdown table, and no node types
//!
//! A table is a *document*. D3's gate is "one table algorithm and no
//! surface-specific duplicate data model", and the epic names the failure
//! mode for this slice outright: do not mirror Markdown nodes into a custom
//! wire AST. So this module defines none. Its input is what every caller
//! already holds — rows of strings — and its output is GFM source, which A1's
//! dialect, the plain tier, and every Markdown reader already render. [`Align`]
//! is not a node: it is GFM's own three delimiter forms, which a pipe table
//! cannot be emitted without.
//!
//! ## Why it lives in `markup`, unconditionally
//!
//! `plain`'s reason (C0a, #1856): the headless wyvern tier keeps Markdown as
//! source and still prints tables, so this must survive
//! `--no-default-features`. It therefore takes no `pulldown-cmark`, no
//! `markdown` feature and no ANSI. `agentic::markdown::table` stays what it
//! is — the color-on box-drawing *renderer* of already-parsed GFM, one layer
//! outward. This module is the thing that *produces* the GFM.
//!
//! ## One width model
//!
//! Column widths, padding and truncation measure in display CELLS, through
//! [`crate::tty::width`] — the module that already carries the title. A char
//! count and a byte count both get this wrong: `日本` is 2 chars, 6 bytes and
//! **4 cells**, and the A0 inventory found the workspace sizing columns by all
//! three (§4.2.13 sizes by bytes, §4.1.5 pads by chars).

use crate::tty::width::{ch_width, str_width};
use std::fmt::Write as _;

/// GFM's three delimiter forms. Left is spelled `---` rather than `:--`
/// because an unmarked column already left-aligns everywhere, and the
/// unmarked form is what hand-written tables use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// One column's presentation.
#[derive(Clone, Debug, Default)]
pub struct Column {
    /// Header cell text.
    pub header: String,
    pub align: Align,
    /// Cap on the column's content in display cells; `None` fits content.
    /// A capped cell is truncated with a trailing `…`.
    pub max_width: Option<usize>,
}

impl Column {
    /// A left-aligned, uncapped column.
    pub fn new(header: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            align: Align::Left,
            max_width: None,
        }
    }

    #[must_use]
    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    #[must_use]
    pub fn max_width(mut self, cells: usize) -> Self {
        self.max_width = Some(cells);
        self
    }
}

/// GFM needs at least three dashes in a delimiter cell, so no column is
/// rendered narrower than that however short its content is.
const MIN_COL: usize = 3;

/// Make one cell safe to sit between pipes: an unescaped `|` would end the
/// cell early and silently shift every column after it, and a newline would
/// end the ROW. Both are reachable from ordinary content — an evaluator
/// detail string carrying a shell pipeline is the case that motivated this.
fn escape_cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '|' => out.push_str("\\|"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Truncate to `max` display cells, appending `…` (one cell) when cut.
/// Cell-wise, so a CJK cap does not overshoot and a multi-byte codepoint is
/// never split mid-sequence.
fn truncate_cells(s: &str, max: usize) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let cap = max - 1;
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = ch_width(c);
        if w + cw > cap {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Escape, then cap. In that order: escaping can only widen a cell (`|` →
/// `\|`), so capping first would let the escape push it back over the cap.
fn prepare(s: &str, max: Option<usize>) -> String {
    let escaped = escape_cell(s);
    match max {
        Some(m) => truncate_cells(&escaped, m),
        None => escaped,
    }
}

/// Pad `cell` to `width` cells under `align`.
fn pad(cell: &str, width: usize, align: Align) -> String {
    let slack = width.saturating_sub(str_width(cell));
    let (l, r) = match align {
        Align::Left => (0, slack),
        Align::Right => (slack, 0),
        Align::Center => (slack / 2, slack - slack / 2),
    };
    format!("{}{cell}{}", " ".repeat(l), " ".repeat(r))
}

/// One `| a | b |` line.
fn row_line(cells: &[String], widths: &[usize], aligns: &[Align]) -> String {
    let mut s = String::from("|");
    for (i, w) in widths.iter().enumerate() {
        let empty = String::new();
        let cell = cells.get(i).unwrap_or(&empty);
        let _ = write!(s, " {} |", pad(cell, *w, aligns[i]));
    }
    s
}

/// The `| --- | ---: |` delimiter line, which is what carries alignment in
/// GFM — the padding above is for human eyes only.
fn delimiter_line(widths: &[usize], aligns: &[Align]) -> String {
    let mut s = String::from("|");
    for (i, w) in widths.iter().enumerate() {
        let bar = match aligns[i] {
            Align::Left => "-".repeat(*w),
            Align::Right => format!("{}:", "-".repeat(w - 1)),
            Align::Center => format!(":{}:", "-".repeat(w - 2)),
        };
        let _ = write!(s, " {bar} |");
    }
    s
}

/// Render `rows` as a GFM pipe table under `columns`.
///
/// Cells are matched to columns positionally; a short row is padded with
/// empty cells and any cell beyond the last column is dropped, so a ragged
/// input can never emit a ragged table. Returns the empty string when there
/// are no columns — a table with no columns is not a document, and callers
/// with nothing to show say so in their own words.
pub fn render_table(columns: &[Column], rows: &[Vec<String>]) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let n = columns.len();
    let header: Vec<String> = columns
        .iter()
        .map(|c| prepare(&c.header, c.max_width))
        .collect();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            (0..n)
                .map(|i| prepare(r.get(i).map_or("", String::as_str), columns[i].max_width))
                .collect()
        })
        .collect();

    let aligns: Vec<Align> = columns.iter().map(|c| c.align).collect();
    let widths: Vec<usize> = (0..n)
        .map(|i| {
            body.iter()
                .map(|r| str_width(&r[i]))
                .chain(std::iter::once(str_width(&header[i])))
                .max()
                .unwrap_or(0)
                .max(MIN_COL)
        })
        .collect();

    let mut out = String::new();
    let _ = writeln!(out, "{}", row_line(&header, &widths, &aligns));
    let _ = writeln!(out, "{}", delimiter_line(&widths, &aligns));
    for r in &body {
        let _ = writeln!(out, "{}", row_line(r, &widths, &aligns));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(r: &[&[&str]]) -> Vec<Vec<String>> {
        r.iter()
            .map(|row| row.iter().map(|c| (*c).to_string()).collect())
            .collect()
    }

    /// The byte golden. Padding is for human eyes; the delimiter row is what
    /// carries alignment to a Markdown reader.
    #[test]
    fn a_plain_table_is_byte_exact_gfm() {
        let cols = [
            Column::new("case"),
            Column::new("score").align(Align::Right),
        ];
        let got = render_table(&cols, &rows(&[&["a", "1.00"], &["bbbb", "0.50"]]));
        assert_eq!(
            got,
            "\
| case | score |
| ---- | ----: |
| a    |  1.00 |
| bbbb |  0.50 |
"
        );
    }

    /// **The anti-vacuous twin for the width model.** Swap `str_width` for a
    /// `chars().count()` anywhere in this module and this is the test that
    /// fails: `名前` and `日本` are 2 chars but 4 cells, so a char-sized
    /// column pads them to a ragged 5-cell row while the ASCII row stays at
    /// 3. Every other test here passes under that mutation.
    #[test]
    fn cjk_columns_align_in_cells_not_chars() {
        let cols = [Column::new("名前")];
        let got = render_table(&cols, &rows(&[&["日本"], &["a"]]));
        assert_eq!(
            got,
            "\
| 名前 |
| ---- |
| 日本 |
| a    |
"
        );
    }

    /// An unescaped `|` would end the cell early and shift every column after
    /// it — a silently WRONG table, not a broken one. Reachable from ordinary
    /// content: an evaluator detail carrying a shell pipeline.
    #[test]
    fn a_pipe_in_a_cell_cannot_break_the_row() {
        let cols = [Column::new("cmd")];
        let got = render_table(&cols, &rows(&[&["a | b"]]));
        assert_eq!(
            got,
            "\
| cmd    |
| ------ |
| a \\| b |
"
        );
        // One data row plus header and delimiter — the pipe did not split it.
        assert_eq!(got.lines().count(), 3);
    }

    /// A newline would end the ROW, not just the cell.
    #[test]
    fn a_newline_in_a_cell_cannot_break_the_table() {
        let cols = [Column::new("d")];
        let got = render_table(&cols, &rows(&[&["one\ntwo"]]));
        assert_eq!(got.lines().count(), 3, "still exactly three lines: {got:?}");
        assert!(got.contains("| one two |"));
    }

    /// A cap counts cells, so a CJK cell is cut on a cell boundary and never
    /// mid-codepoint. `日本語` is 6 cells; capped at 3 it keeps one ideograph
    /// and the one-cell ellipsis.
    #[test]
    fn a_capped_cell_truncates_on_a_cell_boundary() {
        let cols = [Column::new("x").max_width(3)];
        let got = render_table(&cols, &rows(&[&["日本語"]]));
        assert_eq!(
            got,
            "\
| x   |
| --- |
| 日… |
"
        );
    }

    /// Alignment travels in the delimiter row — that is the only place a
    /// Markdown reader looks for it.
    #[test]
    fn alignment_travels_in_the_delimiter_row() {
        let cols = [
            Column::new("a"),
            Column::new("b").align(Align::Center),
            Column::new("c").align(Align::Right),
        ];
        let got = render_table(&cols, &[]);
        assert_eq!(
            got,
            "\
| a   |  b  |   c |
| --- | :-: | --: |
"
        );
    }

    /// A short row is padded and an overlong one is clipped, so ragged input
    /// cannot emit a ragged table.
    #[test]
    fn a_ragged_row_cannot_emit_a_ragged_table() {
        let cols = [Column::new("h1"), Column::new("h2")];
        let got = render_table(&cols, &rows(&[&["a"], &["a", "b", "c"]]));
        assert_eq!(
            got,
            "\
| h1  | h2  |
| --- | --- |
| a   |     |
| a   | b   |
"
        );
    }

    /// No columns is not a document. The caller says what it means.
    #[test]
    fn no_columns_renders_nothing() {
        assert_eq!(render_table(&[], &rows(&[&["a"]])), "");
    }
}
