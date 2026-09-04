//! **A cursor over a list longer than its window.**
//!
//! Six surfaces are becoming arrow-navigated panels — models, backends,
//! context, conversations, personas, and whatever follows — and every one of
//! them needs the same three numbers: where the cursor is, where the window
//! starts, and how tall the window is. That is the whole of this module.
//!
//! It exists BEFORE the second consumer rather than after the fourth. The
//! reuse discipline in `CLAUDE.md` is written from a measured example — five
//! spinner implementations, four erase strategies, three animation clocks —
//! and the cheapest moment to not repeat that is while there is still one
//! caller.
//!
//! # What it deliberately is not
//!
//! Not a widget. It renders nothing, owns no keys and knows no ratatui: a
//! panel binds its own keys (`/backends` wants `e`/`a`/`d`, the model picker
//! does not) and draws its own rows. Sharing the ARITHMETIC is the win;
//! sharing a "list panel" would force five different panels through one
//! shape they do not agree on.

/// Where the cursor is in a list, and which slice of it is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListCursor {
    at: usize,
    top: usize,
    rows: usize,
    len: usize,
}

impl ListCursor {
    /// A cursor over `len` items showing `rows` at a time, starting at `at`.
    ///
    /// `rows` is clamped to at least one: a zero-row window would make every
    /// invariant below vacuous and hide the bug rather than the list.
    pub(crate) fn new(len: usize, rows: usize, at: usize) -> Self {
        let mut cursor = Self {
            at: at.min(len.saturating_sub(1)),
            top: 0,
            rows: rows.max(1),
            len,
        };
        cursor.reveal();
        cursor
    }

    pub(crate) fn at(self) -> usize {
        self.at
    }

    pub(crate) fn top(self) -> usize {
        self.top
    }

    /// One window, less a row of overlap — a page that keeps one line of
    /// context reads as movement; a page that keeps none reads as teleporting.
    pub(crate) fn page(self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    /// Move the cursor, clamping at both ends.
    ///
    /// **Clamped, never wrapped.** Wrapping means holding `↓` past the end
    /// silently returns you to the top, and on a list of fifty-two that is
    /// indistinguishable from having gone nowhere.
    pub(crate) fn step(&mut self, delta: isize) {
        if self.len == 0 {
            return;
        }
        let last = self.len - 1;
        self.at = if delta < 0 {
            self.at.saturating_sub(delta.unsigned_abs())
        } else {
            self.at.saturating_add(delta.unsigned_abs()).min(last)
        };
        self.reveal();
    }

    /// Named `home`/`end` rather than `to_start`/`to_end`: on a `Copy` type a
    /// `to_*` method reads as a conversion, and these move a cursor.
    pub(crate) fn home(&mut self) {
        self.at = 0;
        self.reveal();
    }

    pub(crate) fn end(&mut self) {
        self.at = self.len.saturating_sub(1);
        self.reveal();
    }

    /// Move the WINDOW so the cursor is inside it. The cursor never moves to
    /// suit the window — a list that scrolled out from under the selection
    /// would make the selected row the one place you cannot look.
    ///
    /// Invariant afterwards: `top <= at < top + rows`, and `top` is never
    /// further than the last full window.
    fn reveal(&mut self) {
        if self.at < self.top {
            self.top = self.at;
        } else if self.at >= self.top + self.rows {
            self.top = self.at + 1 - self.rows;
        }
        self.top = self.top.min(self.len.saturating_sub(self.rows));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The invariant, walked end to end in both directions. Everything else
    /// here is a special case of this holding.
    #[test]
    fn the_cursor_never_leaves_its_window() {
        let mut c = ListCursor::new(52, 9, 0);
        for _ in 0..60 {
            c.step(1);
            assert!(
                c.at() >= c.top() && c.at() < c.top() + 9,
                "cursor {} outside [{}, {})",
                c.at(),
                c.top(),
                c.top() + 9
            );
        }
        assert_eq!(c.at(), 51, "clamped at the end, not wrapped");
        for _ in 0..60 {
            c.step(-1);
            assert!(c.at() >= c.top() && c.at() < c.top() + 9);
        }
        assert_eq!(c.at(), 0, "and clamped at the start");
        assert_eq!(c.top(), 0);
    }

    /// The window stops at the last full page, so the final rows are never
    /// rendered against blank space the operator can walk into.
    #[test]
    fn the_window_stops_at_the_last_full_page() {
        let mut c = ListCursor::new(52, 9, 0);
        c.end();
        assert_eq!(c.at(), 51);
        assert_eq!(c.top(), 43, "52 - 9");
        assert_eq!(c.top() + 9, 52, "the window ends exactly at the list end");
    }

    /// A list shorter than its window never scrolls at all.
    #[test]
    fn a_short_list_never_scrolls() {
        let mut c = ListCursor::new(3, 9, 0);
        c.end();
        assert_eq!(c.at(), 2);
        assert_eq!(c.top(), 0);
        c.step(5);
        assert_eq!(c.top(), 0);
    }

    /// Empty is survivable and inert — `/models` against an unreachable
    /// backend is a normal Tuesday, not an exceptional case.
    #[test]
    fn an_empty_list_is_inert_rather_than_panicking() {
        let mut c = ListCursor::new(0, 9, 0);
        assert_eq!(c.at(), 0);
        assert_eq!(c.top(), 0);
        c.step(1);
        c.step(-1);
        c.end();
        c.home();
        assert_eq!(c.at(), 0);
        assert_eq!(c.top(), 0);
    }

    /// Opening on a chosen row is what makes "what else?" answerable from
    /// where you already are.
    #[test]
    fn it_opens_on_the_row_it_is_given_and_reveals_it() {
        let c = ListCursor::new(52, 9, 40);
        assert_eq!(c.at(), 40);
        assert!(c.at() >= c.top() && c.at() < c.top() + 9);

        // An out-of-range start is clamped rather than trusted.
        let c = ListCursor::new(5, 9, 99);
        assert_eq!(c.at(), 4);
    }

    /// A page keeps one row of context, and both directions agree.
    #[test]
    fn paging_moves_one_window_less_an_overlap_row() {
        let mut c = ListCursor::new(52, 9, 0);
        assert_eq!(c.page(), 8);
        let page = c.page() as isize;
        c.step(page);
        assert_eq!(c.at(), 8);
        c.step(page);
        assert_eq!(c.at(), 16);
        c.step(-page);
        assert_eq!(c.at(), 8);
    }

    /// A one-row window is degenerate but legal, and a zero-row window is
    /// treated as one rather than making every invariant vacuous.
    #[test]
    fn degenerate_window_heights_are_survivable() {
        let mut c = ListCursor::new(5, 1, 0);
        c.step(3);
        assert_eq!(c.at(), 3);
        assert_eq!(c.top(), 3, "a one-row window tracks the cursor exactly");
        assert_eq!(c.page(), 1, "and pages by one");

        let c = ListCursor::new(5, 0, 2);
        assert_eq!(c.at(), 2);
        assert_eq!(c.top(), 2);
    }
}
