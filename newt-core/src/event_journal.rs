//! The chained event journal — grants, kills, reopens, note appends,
//! compressions, and conversation ops (#2085, #2009 PR-E).
//!
//! # Why this is not a third journal
//!
//! `newt-core` already had two, with three different integrity guarantees
//! between them: [`settings_receipt`](crate::settings_receipt) addresses each
//! row with a `ContentId`, and [`denial_journal`](crate::denial_journal)
//! addresses nothing at all. Both files independently describe `ts_claim` as a
//! *"wall-clock display claim; append order remains the ordering ground
//! truth"* — and neither verifies append order.
//!
//! So this module is **the chained shape both are meant to migrate onto**, not
//! a new one beside them. Adding a third journal is the sprawl the reuse
//! discipline exists to prevent.
//!
//! That sentence is now load-bearing rather than aspirational: the chain
//! machinery here — [`Journal`], [`JournalLine`], [`verify_chain`],
//! [`read_jsonl`], [`resume`], [`append_to`], [`read_head`] — is generic over
//! the payload, and [`denial_journal`](crate::denial_journal) is its first
//! adopter (`JournalLine<DenialRecord>`). A migrating journal reuses this
//! code; it does not copy the shape. `JournalEvent` remains the default
//! payload, and what stays event-specific is only the vocabulary
//! ([`EventKind`]), the path ([`journal_path`]) and the arming
//! ([`JOURNAL_PATH_ENV`]) — the things that differ per stream.
//!
//! When a third adopter arrives (`settings_receipt` is next), moving these
//! types to their own module is a rename, not a redesign.
//!
//! # What a chain buys, stated exactly
//!
//! A per-row address detects an **edited** row: re-derive the id from the row's
//! own bytes and compare. It cannot detect a **deleted** or **reordered** row,
//! because every row is independently valid and so is any subset of them.
//!
//! Each event here is a [`MerkleNode`] whose parent is the previous event's id,
//! so the id binds **the payload AND the link**. Remove a row and the next
//! row's parent names an id that is no longer there; swap two rows and the same
//! check fails. That is the deletion and reorder case closed.
//!
//! **It does not close truncation at the tail.** Lopping the last N lines off
//! leaves a shorter chain that is perfectly valid on its own terms. Nothing
//! *inside* an append-only file can detect that, which is why the Authority
//! Register's history law is *chain-plus-one-ref*: the chain, plus a head
//! reference stored separately. [`verify_chain`] takes that head as its second
//! argument and reports [`ChainBreak::Truncated`] when the chain no longer
//! reaches it. Pass `None` and you get the chain's own guarantees and honestly
//! not more.
//!
//! # Best effort, never load-bearing
//!
//! As with the other two journals: an observability write must never undo the
//! thing the operator asked for. Minting is fallible and reported; the caller
//! records what it can and proceeds.

use content_addressable::{canonical, ContentAddressable, ContentError, ContentId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub use content_addressable::MerkleNode;

/// Schema tag. Bumping it re-addresses every event, by construction.
pub const JOURNAL_EVENT_SCHEMA_V1: &str = "newt.journal-event/v1";

/// The six mutator classes #2009 counted as `Receipt::Missing` debt.
///
/// A closed vocabulary on purpose. The journal records *which kind of thing
/// happened*, and a caller cannot widen that set by passing a free string — a
/// new kind is a new variant and a schema decision, not a call-site typo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    /// A permission was granted.
    Grant,
    /// A running thing was killed.
    Kill,
    /// A previously settled decision was reopened.
    Reopen,
    /// Text was appended to a note.
    NoteAppend,
    /// A conversation was compressed.
    Compression,
    /// A conversation was renamed, forked, or dropped.
    ConversationOp,
}

impl EventKind {
    /// The wire token, matching the serde representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grant => "grant",
            Self::Kill => "kill",
            Self::Reopen => "reopen",
            Self::NoteAppend => "note-append",
            Self::Compression => "compression",
            Self::ConversationOp => "conversation-op",
        }
    }

    /// Every kind, in declaration order — so a reader can enumerate the
    /// vocabulary without the list being restated somewhere else.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Grant,
            Self::Kill,
            Self::Reopen,
            Self::NoteAppend,
            Self::Compression,
            Self::ConversationOp,
        ]
    }
}

