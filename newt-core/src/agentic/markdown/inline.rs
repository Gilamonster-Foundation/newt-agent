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
    /// Inline code / file path — uses its own theme role.
    pub code: bool,
    pub color: Option<CtColor>,
    pub role: Option<crate::tty::theme::Role>,
}

/// One source `char` carrying its absolute style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Cell {
    pub ch: char,
    pub style: Style,
}

/// The SGR "open" sequence for a style (empty for the default style, so plain
/// text carries no escapes at all — golden-friendly and minimal).
pub(super) fn open(style: Style) -> String {
    open_with_theme(style, &crate::tty::theme::active())
}

fn open_with_theme(style: Style, theme: &crate::tty::theme::Theme) -> String {
    use crate::tty::theme::Role;
    use crossterm::style::{Attribute, ContentStyle};
    let role = if style.code {
        Some(Role::InlineCode)
    } else {
        style.role
    };
    let mut resolved = role.map_or_else(ContentStyle::default, |r| theme.style(r));
    if style.bold
        && theme
            .style(Role::MarkdownStrong)
            .attributes
            .has(Attribute::Bold)
    {
        resolved.attributes.set(Attribute::Bold);
    }
    if style.italic
        && theme
            .style(Role::MarkdownItalic)
            .attributes
            .has(Attribute::Italic)
    {
        resolved.attributes.set(Attribute::Italic);
    }
    if style.underline
        && theme
            .style(Role::MarkdownLink)
            .attributes
            .has(Attribute::Underlined)
    {
        resolved.attributes.set(Attribute::Underlined);
    }
    if style.strike
        && theme
            .style(Role::MarkdownStrike)
            .attributes
            .has(Attribute::CrossedOut)
    {
        resolved.attributes.set(Attribute::CrossedOut);
    }
    if !style.code {
        if let Some(c) = style.color {
            resolved.foreground_color = Some(c);
        }
    }
    crate::tty::theme::ansi_style(resolved)
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
    let mut space = Cell {
        ch: ' ',
        style: Style::default(),
    };
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    let mut cur: Vec<Cell> = Vec::new();
    let mut cur_w = 0usize;
    for w in words {
        space.style = w.first().map(|c| c.style).unwrap_or_default();
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

#[cfg(test)]
mod theme_tests {
    use super::*;
    use crate::tty::theme::{Role, Theme};
    use crossterm::style::Attribute;

    #[test]
    fn heading_and_code_obey_explicit_bold_off_and_custom_color() {
        let mut theme = Theme::builtin();
        for role in [
            Role::MarkdownHeading,
            Role::InlineCode,
            Role::MarkdownStrong,
            Role::MarkdownLink,
            Role::MarkdownStrike,
        ] {
            let mut style = theme.style(role);
            style.attributes.unset(Attribute::Bold);
            style.attributes.set(Attribute::Italic);
            style.foreground_color = Some(CtColor::Rgb {
                r: 12,
                g: 34,
                b: 56,
            });
            theme.set_style(role, style);
            let open = open_with_theme(
                Style {
                    role: Some(role),
                    code: role == Role::InlineCode,
                    bold: role == Role::MarkdownStrong,
                    ..Style::default()
                },
                &theme,
            );
            assert!(!open.contains("\x1b[1m"));
            assert!(open.contains("\x1b[3m"));
            assert!(open.contains("\x1b[38;2;12;34;56m"));
        }
    }
}
