//! #3016: the F4 fold list — a pure state for a transcript-style view whose
//! items fold and unfold in place.
//!
//! The crush item model (see the harness survey in #3016): an item renders at a
//! width, a memo keyed on (item, width, version) keeps unchanged items from
//! re-rendering, and the view is a walk from `(offset_idx, offset_line)` that
//! stops when the viewport budget is full. Toggling bumps one item's version, so
//! only that item re-renders. A collapsed item shows a preview: the tail while
//! its output is streaming, the head once it is finished.
//!
//! No terminal and no ratatui types here, so every rule is unit-tested. An
//! item is addressed by an opaque `key`; this module does not know how a
//! handle is spelled. Bodies live here, never in the model's SpillStore, and
//! nothing is persisted (#3016 decisions 2 and 3).
// ponytail: no follow-bottom anchor yet; add when a live item streams into the view.

use std::collections::HashMap;

use newt_core::tty::wrap_line;

/// Body lines a collapsed item previews before the "more lines" hint.
const PREVIEW_LINES: usize = 3;

struct FoldItem {
    key: String,
    title: String,
    body: Vec<String>,
    finished: bool,
    expanded: bool,
    version: u64,
}

/// One drawn row: its text, the item it belongs to, and whether it is that
/// item's header line (the row a selection marker belongs on).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub text: String,
    pub item: usize,
    pub header: bool,
}

#[derive(Default)]
pub(crate) struct FoldList {
    items: Vec<FoldItem>,
    selected: usize,
    offset_idx: usize,
    offset_line: usize,
    /// item -> (width, version, rendered lines)
    memo: HashMap<usize, (usize, u64, Vec<String>)>,
    renders: usize,
    /// A selection move is pending: the next render must show the header.
    reveal: bool,
}

impl FoldList {
    /// Add an empty, still-streaming item and return its index.
    pub(crate) fn push(&mut self, key: impl Into<String>, title: impl Into<String>) -> usize {
        self.items.push(FoldItem {
            key: key.into(),
            title: title.into(),
            body: Vec::new(),
            finished: false,
            expanded: false,
            version: 0,
        });
        self.items.len() - 1
    }

    /// Add a finished item in one call.
    pub(crate) fn push_finished(
        &mut self,
        key: impl Into<String>,
        title: impl Into<String>,
        body: impl IntoIterator<Item = String>,
    ) -> usize {
        let idx = self.push(key, title);
        self.append(idx, body);
        self.finish(idx);
        idx
    }

    /// Stream more output into an item. A finished item is frozen.
    pub(crate) fn append(&mut self, idx: usize, lines: impl IntoIterator<Item = String>) {
        if let Some(item) = self.items.get_mut(idx).filter(|i| !i.finished) {
            item.body.extend(lines);
            item.version += 1;
        }
    }

    pub(crate) fn finish(&mut self, idx: usize) {
        if let Some(item) = self.items.get_mut(idx).filter(|i| !i.finished) {
            item.finished = true;
            item.version += 1;
        }
    }

    /// The selected item's key, for the caller to act on.
    pub(crate) fn selected_key(&self) -> Option<&str> {
        self.items.get(self.selected).map(|i| i.key.as_str())
    }

    /// The selected item's index.
    pub(crate) fn selected_item(&self) -> usize {
        self.selected
    }