impl std::fmt::Display for EventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One journalled event.
///
/// `subject` and `detail` are the event's own vocabulary — a tool name, a
/// conversation id, a note path — never raw operator text, so the journal
/// cannot become a second transcript of what was typed. This is the same rule
/// [`SettingChange`](crate::settings_receipt::SettingChange) holds itself to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEvent {
    pub schema: String,
    pub kind: EventKind,
    /// What the event was about: the tool granted, the conversation renamed.
    pub subject: String,
    /// The part a reader cannot reconstruct from `kind` and `subject` alone.
    pub detail: String,
    /// The route the operator actually took — `/settings`, `/resume drop`.
    /// The same effect through two verbs is two different events.
    pub via: String,
    /// Wall-clock display claim. The CHAIN is the ordering ground truth here,
    /// which is the difference between this journal and the two it replaces.
    pub ts_claim: String,
}

impl JournalEvent {
    /// Build an event, stamping the schema tag and the display timestamp.
    #[must_use]
    pub fn new(kind: EventKind, subject: &str, detail: &str, via: &str) -> Self {
        Self {
            schema: JOURNAL_EVENT_SCHEMA_V1.to_string(),
            kind,
            subject: subject.to_string(),
            detail: detail.to_string(),
            via: via.to_string(),
            ts_claim: chrono::Utc::now().to_rfc3339(),
        }
    }
}

impl ContentAddressable for JournalEvent {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// One journal line: a node and the address the whole node computes to.
///
/// The id covers the payload **and** the parent link, which is what makes the
/// line's position in the chain part of what it claims.
///
/// Generic over the payload so a migrating journal reuses this line rather
/// than declaring its own; `T` defaults to [`JournalEvent`], and
/// `denial_journal` uses `JournalLine<DenialRecord>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: DeserializeOwned"))]
pub struct JournalLine<T = JournalEvent> {
    /// The [`MerkleNode`]'s `ContentId`, canonical string form.
    pub id: String,
    pub node: MerkleNode<T>,
}

impl<T> JournalLine<T> {
    /// The single parent this line claims, if it claims exactly one.
    ///
    /// The journal is a chain, so a line has zero parents (genesis) or one. A
    /// line carrying several is a DAG node in a file that promised a chain, and
    /// [`verify_chain`] reports it rather than picking one.
    #[must_use]
    pub fn parent(&self) -> Option<&ContentId> {
        let mut parents = self.node.parents().iter();
        match (parents.next(), parents.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    }
}

impl<T: Serialize> JournalLine<T> {
    /// Re-derive this line's address from the node and compare it to the claim.
    ///
    /// Per-line only — it says nothing about the line's neighbours. Use
    /// [`verify_chain`] for that; a file of individually intact lines can still
    /// be missing half its history.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        ContentId::from_str(&self.id)
            .ok()
            .and_then(|id| self.node.verify(&id).ok())
            .unwrap_or(false)
    }

    /// The JSON line this is stored as. Pure — the fs write is separate so the
    /// unit tier can check the encoding without a file.
    ///
    /// # Errors
    ///
    /// Propagates a serialization failure.
    pub fn render_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// A chain in progress: the head to link the next event onto.
///
/// Deliberately holds **only** the head. A journal that accumulated its lines
/// in memory would quietly become a second copy of the file and drift from it;
/// the file is the record, and this is the one pointer needed to extend it.
///
/// Not generic: a head is a `ContentId` whatever the payload was, so
/// [`append`](Self::append) takes the payload type instead. That is what lets
/// [`resume`] read a head off any journal file without knowing what the stream
/// records.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Journal {
    head: Option<ContentId>,
}

impl Journal {
    /// An empty journal — the next event will be genesis.
    #[must_use]
    pub fn new() -> Self {
        Self { head: None }
    }

    /// Resume a journal whose head is already known.
    #[must_use]
    pub fn resuming_from(head: ContentId) -> Self {
        Self { head: Some(head) }
    }

    /// The current head, or `None` for an empty journal.
    #[must_use]
    pub fn head(&self) -> Option<&ContentId> {
        self.head.as_ref()
    }

