//! Bounded, process-local archive for completed Rich-TUI tool output.

use crate::spill_view::{SpillStream, SpillView};
use newt_core::agentic::{session_output_cid, SpillCid, MIN_SPILL_TAIL_CHARS};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const SANITIZE_HISTORY_LINES: usize = 4_096;
const SANITIZE_LINE_CHARS: usize = 4_096;
const MAX_ENTRIES: usize = 64;
const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 1024 * 1024;

/// Its reader half (`id`, `dropped_lines`, the accessors, and the archive's
/// `get`/`latest`) serves the Rich-TUI transcript pager and `/spill`. A
/// `live-spill` build without `rich-tui` still archives spills but compiles no
/// reader, so that half is unread there — the same shape, and the same
/// attribute, as `mod transcript_pager` in lib.rs.
#[derive(Clone, Debug)]
pub(crate) struct CompletedSpill {
    #[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
    id: SpillCid,
    lines: Arc<[String]>,
    #[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
    dropped_lines: usize,
}

#[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
impl CompletedSpill {
    pub(crate) fn id(&self) -> SpillCid {
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

/// Why `/spill open <tail>` named no single retained result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpillLookupError {
    TooShort,
    Unknown,
    Ambiguous,
}

impl std::error::Error for SpillLookupError {}

impl std::fmt::Display for SpillLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(
                f,
                "a spill id needs at least {MIN_SPILL_TAIL_CHARS} characters"
            ),
            Self::Unknown => write!(f, "no retained spill ends with that id"),
            Self::Ambiguous => write!(f, "that id matches more than one spill; type more of it"),
        }
    }
}

#[derive(Debug)]
struct ArchiveState {
    nonce: [u8; 16],
    inspection_available: bool,
    bytes: usize,
    entries: VecDeque<CompletedSpill>,
}

/// Session-owned archive. It never touches the conversation store or disk;
/// oldest bodies are evicted when either the entry or byte budget is reached.
#[derive(Debug)]
pub(crate) struct CompletedSpillArchive {
    state: Mutex<ArchiveState>,
    max_entries: usize,
    max_total_bytes: usize,
    max_entry_bytes: usize,
}

