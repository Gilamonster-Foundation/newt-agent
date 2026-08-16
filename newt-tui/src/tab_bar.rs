//! What a tab looks like once it has been projected for display.
//!
//! This module holds the *data* that crosses the session→terminal boundary,
//! and deliberately nothing else. The layout and rendering that consume it
//! (#1669 PR-B / roadmap 16.2) land separately and depend on this, not the
//! other way round: the surface protocol must not have to know how a bar is
//! drawn in order to carry one.
//!
//! Why a projection rather than the live `TabSet`: the tabs are session state,
//! the bar is terminal chrome, and after the execution relocation (#1718)
//! those live on different threads. Sending a snapshot keeps the terminal from
//! reaching into a session's mutable state to draw a frame — the same reason
//! `set_runtime_context` sends values rather than lending a handle.

/// One tab, projected for rendering.
///
/// Labels are carried, never referenced: they are recomputed by the session
/// each loop head from the conversation store, so a `/rename` shows up on the
/// next prompt and a title cannot go stale on the far side of the channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabCell {
    /// 1-based, what the operator types and what `<n>gt` means.
    pub number: usize,
    /// Freshly computed title, or `#shortid`.
    pub label: String,
    pub active: bool,
    /// This tab's pin could not be established, so it is refusing turns.
    /// Carried because the bar is the only always-visible surface, so a
    /// degraded tab must be legible there and not only on the switch that
    /// produced it.
    pub degraded: bool,
    /// Work arrived for an inactive tab since it was last visited.
    pub pending: bool,
}

/// How many rows the bar wants: **0 or 1**, never more.
pub(crate) fn bar_rows(cells: &[TabCell]) -> u16 {
    u16::from(cells.len() >= 2)
}

/// One cell's rendered text, e.g. `1:build`, `2!:deploy`, `3*:notes`.
fn cell_text(c: &TabCell) -> String {
    let mut s = c.number.to_string();
    if c.degraded {
        s.push('!');
    }
    if c.pending && !c.active {
        s.push('*');
    }
    s.push(':');
    s.push_str(&c.label);
    s
}

/// Which cells fit in `width`, as a window that always contains the active one.
///
/// Returns `(start, end)` as a half-open range over `cells`. Widening the
/// terminal can only ever show more, never fewer — the window grows left from
/// the active cell once the right side is exhausted, which is what makes
/// `A→B→A` land on the same view rather than drifting.
pub(crate) fn visible_window(cells: &[TabCell], width: u16) -> (usize, usize) {
    if cells.is_empty() || width == 0 {
        return (0, 0);
    }
    let active = cells.iter().position(|c| c.active).unwrap_or(0);
    let w = width as usize;
    let cost = |i: usize| cell_text(&cells[i]).chars().count() + 1; // + separator

    // Always show the active cell, even if it alone overflows — truncation
    // inside the cell is better than an empty bar.
    let mut used = cost(active);
    let (mut start, mut end) = (active, active + 1);
    // Grow right first (reading order), then left.
    loop {
        let grew_right = end < cells.len() && used + cost(end) <= w;
        if grew_right {
            used += cost(end);
            end += 1;
            continue;
        }
        let grew_left = start > 0 && used + cost(start - 1) <= w;
        if grew_left {
            start -= 1;
            used += cost(start);
            continue;
        }
        break;
    }
    (start, end)
}