    /// Link `event` onto the chain and advance the head.
    ///
    /// # Errors
    ///
    /// Propagates a canonical-encoding failure. The head is read but not
    /// written until the id is in hand, so a failed append leaves the journal
    /// exactly where it was rather than breaking the chain for the next one.
    pub fn append<T: Serialize>(&mut self, event: T) -> Result<JournalLine<T>, ContentError> {
        let node = match self.head {
            Some(parent) => MerkleNode::new(event, [parent]),
            None => MerkleNode::genesis(event),
        };
        let id = node.id()?;
        self.head = Some(id);
        Ok(JournalLine {
            id: id.to_string(),
            node,
        })
    }
}

/// One way a chain failed to verify, with the line it was found at.
///
/// Reported as a list rather than a bool because "the journal is broken" is not
/// actionable and "line 4 is missing its parent" is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainBreak {
    /// The line's own bytes do not compute to the id it carries — an edit.
    Edited { index: usize },
    /// The line's id is not parseable as a `ContentId` at all.
    Unreadable { index: usize },
    /// The line's parent is not the previous line's id — a deletion or a
    /// reorder. Both look identical from inside the file, so this does not
    /// guess which.
    BrokenLink { index: usize },
    /// A non-first line has no parent, or a line has more than one. Either way
    /// the file is not the chain it claims to be.
    NotAChain { index: usize },
    /// The chain verified, but its last line is not the head that was stored
    /// separately — lines were removed from the END.
    ///
    /// This is the case a parent chain alone cannot see, and the reason
    /// [`verify_chain`] takes a head at all.
    Truncated { expected_head: String },
}

/// Verify a whole chain, and — when a head reference is supplied — that the
/// chain still reaches it.
///
/// An empty return means intact. Pass `expected_head: None` to check only what
/// the file can prove about itself; the truncation case is then not checked,
/// and is honestly not reported as passing.
#[must_use]
pub fn verify_chain<T: Serialize>(
    lines: &[JournalLine<T>],
    expected_head: Option<&str>,
) -> Vec<ChainBreak> {
    let mut breaks = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if !line.is_intact() {
            // An edited line's id is not trustworthy, so checking its link too
            // would report a second break for the same single cause.
            breaks.push(if ContentId::from_str(&line.id).is_err() {
                ChainBreak::Unreadable { index }
            } else {
                ChainBreak::Edited { index }
            });
            continue;
        }

        match (index, line.parent()) {
            // Genesis: the first line, with no parents at all.
            (0, None) if line.node.parents().is_empty() => {}
            // A link: the parent must be the previous line's id.
            (_, Some(parent)) if index > 0 => {
                if parent.to_string() != lines[index - 1].id {
                    breaks.push(ChainBreak::BrokenLink { index });
                }
            }
            // A first line with a parent, a later line without one, or any
            // line carrying several: not the chain shape this file promises.
            _ => breaks.push(ChainBreak::NotAChain { index }),
        }
    }

    if let Some(expected) = expected_head {
        if lines.last().map(|line| line.id.as_str()) != Some(expected) {
            breaks.push(ChainBreak::Truncated {
                expected_head: expected.to_string(),
            });
        }
    }

    breaks
}