    pub(crate) fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
        self.reveal = true;
    }

    pub(crate) fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.reveal = true;
    }

    /// Fold or unfold the selected item in place.
    pub(crate) fn toggle_selected(&mut self) {
        if let Some(item) = self.items.get_mut(self.selected) {
            item.expanded = !item.expanded;
            item.version += 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn is_expanded(&self, idx: usize) -> bool {
        self.items.get(idx).is_some_and(|i| i.expanded)
    }

    /// How many item renders have run; the memo is why this stays flat.
    #[cfg(test)]
    pub(crate) fn render_count(&self) -> usize {
        self.renders
    }

    /// Scroll the view by `delta` rendered lines (negative is up).
    pub(crate) fn scroll(&mut self, delta: isize, width: usize) {
        for _ in 0..delta.unsigned_abs() {
            if delta > 0 {
                let len = self.lines(self.offset_idx, width).len();
                if self.offset_line + 1 < len {
                    self.offset_line += 1;
                } else if self.offset_idx + 1 < self.items.len() {
                    self.offset_idx += 1;
                    self.offset_line = 0;
                }
            } else if self.offset_line > 0 {
                self.offset_line -= 1;
            } else if self.offset_idx > 0 {
                self.offset_idx -= 1;
                self.offset_line = self.lines(self.offset_idx, width).len() - 1;
            }
        }
    }

    /// Walk forward from the offset until `budget` rows are filled. After a
    /// selection move the selected header is always on screen: if the walk
    /// misses it, the selected item becomes the top item. Free scrolling is
    /// left alone.
    pub(crate) fn render(&mut self, width: usize, budget: usize) -> Vec<Row> {
        if self.items.is_empty() {
            return Vec::new();
        }
        self.selected = self.selected.min(self.items.len() - 1);
        self.offset_idx = self.offset_idx.min(self.items.len() - 1);
        let reveal = std::mem::take(&mut self.reveal);
        if reveal && self.selected < self.offset_idx {
            self.offset_idx = self.selected;
            self.offset_line = 0;
        }
        let mut rows = self.walk(width, budget);
        if reveal && !rows.iter().any(|r| r.header && r.item == self.selected) {
            self.offset_idx = self.selected;
            self.offset_line = 0;
            rows = self.walk(width, budget);
        }
        rows
    }

    fn walk(&mut self, width: usize, budget: usize) -> Vec<Row> {
        let mut rows = Vec::with_capacity(budget);
        let first = self.lines(self.offset_idx, width).len();
        self.offset_line = self.offset_line.min(first - 1);
        let mut line = self.offset_line;
        for idx in self.offset_idx..self.items.len() {
            let lines = self.lines(idx, width);
            for (n, text) in lines.iter().enumerate().skip(line) {
                if rows.len() == budget {
                    return rows;
                }
                rows.push(Row {
                    text: text.clone(),
                    item: idx,
                    header: n == 0,
                });
            }
            line = 0;
        }
        rows
    }

    /// The item's rendered lines at `width`, from the memo when its version
    /// and width are unchanged.
    fn lines(&mut self, idx: usize, width: usize) -> &[String] {
        let version = self.items[idx].version;
        if !matches!(self.memo.get(&idx), Some((w, v, _)) if *w == width && *v == version) {
            self.renders += 1;
            let lines = render_item(&self.items[idx], width);
            self.memo.insert(idx, (width, version, lines));
        }
        &self.memo[&idx].2
    }
}

fn render_item(item: &FoldItem, width: usize) -> Vec<String> {
    let mark = if item.expanded { '▾' } else { '▸' };
    let mut out = vec![wrap_line(&format!("{mark} {}", item.title), width.max(1)).swap_remove(0)];
    let body_width = width.saturating_sub(2).max(1);
    let indent = |lines: &[String]| -> Vec<String> {
        lines
            .iter()
            .flat_map(|l| wrap_line(l, body_width))
            .map(|l| format!("  {l}"))
            .collect()
    };
    let n = item.body.len();
    if item.expanded || n <= PREVIEW_LINES {
        out.extend(indent(&item.body));
    } else if item.finished {
        out.extend(indent(&item.body[..PREVIEW_LINES]));
        out.push(format!(
            "  … {} more lines [space to expand]",
            n - PREVIEW_LINES
        ));
    } else {
        out.push(format!(
            "  … {} earlier lines [space to expand]",
            n - PREVIEW_LINES
        ));
        out.extend(indent(&item.body[n - PREVIEW_LINES..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn collapsed_finished_item_previews_the_head_with_a_hint() {
        let mut l = FoldList::default();
        l.push_finished("a", "spill a", body(10));
        let rows = l.render(40, 20);
        assert_eq!(
            texts(&rows),
            [
                "▸ spill a",
                "  line 0",
                "  line 1",
                "  line 2",
                "  … 7 more lines [space to expand]"
            ]
        );
    }

    #[test]
    fn streaming_item_previews_the_tail_and_finishing_flips_to_the_head() {
        let mut l = FoldList::default();
        let i = l.push("a", "live");
        l.append(i, body(10));
        let rows = l.render(40, 20);
        assert_eq!(texts(&rows)[1], "  … 7 earlier lines [space to expand]");
        assert_eq!(texts(&rows)[4], "  line 9");
        l.finish(i);
        assert_eq!(texts(&l.render(40, 20))[1], "  line 0");
        l.append(i, body(5));
        assert_eq!(l.items[0].body.len(), 10, "a finished item is frozen");
    }

    #[test]
    fn toggle_expands_in_place_and_collapses_again() {
        let mut l = FoldList::default();
        l.push_finished("a", "A", body(6));
        l.push_finished("b", "B", body(1));
        l.toggle_selected();
        let rows = l.render(40, 20);
        assert_eq!(rows.len(), 1 + 6 + 2, "full body, then the next item");
        assert_eq!(rows[0].text, "▾ A");
        assert!(l.is_expanded(0) && !l.is_expanded(1));
        l.toggle_selected();
        assert_eq!(l.render(40, 20).len(), 5 + 2);
    }

    #[test]
    fn toggle_rerenders_only_the_toggled_item() {
        let mut l = FoldList::default();
        for k in ["a", "b", "c"] {
            l.push_finished(k, k, body(8));
        }
        l.render(40, 50);
        let before = l.render_count();
        l.render(40, 50);
        assert_eq!(
            l.render_count(),
            before,
            "unchanged items come from the memo"
        );
        l.toggle_selected();
        l.render(40, 50);
        assert_eq!(l.render_count(), before + 1);
        l.render(30, 50);
        assert_eq!(
            l.render_count(),
            before + 4,
            "a width change invalidates all"
        );
    }

    #[test]
    fn the_budget_bounds_the_walk() {
        let mut l = FoldList::default();
        l.push_finished("a", "A", body(100));
        l.toggle_selected();
        assert_eq!(l.render(40, 7).len(), 7);
    }

    #[test]
    fn scrolling_walks_inside_a_body_and_across_items() {
        let mut l = FoldList::default();
        l.push_finished("a", "A", body(6));
        l.push_finished("b", "B", body(1));
        l.toggle_selected();
        l.scroll(3, 40);
        let rows = l.render(40, 3);
        assert_eq!(texts(&rows), ["  line 2", "  line 3", "  line 4"]);
        l.scroll(4, 40);
        assert_eq!(l.render(40, 3)[0].text, "▸ B");
        l.scroll(-100, 40);
        assert_eq!(l.render(40, 3)[0].text, "▾ A");
    }

    #[test]
    fn selection_moves_clamp_and_stay_on_screen() {
        let mut l = FoldList::default();
        for k in ["a", "b", "c"] {
            l.push_finished(k, k, body(8));
        }
        l.select_prev();
        assert_eq!(l.selected_item(), 0);
        l.select_next();
        l.select_next();
        l.select_next();
        assert_eq!(l.selected_item(), 2);
        let rows = l.render(40, 6);
        assert!(rows.iter().any(|r| r.header && r.item == 2));
        l.select_prev();
        l.select_prev();
        assert!(l.render(40, 6).iter().any(|r| r.header && r.item == 0));
    }

    #[test]
    fn long_lines_wrap_at_the_width_and_empty_list_is_safe() {
        let mut l = FoldList::default();
        assert!(l.render(10, 5).is_empty());
        l.push_finished("a", "A", vec!["x".repeat(30)]);
        assert!(l.render(12, 20).len() > 2);
        l.toggle_selected();
        l.scroll(1, 12);
        l.toggle_selected();
        assert!(
            !l.render(12, 20).is_empty(),
            "offset re-clamps after a collapse"
        );
    }
}