/// The bar's text for `width` columns, or `None` when it should not render.
///
/// `…` marks tabs scrolled off either side, so the operator can tell "3 tabs"
/// from "3 of 9".
pub(crate) fn layout_tab_cells(cells: &[TabCell], width: u16) -> Option<String> {
    if bar_rows(cells) == 0 || width == 0 {
        return None;
    }
    // The ellipses cost columns too. Reserve for them and re-window until the
    // reservation stops changing — it converges in at most two passes, since
    // `reserve` only ever takes 0, 1 or 2. Without this the line was built
    // one-or-two columns too wide and the final clip ate the very marker that
    // says "there are more tabs this way", which is the one character the
    // operator most needs.
    let mut reserve = 0u16;
    let (start, end) = loop {
        let (s, e) = visible_window(cells, width.saturating_sub(reserve));
        let need = u16::from(s > 0) + u16::from(e < cells.len());
        if need <= reserve {
            break (s, e);
        }
        reserve = need;
    };
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    for (i, c) in cells[start..end].iter().enumerate() {
        if i > 0 || start > 0 {
            out.push(' ');
        }
        out.push_str(&cell_text(c));
    }
    if end < cells.len() {
        out.push('…');
    }
    // A single over-wide cell can still exceed the width; clip rather than wrap,
    // since wrapping would silently steal a second row from the input.
    if out.chars().count() > width as usize {
        out = out.chars().take(width as usize).collect();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(n: usize, label: &str, active: bool) -> TabCell {
        TabCell {
            number: n,
            label: label.to_string(),
            active,
            degraded: false,
            pending: false,
        }
    }

    #[test]
    fn a_single_tab_renders_no_row_at_all() {
        let one = vec![cell(1, "solo", true)];
        assert_eq!(bar_rows(&one), 0, "not an empty row — NO row");
        assert_eq!(layout_tab_cells(&one, 80), None);
        // And zero tabs is the same, defensively.
        assert_eq!(bar_rows(&[]), 0);
        assert_eq!(layout_tab_cells(&[], 80), None);
    }

    #[test]
    fn two_tabs_render_one_row() {
        let cells = vec![cell(1, "a", true), cell(2, "b", false)];
        assert_eq!(bar_rows(&cells), 1);
        assert_eq!(layout_tab_cells(&cells, 80).unwrap(), "1:a 2:b");
    }

    #[test]
    fn the_active_cell_is_always_visible_however_narrow() {
        let cells: Vec<TabCell> = (1..=9).map(|n| cell(n, "conversation", n == 9)).collect();
        let line = layout_tab_cells(&cells, 20).unwrap();
        assert!(
            line.contains("9:"),
            "a bar that can hide the tab you are looking at is worse than none: {line}"
        );
        // Even at width 1 the active cell wins over showing nothing.
        let narrow = layout_tab_cells(&cells, 1).unwrap();
        assert_eq!(narrow.chars().count(), 1);
    }

    #[test]
    fn overflow_is_marked_on_the_side_it_happened() {
        let cells: Vec<TabCell> = (1..=9).map(|n| cell(n, "xxxxx", n == 5)).collect();
        let line = layout_tab_cells(&cells, 24).unwrap();
        assert!(line.starts_with('…'), "tabs hidden to the left: {line}");
        assert!(line.ends_with('…'), "tabs hidden to the right: {line}");
        // First tab active → nothing hidden on the left.
        let first: Vec<TabCell> = (1..=9).map(|n| cell(n, "xxxxx", n == 1)).collect();
        let line = layout_tab_cells(&first, 24).unwrap();
        assert!(!line.starts_with('…'), "{line}");
        assert!(line.ends_with('…'), "{line}");
    }

    #[test]
    fn the_line_never_exceeds_the_width() {
        for width in 1u16..40 {
            let cells: Vec<TabCell> = (1..=6)
                .map(|n| cell(n, "a-long-conversation-title", n == 3))
                .collect();
            let line = layout_tab_cells(&cells, width).unwrap();
            assert!(
                line.chars().count() <= width as usize,
                "width {width} overflowed with {line:?}"
            );
        }
    }

    #[test]
    fn a_degraded_tab_is_marked_in_the_bar() {
        let mut cells = vec![cell(1, "a", true), cell(2, "b", false)];
        cells[1].degraded = true;
        let line = layout_tab_cells(&cells, 80).unwrap();
        assert!(line.contains("2!:b"), "degraded tabs carry `!`: {line}");
    }

    #[test]
    fn a_pending_inactive_tab_is_badged_but_the_active_one_is_not() {
        let mut cells = vec![cell(1, "a", true), cell(2, "b", false)];
        cells[0].pending = true;
        cells[1].pending = true;
        let line = layout_tab_cells(&cells, 80).unwrap();
        assert!(
            line.contains("1:a"),
            "the ACTIVE tab is never badged — you are looking at it: {line}"
        );
        assert!(line.contains("2*:b"), "{line}");
    }

    #[test]
    fn widening_never_shows_fewer_tabs() {
        let cells: Vec<TabCell> = (1..=8).map(|n| cell(n, "conv", n == 4)).collect();
        let mut last = 0usize;
        for width in 4u16..80 {
            let (s, e) = visible_window(&cells, width);
            let shown = e - s;
            assert!(
                shown >= last,
                "widening from {} to {width} showed fewer tabs ({last} -> {shown})",
                width - 1
            );
            last = shown;
        }
    }

    #[test]
    fn the_window_is_stable_for_a_given_active_tab_and_width() {
        // Layout is a pure function of (cells, width): the same inputs must
        // land the same view, which is what makes A→B→A return to the frame
        // the operator left rather than drifting.
        let cells: Vec<TabCell> = (1..=7).map(|n| cell(n, "conv", n == 5)).collect();
        let a = layout_tab_cells(&cells, 30);
        let b = layout_tab_cells(&cells, 30);
        assert_eq!(a, b);
    }
}
