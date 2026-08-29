//! **The ONE width model** — display columns, not bytes and not `char`s.
//!
//! # Why this is in `tty` and not in the Markdown renderer
//!
//! A terminal measures in cells: a CJK ideograph or a wide emoji occupies two,
//! a combining mark zero. `unicode-width` encodes the Unicode East-Asian-Width
//! table the terminal itself uses, so our wrap and fit points line up with what
//! the operator actually sees.
//!
//! The workspace grew **four** competing answers to "how wide is this?" —
//! `tty::fit_line`'s `char` count, `agentic::display::wrap_to_width`'s `char`
//! count, this pair, and a hand-rolled glyph allowlist in the TUI — and the
//! only correct one was locked at `pub(super)` inside `agentic::markdown`.
//! That is the same mechanical cause `frames.rs` records for the duplicate
//! spinner frame sets: *a private module with a curated re-export list, so
//! nothing outside it could import the good implementation.* So the primitive
//! moves **up** to the public `tty` module and `agentic::markdown` re-exports
//! it downward.
//!
//! `unicode-width` is therefore a NON-optional dependency of `newt-core`: this
//! module is unconditional (`tty::fit_line` measures through it on every
//! build), so it cannot ride on the optional `markdown` feature. It is a pure
//! lookup-table crate with no transitive dependencies, so the headless wyvern
//! strip pays essentially nothing for it.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of a single `char` in terminal cells (control chars → 0).
pub fn ch_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Display width of a string in terminal cells.
pub fn str_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Word-wrap `s` into lines no wider than `width` columns (#1153): the tool-call
/// display must show the FULL command/path so the operator can audit exactly
/// what ran — truncating a `grep … | grep …` with `…` hid it. Wraps on
/// whitespace when possible; a single token longer than `width` is hard-split
/// so nothing is ever dropped.
///
/// Width is counted in display CELLS, like the rest of this module. It counted
/// `char`s until D3a (#1874) — a second metric inside the module that claims
/// the title, logged as such by the A0 inventory (§4.2.2). For the ASCII
/// commands and paths this was written for a cell IS a char, so those wraps are
/// byte-unmoved; see `wrap_line_is_byte_identical_for_ascii`. Returns at least
/// one line (possibly empty for empty input).
pub fn wrap_line(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in s.split('\n') {
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in logical.split_inclusive(' ') {
            let wlen = str_width(word);
            if cur_len + wlen > width && cur_len > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            // A single word wider than the line: hard-split it so nothing is lost.
            if wlen > width {
                for ch in word.chars() {
                    let cw = ch_width(ch);
                    // `cur_len > 0` keeps a char wider than the whole budget
                    // from flushing an empty line ahead of itself. For ASCII
                    // this is exactly the old `cur_len == width`, since a cell
                    // is a char and `cur_len` never passes `width`.
                    if cur_len + cw > width && cur_len > 0 {
                        lines.push(std::mem::take(&mut cur));
                        cur_len = 0;
                    }
                    cur.push(ch);
                    cur_len += cw;
                }
            } else {
                cur.push_str(word);
                cur_len += wlen;
            }
        }
        lines.push(cur);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{ch_width, str_width, wrap_line};

    /// **The twin for the cell migration.** For ASCII a cell IS a char, so
    /// every path this function was actually written for — commands, paths,
    /// prose — must be byte-unmoved. These goldens were captured from the
    /// char-counting implementation BEFORE the change, over the shapes its
    /// own doc comment names.
    #[test]
    fn wrap_line_is_byte_identical_for_ascii() {
        assert_eq!(
            wrap_line("cargo build --release", 11),
            ["cargo ", "build ", "--release"]
        );
        assert_eq!(
            wrap_line("grep -rn foo /very/long/path/that/keeps/going", 20),
            ["grep -rn foo ", "/very/long/path/that", "/keeps/going"]
        );
        assert_eq!(
            wrap_line("aaaaaaaaaaaaaaaaaaaaaaaaa", 10),
            ["aaaaaaaaaa", "aaaaaaaaaa", "aaaaa"]
        );
        assert_eq!(wrap_line("a\nb", 40), ["a", "b"]);
        assert_eq!(wrap_line("", 5), [""]);
        assert_eq!(
            wrap_line("one two three four", 9),
            ["one two ", "three ", "four"]
        );
    }

    /// The module is titled "the ONE width model" and `wrap_line` counted
    /// CHARS — the A0 inventory logged it as a separate metric living inside
    /// the module that claims the title (§4.2.2). Four ideographs are 4 chars
    /// and **8 cells**: the char rule emitted one 8-cell line for a 4-column
    /// budget, overflowing every caller that trusted the width.
    #[test]
    fn wrap_line_wraps_by_cells_not_chars() {
        assert_eq!(wrap_line("日本語版", 4), ["日本", "語版"]);
    }

    /// A combining mark occupies no cell, so it rides along with its base
    /// rather than forcing a break — the char rule counted it as one column.
    #[test]
    fn a_combining_mark_costs_no_column() {
        assert_eq!(wrap_line("e\u{0301}xy", 3), ["e\u{0301}xy"]);
    }

    /// The reason the promotion is worth doing at all: a `char` count and a
    /// byte count both get this wrong (2 and 6 respectively), and every width
    /// model in the workspace except this one used one of those.
    #[test]
    fn a_cjk_string_measures_in_cells_not_chars_or_bytes() {
        assert_eq!(str_width("日本"), 4);
        assert_eq!("日本".chars().count(), 2, "the char count is NOT the width");
        assert_eq!("日本".len(), 6, "and neither is the byte count");
        assert_eq!(ch_width('日'), 2);
        // A combining mark occupies no cell of its own.
        assert_eq!(ch_width('\u{0301}'), 0);
    }
}
