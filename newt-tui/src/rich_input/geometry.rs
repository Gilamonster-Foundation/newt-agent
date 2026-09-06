//! Input wrapping, visual rows and cursor projection for the rich editor.

use newt_core::tty::str_width;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use tui_textarea::TextArea;

/// Soft-wrap one logical line into visual segments that fit the input width.
/// `first_w` is the available text width for the first segment (after the prompt
/// or gutter indent), `cont_w` for each wrapped continuation. Breaks at the last
/// space within the width (word wrap), falling back to a hard mid-token break so
/// an unbreakable run still fits. Every char belongs to exactly one segment (the
/// breaking space ends its segment, nothing is dropped) so the cursor maps back
/// cleanly. Each entry is `(char_index_where_the_segment_starts, segment_text)`;
/// there is always at least one segment (empty for an empty line).
pub(super) fn wrap_segments(text: &str, first_w: usize, cont_w: usize) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![(0, String::new())];
    }
    let mut segs: Vec<(usize, String)> = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let avail = if segs.is_empty() { first_w } else { cont_w }.max(1);
        let mut hard_end = start;
        while hard_end < chars.len() {
            // Width is contextual for emoji presentation selectors and ZWJ
            // sequences, so summing scalar widths is not equivalent to the
            // terminal width of the candidate substring.
            let candidate: String = chars[start..=hard_end].iter().collect();
            if str_width(&candidate) > avail && hard_end > start {
                break;
            }
            // Always take at least one scalar so a glyph wider than a
            // one-column budget cannot stall the wrapper. Zero-width combining
            // marks naturally stay with the preceding cell.
            hard_end += 1;
        }
        if hard_end == chars.len() {
            segs.push((start, chars[start..].iter().collect()));
            break;
        }
        // Prefer breaking just after the last space in range (word wrap); else
        // hard-break at the cell budget. Force ≥1 char of progress.
        let break_at = (start..hard_end)
            .rev()
            .find(|&i| chars[i] == ' ')
            .map(|i| i + 1)
            .unwrap_or(hard_end)
            .max(start + 1);
        segs.push((start, chars[start..break_at].iter().collect()));
        start = break_at;
    }
    segs
}

/// Build the wrapped visual rows for the overhang surface, plus the cursor's
/// position in full-row space `(col, row)`. Row 0 is prefixed by `prompt`;
/// wrapped continuations and later logical lines hang-indent by `g`. Pure
/// (no `Frame`) so the wrap + cursor math is unit-tested; `draw_overhang`
/// renders it (with vertical scroll) and `event_loop` uses the row count to
/// size the inline viewport.
pub(super) fn overhang_rows(
    prompt: &Line<'static>,
    lines: &[String],
    cursor: (usize, usize),
    g: u16,
    width: u16,
    hint: Option<&str>,
) -> (Vec<Line<'static>>, u16, u16) {
    let pw = prompt.width() as u16;
    let (crow, ccol) = cursor;
    let cont_w = width.saturating_sub(g).max(1) as usize;
    let mut rows: Vec<Line> = Vec::new();
    let (mut cx, mut cy) = (pw, 0u16);
    for (i, text) in lines.iter().enumerate() {
        let first_indent = if i == 0 { pw } else { g };
        let first_w = width.saturating_sub(first_indent).max(1) as usize;
        let segs = wrap_segments(text, first_w, cont_w);
        let n = segs.len();
        for (s, (seg_start, seg_text)) in segs.into_iter().enumerate() {
            let indent = if s == 0 { first_indent } else { g };
            let line = if i == 0 && s == 0 {
                let mut spans = prompt.spans.clone();
                spans.push(Span::raw(seg_text.clone()));
                // The dim mode hint sits AFTER the input on the first row (the
                // line is empty when a hint is shown), so the block cursor —
                // anchored at the prompt end — lands ON the hint's first cell,
                // placeholder-style, instead of after the whole hint.
                if let Some(h) = hint {
                    spans.push(Span::styled(
                        h.to_string(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                Line::from(spans)
            } else {
                Line::from(vec![
                    Span::raw(" ".repeat(indent as usize)),
                    Span::raw(seg_text.clone()),
                ])
            };
            if i == crow {
                let seg_end = seg_start + seg_text.chars().count();
                let last = s + 1 == n;
                if (ccol >= seg_start && ccol < seg_end) || (last && ccol >= seg_end) {
                    cy = rows.len() as u16;
                    let cursor_chars = ccol.saturating_sub(seg_start);
                    let cursor_prefix: String = seg_text.chars().take(cursor_chars).collect();
                    let cursor_cells = str_width(&cursor_prefix);
                    cx = indent + cursor_cells as u16;
                }
            }
            rows.push(line);
        }
    }
    (rows, cx, cy)
}

/// Render the prompt as an inline prefix on row 0, the input flowing after it
/// and **soft-wrapping** within the terminal width; continuation/wrapped rows
/// hang-indent by `g`. tui-textarea can't vary indent per row or soft-wrap, so
/// we render the buffer ourselves as a `Paragraph` and place the block cursor by
/// hand. A vertical scroll keeps the cursor row visible when the wrapped input
/// is taller than the inline region.
pub(super) fn draw_overhang(
    f: &mut Frame,
    area: Rect,
    prompt: &Line<'static>,
    textarea: &TextArea,
    g: u16,
    hint: Option<&str>,
    show_cursor: bool,
) {
    let (rows, cx, cy) = overhang_rows(
        prompt,
        textarea.lines(),
        textarea.cursor(),
        g,
        area.width,
        hint,
    );

    // Vertical scroll so the cursor row stays visible past MAX_INPUT_ROWS.
    let last_row = area.height.saturating_sub(1);
    let scroll_y = cy.saturating_sub(last_row);
    f.render_widget(Paragraph::new(rows).scroll((scroll_y, 0)), area);

    // Place the REAL terminal cursor at the input position so it blinks (the
    // terminal's native cursor), sitting on the first hint cell when the line is
    // empty — a placeholder-style cursor rather than a static block tacked on at
    // the end of the dim hint.
    let cur_x = area.x + cx;
    let cur_y = area.y + cy - scroll_y;
    if show_cursor
        && cur_x <= area.right().saturating_sub(1)
        && cur_y <= area.bottom().saturating_sub(1)
    {
        f.set_cursor_position((cur_x, cur_y));
    }
}