/// Parse a JSONL body into lines, skipping what does not parse.
///
/// Mirrors [`settings_receipt::read_jsonl`](crate::settings_receipt::read_jsonl)
/// so the two read the same way. A skipped unparseable line does not vanish
/// from the audit: it becomes a [`ChainBreak::BrokenLink`] at the line after
/// it, because dropping it from the list does not drop it from the chain.
#[must_use]
pub fn read_jsonl<T: DeserializeOwned>(body: &str) -> Vec<JournalLine<T>> {
    body.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Override for where the journal lands.
///
/// Set to the **empty string** to turn the journal off entirely — the same
/// switch, for the same reason, as
/// [`RECEIPT_PATH_ENV`](crate::settings_receipt::RECEIPT_PATH_ENV): without it
/// a unit test that granted a permission would append to the developer's real
/// journal.
pub const JOURNAL_PATH_ENV: &str = "NEWT_EVENT_JOURNAL";

/// Where the journal lands: `$NEWT_EVENT_JOURNAL`, else `events.jsonl` beside
/// `settings.toml`.
///
/// `None` — do not write — when the override is set but empty, or when there is
/// no user config dir at all.
#[must_use]
pub fn journal_path() -> Option<PathBuf> {
    match std::env::var_os(JOURNAL_PATH_ENV) {
        Some(explicit) if explicit.is_empty() => return None,
        Some(explicit) => return Some(PathBuf::from(explicit)),
        None => {}
    }
    crate::settings::settings_path().map(|p| p.with_file_name("events.jsonl"))
}

/// The head reference for a journal at `path` — the "one ref" half of
/// *chain-plus-one-ref*.
///
/// A sibling file rather than a line inside the journal, because a head stored
/// *in* the thing it is meant to vouch for cannot detect that thing being
/// truncated.
#[must_use]
pub fn head_path(path: &Path) -> PathBuf {
    path.with_extension("head")
}

/// Read the stored head reference, if there is one.
///
/// Returns `None` for a missing, empty, or unreadable ref — all of which mean
/// "no anchor available", which [`verify_chain`] reports honestly as "the
/// truncation case was not checked" rather than as a pass.
#[must_use]
pub fn read_head(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(head_path(path)).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Resume the chain from whatever is already on disk.
///
/// The head ref is the fast path. When it is missing — a journal written before
/// the ref existed, or a lost ref — the last line of the journal is used
/// instead, so a resumed session extends the existing chain rather than
/// starting a second one beside it.
///
/// Payload-agnostic: the fallback reads the last line's `id` through
/// `serde_json::Value`, so this works for any journal's file without being
/// told what the stream records.
#[must_use]
pub fn resume(path: &Path) -> Journal {
    let head = read_head(path).or_else(|| {
        let body = std::fs::read_to_string(path).ok()?;
        read_jsonl::<serde_json::Value>(&body)
            .last()
            .map(|line| line.id.clone())
    });
    head.and_then(|id| ContentId::from_str(&id).ok())
        .map_or_else(Journal::new, Journal::resuming_from)
}

/// Append one event to the journal at `path` and advance the stored head.
///
/// # Errors
///
/// Propagates a filesystem or encoding failure.
///
/// # Ordering
///
/// The line is appended **before** the head ref is updated. That order is
/// deliberate: a crash between the two leaves a journal one line ahead of its
/// ref, which [`verify_chain`] reports as [`ChainBreak::Truncated`] — a false
/// alarm that is inspectable and recoverable. The other order would leave the
/// ref pointing at a line that was never written, which is indistinguishable
/// from deletion. **Given a choice of which way to be wrong, be wrong in the
/// direction that over-reports.**
pub fn append_to<T: Serialize>(
    journal: &mut Journal,
    path: &Path,
    event: T,
) -> anyhow::Result<JournalLine<T>> {
    let line = journal.append(event)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rendered = line.render_line()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{rendered}")?;
    drop(file);

    crate::atomic_fs::atomic_write(&head_path(path), line.id.as_bytes())?;
    Ok(line)
}

/// Mint and durably record one event, best effort.
///
/// Returns `None` when there was nowhere to write or the write failed. Like
/// [`settings_receipt::record`](crate::settings_receipt::record), failing to
/// record must never undo what the operator asked for.
#[must_use]
pub fn record(kind: EventKind, subject: &str, detail: &str, via: &str) -> Option<JournalLine> {
    record_event(JournalEvent::new(kind, subject, detail, via))
}

/// [`record`], for a caller that already holds the event.
///
/// **The seam a call site uses when it must also SHOW the event to a test.**
/// `record` writes and hands back a [`JournalLine`] only when there was
/// somewhere to write — which under a test guard is never, since the guard
/// blanks [`JOURNAL_PATH_ENV`] precisely so a unit test cannot append to the
/// developer's real journal. A caller that built the event first can therefore
/// return the event it recorded, and its test asserts on **that object** rather
/// than on a second one minted from the same arguments and hoped to match.
#[must_use]
pub fn record_event(event: JournalEvent) -> Option<JournalLine> {
    let path = journal_path()?;
    let mut journal = resume(&path);
    append_to(&mut journal, &path, event).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(subject: &str) -> JournalEvent {
        JournalEvent::new(EventKind::Grant, subject, "allow", "/settings")
    }

    /// Build a well-formed chain of `n` events.
    fn chain(n: usize) -> (Journal, Vec<JournalLine>) {
        let mut journal = Journal::new();
        let lines = (0..n)
            .map(|i| journal.append(event(&format!("tool-{i}"))).expect("append"))
            .collect();
        (journal, lines)
    }

    #[test]
    fn a_fresh_journal_starts_with_no_head() {
        assert!(Journal::new().head().is_none());
    }

    #[test]
    fn the_first_event_is_genesis_and_later_ones_link_back() {
        let (_, lines) = chain(3);
        assert!(lines[0].node.parents().is_empty(), "first is genesis");
        assert_eq!(
            lines[1].parent().map(ToString::to_string),
            Some(lines[0].id.clone())
        );
        assert_eq!(
            lines[2].parent().map(ToString::to_string),
            Some(lines[1].id.clone())
        );
    }

    #[test]
    fn the_head_tracks_the_last_line() {
        let (journal, lines) = chain(4);
        assert_eq!(
            journal.head().map(ToString::to_string),
            Some(lines[3].id.clone())
        );
    }

    #[test]
    fn a_resumed_journal_links_onto_the_head_it_was_given() {
        let (journal, lines) = chain(2);
        let head = *journal.head().expect("head");
        let mut resumed = Journal::resuming_from(head);
        let next = resumed.append(event("after-resume")).expect("append");
        assert_eq!(
            next.parent().map(ToString::to_string),
            Some(lines[1].id.clone())
        );
    }

    #[test]
    fn an_intact_chain_reports_no_breaks() {
        let (journal, lines) = chain(5);
        let head = journal.head().expect("head").to_string();
        assert_eq!(verify_chain(&lines, Some(&head)), vec![]);
    }

    #[test]
    fn an_empty_chain_with_no_head_is_intact() {
        assert_eq!(verify_chain::<JournalEvent>(&[], None), vec![]);
    }

    // --- the tamper suite: each case the per-row check cannot see ---

    /// The case `SettingReceipt::is_intact` already catches, kept here so the
    /// chain is not shown to be better at the hard cases while being worse at
    /// the basic one.
    #[test]
    fn an_edited_line_is_caught() {
        let (_, mut lines) = chain(3);
        lines[1].node.payload.detail = "deny".to_string();
        assert!(verify_chain(&lines, None).contains(&ChainBreak::Edited { index: 1 }));
    }

    /// **The case a per-row check cannot see.** Every remaining line is
    /// individually valid; only the link at the join reveals the removal.
    #[test]
    fn a_deleted_line_is_caught() {
        let (_, mut lines) = chain(4);
        lines.remove(1);
        let breaks = verify_chain(&lines, None);
        assert!(
            breaks.contains(&ChainBreak::BrokenLink { index: 1 }),
            "removing a middle line must break the link at the line after it, got {breaks:?}"
        );
        assert!(
            lines.iter().all(JournalLine::is_intact),
            "and every surviving line is still individually intact — which is \
             exactly why the per-row check is not enough"
        );
    }

    /// Also invisible per-row: the same lines, in the wrong order.
    #[test]
    fn a_reordered_pair_is_caught() {
        let (_, mut lines) = chain(4);
        lines.swap(1, 2);
        let breaks = verify_chain(&lines, None);
        assert!(!breaks.is_empty(), "a reorder must not verify");
        assert!(lines.iter().all(JournalLine::is_intact));
    }

    /// Removing from the tail leaves a shorter chain that is valid on its own
    /// terms — the head reference is the only thing that catches it.
    #[test]
    fn a_truncated_tail_is_caught_only_by_the_head_ref() {
        let (journal, mut lines) = chain(5);
        let head = journal.head().expect("head").to_string();
        lines.truncate(3);

        assert_eq!(
            verify_chain(&lines, None),
            vec![],
            "the chain alone cannot see its own truncation — this is the \
             limit that makes the stored head necessary, not a bug"
        );
        assert_eq!(
            verify_chain(&lines, Some(&head)),
            vec![ChainBreak::Truncated {
                expected_head: head
            }],
        );
    }

    /// The whole file replaced by a fresh, internally consistent one. It
    /// verifies perfectly against itself, and not against the stored head.
    #[test]
    fn a_wholesale_replacement_is_caught_by_the_head_ref() {
        let (real, _) = chain(3);
        let head = real.head().expect("head").to_string();
        let (_, forged) = chain(3);

        assert_eq!(verify_chain(&forged, None), vec![], "internally consistent");
        assert!(matches!(
            verify_chain(&forged, Some(&head)).as_slice(),
            [ChainBreak::Truncated { .. }]
        ));
    }

    #[test]
    fn a_line_claiming_an_unparseable_id_is_reported_as_unreadable() {
        let (_, mut lines) = chain(2);
        lines[0].id = "not-a-content-id".to_string();
        assert!(verify_chain(&lines, None).contains(&ChainBreak::Unreadable { index: 0 }));
    }

    #[test]
    fn a_first_line_carrying_a_parent_is_not_a_chain() {
        let (_, lines) = chain(2);
        // The second line, standing alone, is a non-genesis head.
        let orphan = vec![lines[1].clone()];
        assert_eq!(
            verify_chain(&orphan, None),
            vec![ChainBreak::NotAChain { index: 0 }]
        );
    }

    #[test]
    fn a_line_with_several_parents_is_not_a_chain() {
        let (_, lines) = chain(3);
        let mut forked = lines.clone();
        let extra = ContentId::from_str(&lines[0].id).expect("parse");
        forked[2].node.parents.insert(extra);
        // Re-address it so this tests the SHAPE, not a stale id.
        forked[2].id = forked[2].node.id().expect("id").to_string();
        assert_eq!(
            verify_chain(&forked, None),
            vec![ChainBreak::NotAChain { index: 2 }]
        );
    }

    /// An edit is reported once. Without the `continue`, a tampered line would
    /// also fail its link check and report the same cause twice.
    #[test]
    fn one_tampered_line_reports_one_break() {
        let (_, mut lines) = chain(3);
        lines[1].node.payload.subject = "elsewhere".to_string();
        assert_eq!(verify_chain(&lines, None).len(), 1);
    }

    // --- encoding ---

    #[test]
    fn a_line_round_trips_through_jsonl() {
        let (journal, lines) = chain(3);
        let body = lines
            .iter()
            .map(|line| line.render_line().expect("render"))
            .collect::<Vec<_>>()
            .join("\n");
        let read = read_jsonl(&body);
        assert_eq!(read, lines);
        let head = journal.head().expect("head").to_string();
        assert_eq!(verify_chain(&read, Some(&head)), vec![]);
    }

    #[test]
    fn blank_and_unparseable_lines_are_skipped_on_read() {
        let (_, lines) = chain(1);
        let body = format!(
            "\n{}\n\nnot json\n",
            lines[0].render_line().expect("render")
        );
        assert_eq!(read_jsonl(&body), lines);
    }

    /// A dropped unparseable line does not silently shorten history: the line
    /// after it loses its parent and the chain says so.
    #[test]
    fn an_unparseable_middle_line_surfaces_as_a_broken_link() {
        let (_, lines) = chain(3);
        let body = format!(
            "{}\ncorrupted\n{}",
            lines[0].render_line().expect("render"),
            lines[2].render_line().expect("render"),
        );
        let read: Vec<JournalLine> = read_jsonl(&body);
        assert_eq!(read.len(), 2);
        assert_eq!(
            verify_chain(&read, None),
            vec![ChainBreak::BrokenLink { index: 1 }]
        );
    }

    // --- vocabulary ---

    #[test]
    fn every_kind_has_a_distinct_token_and_all_lists_them() {
        let tokens: std::collections::BTreeSet<_> =
            EventKind::all().iter().map(|k| k.as_str()).collect();
        assert_eq!(tokens.len(), EventKind::all().len());
        assert_eq!(
            EventKind::all().len(),
            6,
            "the six mutator classes #2009 parked; adding one is a schema decision"
        );
    }

    /// The token a kind prints is the token it serializes as — otherwise a
    /// journal would read back differently from how it displays.
    #[test]
    fn the_display_token_matches_the_wire_token() {
        for kind in EventKind::all() {
            let wire = serde_json::to_string(kind).expect("serialize");
            assert_eq!(wire, format!("\"{}\"", kind.as_str()));
            assert_eq!(kind.to_string(), kind.as_str());
        }
    }

    #[test]
    fn every_event_carries_the_schema_tag() {
        assert_eq!(event("x").schema, JOURNAL_EVENT_SCHEMA_V1);
    }

    /// Two different kinds with otherwise identical fields are different
    /// events — the kind is part of what is addressed, not a display label.
    #[test]
    fn the_kind_is_part_of_the_address() {
        let mut grant = event("tool");
        let mut kill = grant.clone();
        kill.kind = EventKind::Kill;
        // Equalise the display timestamps so only the kind differs.
        grant.ts_claim = "fixed".to_string();
        kill.ts_claim = "fixed".to_string();
        assert_ne!(
            grant.content_id().expect("id"),
            kill.content_id().expect("id")
        );
    }
}
