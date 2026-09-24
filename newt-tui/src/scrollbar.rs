//! The scrollbar vocabulary every scrolling surface shares — the spill
//! viewport and panel chrome — so a scrollbar reads the same everywhere in
//! newt: glyphs and thumb placement, nothing else.

pub(crate) const SCROLL_ABOVE: char = '▲';
pub(crate) const SCROLL_BELOW: char = '▼';
pub(crate) const SCROLL_TRACK: char = '▒';
pub(crate) const SCROLL_THUMB: char = '▓';

/// Which of `shown` gutter rows carries the scroll thumb when the window's top
/// is `offset` rows into a scroll range of `max_offset`. `None` when nothing
/// scrolls. Shared by the spill viewport and panel chrome, so every scrollbar
/// in newt places its thumb the same way.
pub(crate) fn thumb_row(offset: usize, max_offset: usize, shown: usize) -> Option<usize> {
    if shown == 0 || max_offset == 0 {
        return None;
    }
    let offset = offset.min(max_offset);
    Some((offset * (shown - 1) + max_offset / 2) / max_offset)
}
