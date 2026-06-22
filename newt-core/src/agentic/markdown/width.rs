//! Display-width primitives for the Markdown renderer.
//!
//! Wrapping and (in Step 24.2) table layout count *display columns*, not bytes
//! or `char`s — a CJK ideograph or a wide emoji occupies two cells, a
//! combining mark zero. `unicode-width` encodes the Unicode East-Asian-Width
//! table that the terminal itself uses, so our wrap points line up with what
//! the user actually sees.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of a single `char` in terminal cells (control chars → 0).
pub(super) fn ch_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Display width of a string in terminal cells.
pub(super) fn str_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}