fn session_nonce() -> [u8; 16] {
    let mut nonce = [0u8; 16];
    // A nonce only has to differ between sessions; an entropy failure is
    // vanishingly rare and would leave a predictable (still valid) handle.
    let _ = getrandom::getrandom(&mut nonce);
    nonce
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
                nonce: session_nonce(),
                inspection_available: false,
                bytes: 0,
                entries: VecDeque::new(),
            }),
            max_entries: max_entries.max(1),
            max_total_bytes: max_total_bytes.max(1),
            max_entry_bytes: max_entry_bytes.max(1).min(max_total_bytes.max(1)),
        }
    }

    pub(crate) fn retain(&self, output: &str) -> Option<SpillCid> {
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
        let id = session_output_cid(state.nonce, &retained.join("\n")).ok()?;
        // Same bytes, same handle: a repeat result moves to the newest slot
        // instead of leaving two entries that answer to one id.
        if let Some(at) = state.entries.iter().position(|spill| spill.id == id) {
            if let Some(old) = state.entries.remove(at) {
                state.bytes = state.bytes.saturating_sub(old.bytes());
            }
        }
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
        Some(id)
    }

    /// Resolve a full handle or a unique suffix of at least
    /// [`MIN_SPILL_TAIL_CHARS`] characters.
    #[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
    pub(crate) fn find(&self, tail: &str) -> Result<CompletedSpill, SpillLookupError> {
        if tail.len() < MIN_SPILL_TAIL_CHARS {
            return Err(SpillLookupError::TooShort);
        }
        self.lookup(tail)
    }

    fn lookup(&self, tail: &str) -> Result<CompletedSpill, SpillLookupError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut hits = state
            .entries
            .iter()
            .filter(|spill| spill.id.to_handle().ends_with(tail));
        match (hits.next(), hits.next()) {
            (Some(spill), None) => Ok(spill.clone()),
            (None, _) => Err(SpillLookupError::Unknown),
            (Some(_), Some(_)) => Err(SpillLookupError::Ambiguous),
        }
    }

    /// `/spill open` completions, newest first: `(id tail, label)` where the
    /// label is the body's first line, cut short.
    #[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
    pub(crate) fn choices(&self) -> Vec<(String, String)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .entries
            .iter()
            .rev()
            .map(|spill| {
                let first = spill.lines.iter().find(|l| !l.trim().is_empty());
                let label: String = first.map_or("", |l| l.trim()).chars().take(40).collect();
                (spill.id.tail(), label)
            })
            .collect()
    }

    /// Called only once the cockpit has installed its operator action.
    #[cfg(all(unix, feature = "rich-tui"))]
    pub(crate) fn enable_inspection(&self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .inspection_available = true;
    }

    /// A bounded, stable inspection snapshot, newest first. Clones share bodies.
    #[cfg(any(test, all(unix, feature = "rich-tui")))]
    pub(crate) fn snapshot(&self) -> Vec<CompletedSpill> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entries
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    #[cfg_attr(not(feature = "rich-tui"), allow(dead_code))]
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
    fn recovery_hint(&self, id: &SpillCid) -> String {
        let available = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .inspection_available;
        if available {
            format!("F4 inspect · /spill open [{}]", id.tail())
        } else {
            format!("/spill open [{}]", id.tail())
        }
    }

    fn retain_completed(&self, output: &str) -> Option<SpillCid> {
        self.retain(output)
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

    fn cid(archive: &CompletedSpillArchive, text: &str) -> SpillCid {
        archive.retain(text).expect("cid")
    }

    #[cfg(all(unix, feature = "rich-tui"))]
    #[test]
    fn markers_advertise_inspection_only_after_the_host_installs_it() {
        use newt_core::agentic::CompletedSpillRenderer;
        let archive = CompletedSpillArchive::default();
        let id = cid(&archive, "one\n");
        let tail = id.tail();
        assert_eq!(archive.recovery_hint(&id), format!("/spill open [{tail}]"));
        archive.enable_inspection();
        assert_eq!(
            archive.recovery_hint(&id),
            format!("F4 inspect · /spill open [{tail}]")
        );
    }

    /// #3022: the completion list is the retained ids, newest first, each with
    /// a short label from the body's first line.
    #[test]
    fn choices_are_newest_first_with_a_short_first_line_label() {
        let archive = CompletedSpillArchive::default();
        let first = cid(&archive, "first line\nmore\n");
        let long = format!("{}\n", "x".repeat(200));
        let second = cid(&archive, &long);
        let choices = archive.choices();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].0, second.tail());
        assert_eq!(choices[1], (first.tail(), "first line".to_string()));
        assert!(choices[0].1.chars().count() <= 41, "{}", choices[0].1);
    }

    /// #3022: markers lead with F4 and the bare form; the id goes last.
    #[test]
    fn the_hint_leads_with_the_bare_form_and_ends_with_the_id() {
        use newt_core::agentic::CompletedSpillRenderer;
        let archive = CompletedSpillArchive::default();
        let id = cid(&archive, "one\n");
        let tail = id.tail();
        assert_eq!(archive.recovery_hint(&id), format!("/spill open [{tail}]"));
    }

    #[test]
    fn inspection_snapshot_is_newest_first_and_survives_eviction() {
        let archive = CompletedSpillArchive::with_limits(2, 1000, 500);
        let first = cid(&archive, "first");
        let second = cid(&archive, "second");
        let snapshot = archive.snapshot();
        assert_eq!(
            snapshot.iter().map(CompletedSpill::id).collect::<Vec<_>>(),
            [second, first]
        );
        archive.retain("third");
        assert!(archive.find(&first.to_handle()).is_err());
        assert_eq!(snapshot[1].lines(), ["first"]);
    }
    /// The cockpit regression: an archive alone must mint ids, so a committed
    /// excerpt can name a real `/spill open <id>` on a surface that cannot
    /// paint an inline viewport.
    #[test]
    fn the_archive_alone_retains_and_declines_to_paint() {
        use newt_core::agentic::CompletedSpillRenderer;

        let archive = CompletedSpillArchive::default();
        let first = archive.retain_completed("first\n").expect("cid");
        let second = archive.retain_completed("second\n").expect("cid");
        assert_eq!(archive.find(&first.tail()).unwrap().lines(), &["first"]);
        assert_eq!(archive.latest().unwrap().id(), second);
        assert_eq!(
            archive.render_completed("first\n", 80, 10),
            0,
            "no rows painted — the surface that uses this cannot take the screen"
        );
    }

    /// #3016: handles are content ids, not a session counter. Same bytes, same
    /// handle (one entry, newest); different bytes, different handle; a unique
    /// tail of at least six characters or the full id resolves.
    #[test]
    fn handles_are_content_ids_and_resolve_by_unique_tail() {
        let archive = CompletedSpillArchive::default();
        let first = cid(&archive, "first\n");
        let second = cid(&archive, "second\n");
        assert_ne!(first, second);
        assert_eq!(cid(&archive, "first\n"), first, "same bytes, same handle");
        assert_eq!(
            archive.snapshot().len(),
            2,
            "a repeat is not a second entry"
        );
        assert_eq!(
            archive.latest().unwrap().id(),
            first,
            "the repeat is newest"
        );
        assert!(first.to_handle().starts_with("bafy"), "{first}");
        assert_eq!(archive.find(&first.to_handle()).unwrap().id(), first);
        assert_eq!(archive.find(&second.tail()).unwrap().id(), second);
    }

    #[test]
    fn a_short_or_unknown_tail_is_a_clear_error() {
        let archive = CompletedSpillArchive::default();
        let id = cid(&archive, "first\n");
        let handle = id.to_handle();
        let short = &handle[handle.len() - (MIN_SPILL_TAIL_CHARS - 1)..];
        assert_eq!(archive.find(short).unwrap_err(), SpillLookupError::TooShort);
        assert_eq!(
            archive.find("aaaaaaaa").unwrap_err(),
            SpillLookupError::Unknown
        );
    }

    /// Two entries sharing a tail are ambiguous, not "first wins".
    #[test]
    fn a_tail_shared_by_two_spills_is_ambiguous() {
        let archive = CompletedSpillArchive::default();
        let mut last_chars = std::collections::HashSet::new();
        let shared = (0..200)
            .find_map(|n| {
                let last = cid(&archive, &format!("body {n}\n")).to_handle().pop()?;
                (!last_chars.insert(last)).then_some(last)
            })
            .expect("pigeonhole: 32 base32 chars, 200 bodies");
        assert_eq!(
            archive.lookup(&shared.to_string()).unwrap_err(),
            SpillLookupError::Ambiguous
        );
    }

    #[test]
    fn retained_text_is_terminal_safe() {
        let archive = CompletedSpillArchive::default();
        let id = cid(&archive, "\x1b[31mred\x1b[0m\nline\twith-control\n");
        let spill = archive.find(&id.to_handle()).unwrap();
        assert_eq!(spill.lines(), &["red", "line    with-control"]);
        assert!(spill
            .lines()
            .iter()
            .all(|line| !line.chars().any(char::is_control)));
    }

    #[test]
    fn bounds_evict_old_entries_and_keep_the_tail_of_large_results() {
        let archive = CompletedSpillArchive::with_limits(2, 64, 20);
        let first = cid(&archive, "first\n");
        let second = cid(&archive, "older-line-that-will-not-fit\ntail\n");
        let third = cid(&archive, "third\n");
        assert!(archive.find(&first.to_handle()).is_err());
        let second = archive.find(&second.to_handle()).unwrap();
        assert_eq!(second.lines(), &["tail"]);
        assert_eq!(second.dropped_lines(), 1);
        assert!(archive.find(&third.to_handle()).is_ok());
    }
}
