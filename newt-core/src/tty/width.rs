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
/// so nothing is ever dropped. Width counted in `char`s (this path is ASCII
/// commands/paths). Returns at least one line (possibly empty for empty input).
pub fn wrap_line(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in s.split('\n') {
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in logical.split_inclusive(' ') {
            let wlen = word.chars().count();
            if cur_len + wlen > width && cur_len > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            // A single word wider than the line: hard-split it so nothing is lost.
            if wlen > width {
                for ch in word.chars() {
                    if cur_len == width {
                        lines.push(std::mem::take(&mut cur));
                        cur_len = 0;
                    }
                    cur.push(ch);
                    cur_len += 1;
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
    use super::{ch_width, str_width};

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
