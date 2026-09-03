//! Bounded, process-local archive for completed Rich-TUI tool output.

use crate::spill_view::{SpillStream, SpillView};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const SANITIZE_HISTORY_LINES: usize = 4_096;
const SANITIZE_LINE_CHARS: usize = 4_096;
const MAX_ENTRIES: usize = 64;
const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(crate) struct CompletedSpill {
    id: u64,
    lines: Arc<[String]>,
    dropped_lines: usize,
}

impl CompletedSpill {
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn lines(&self) -> &[String] {
        &self.lines
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.dropped_lines + self.lines.len()
    }

    pub(crate) fn dropped_lines(&self) -> usize {
        self.dropped_lines
    }

    fn bytes(&self) -> usize {
        self.lines.iter().map(String::len).sum()
    }
}

struct ArchiveState {
    next_id: u64,
    bytes: usize,
    entries: VecDeque<CompletedSpill>,
}

/// Session-owned archive. It never touches the conversation store or disk;
/// oldest bodies are evicted when either the entry or byte budget is reached.
pub(crate) struct CompletedSpillArchive {
    state: Mutex<ArchiveState>,
    max_entries: usize,
    max_total_bytes: usize,
    max_entry_bytes: usize,
}

impl Default for CompletedSpillArchive {
    fn default() -> Self {
        Self::with_limits(MAX_ENTRIES, MAX_TOTAL_BYTES, MAX_ENTRY_BYTES)
    }
}

impl CompletedSpillArchive {
    fn with_limits(max_entries: usize, max_total_bytes: usize, max_entry_bytes: usize) -> Self {
        Self {
            state: Mutex::new(ArchiveState {
                next_id: 1,
                bytes: 0,
                entries: VecDeque::new(),
            }),
            max_entries: max_entries.max(1),
            max_total_bytes: max_total_bytes.max(1),
            max_entry_bytes: max_entry_bytes.max(1).min(max_total_bytes.max(1)),
        }
    }

    pub(crate) fn retain(&self, output: &str) -> u64 {
        let mut view = SpillView::with_limits(80, 1, SANITIZE_HISTORY_LINES, SANITIZE_LINE_CHARS);
        view.push_stream_bytes(SpillStream::Stdout, output.as_bytes());
        view.finish();

        let safe_lines = view.retained_display_lines();
        let mut retained = Vec::new();
        let mut retained_bytes = 0usize;
        let mut omitted_for_bytes = 0usize;
        for line in safe_lines.iter().rev() {
            let cost = line.len();
            if retained_bytes.saturating_add(cost) > self.max_entry_bytes {
                omitted_for_bytes = safe_lines.len().saturating_sub(retained.len());
                break;
            }
            retained_bytes += cost;
            retained.push(line.clone());
        }
        retained.reverse();

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let id = state.next_id;
        state.next_id = state.next_id.saturating_add(1);
        let spill = CompletedSpill {
            id,
            lines: retained.into(),
            dropped_lines: view.dropped_line_count() + omitted_for_bytes,
        };
        state.bytes += spill.bytes();
        state.entries.push_back(spill);
        while state.entries.len() > self.max_entries || state.bytes > self.max_total_bytes {
            if let Some(evicted) = state.entries.pop_front() {
                state.bytes = state.bytes.saturating_sub(evicted.bytes());
            }
        }
        id
    }

    pub(crate) fn get(&self, id: u64) -> Option<CompletedSpill> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .iter()
            .find(|spill| spill.id == id)
            .cloned()
    }

    pub(crate) fn latest(&self) -> Option<CompletedSpill> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .back()
            .cloned()
    }
}

/// Retention WITHOUT an inline viewport.
///
/// The two halves of `CompletedSpillRenderer` are independent capabilities —
/// keeping a body so `/spill open <id>` can reopen it later has nothing to do
/// with painting rows on the screen right now — but only `LiveSpillRenderer`
/// implemented the trait, so a surface that could not paint got neither.
///
/// That is what broke history under the cockpit. `terminal_owns_turn`
/// suppresses `LiveSpillRenderer` entirely (the presenter drops the cursor
/// motion it paints with), which took the ARCHIVE down with it: no body was
/// retained, no id was minted, so every committed excerpt fell back to the
/// generic `/spill N raises this view` and `/spill last` answered "that spill
/// is not retained in this bounded session archive" for a result still on
/// screen. The operator's report was exactly right — live spill worked, dead
/// spill could not be expanded — and the cause was that retention had been
/// welded to rendering.
///
/// `render_completed` returns 0, which the trait already defines as "the
/// viewport could not take the screen". That is the truth here, not a stub.
impl newt_core::agentic::CompletedSpillRenderer for CompletedSpillArchive {
    fn retain_completed(&self, output: &str) -> Option<u64> {
        Some(self.retain(output))
    }

    fn render_completed(&self, _output: &str, _width: usize, _max_height: usize) -> usize {
        0
    }

    /// Never. This archive paints nothing, so there is never a frame on screen
    /// — and saying otherwise would invite a rewind over rows it does not own.
    fn is_active(&self) -> bool {
        false
    }

    /// Nothing to rewind. The committed excerpt above is the whole record here.
    fn erase(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cockpit regression: an archive alone must mint ids, so a committed
    /// excerpt can name a real `/spill open <id>` on a surface that cannot
    /// paint an inline viewport.
    #[test]
    fn the_archive_alone_retains_and_declines_to_paint() {
        use newt_core::agentic::CompletedSpillRenderer;

        let archive = CompletedSpillArchive::default();
        assert_eq!(archive.retain_completed("first\n"), Some(1));
        assert_eq!(archive.retain_completed("second\n"), Some(2));
        assert_eq!(archive.get(1).unwrap().lines(), &["first"]);
        assert_eq!(archive.latest().unwrap().id(), 2);
        assert_eq!(
            archive.render_completed("first\n", 80, 10),
            0,
            "no rows painted — the surface that uses this cannot take the screen"
        );
    }

    #[test]
    fn ids_are_stable_and_latest_tracks_the_newest_result() {
        let archive = CompletedSpillArchive::default();
        assert_eq!(archive.retain("first\n"), 1);
        assert_eq!(archive.retain("second\n"), 2);
        assert_eq!(archive.get(1).unwrap().lines(), &["first"]);
        assert_eq!(archive.latest().unwrap().id(), 2);
    }

    #[test]
    fn retained_text_is_terminal_safe() {
        let archive = CompletedSpillArchive::default();
        let id = archive.retain("\x1b[31mred\x1b[0m\nline\twith-control\n");
        let spill = archive.get(id).unwrap();
        assert_eq!(spill.lines(), &["red", "line    with-control"]);
        assert!(spill
            .lines()
            .iter()
            .all(|line| !line.chars().any(char::is_control)));
    }

    #[test]
    fn bounds_evict_old_entries_and_keep_the_tail_of_large_results() {
        let archive = CompletedSpillArchive::with_limits(2, 64, 20);
        let first = archive.retain("first\n");
        let second = archive.retain("older-line-that-will-not-fit\ntail\n");
        let third = archive.retain("third\n");
        assert!(archive.get(first).is_none());
        let second = archive.get(second).unwrap();
        assert_eq!(second.lines(), &["tail"]);
        assert_eq!(second.dropped_lines(), 1);
        assert!(archive.get(third).is_some());
    }
}
