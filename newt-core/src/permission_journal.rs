//! Chained storage for prompted permission grants (issue #2524 item 3).
//!
//! `permission-log.jsonl` captured the right fields per line (`ts_claim`,
//! `conversation_id`, `tool`, `kind`, `target`, `decision`, `scope`) but, like
//! `denial-journal.jsonl` before #2085, was flat/ungrounded: no chain, no CID,
//! silently editable or truncatable. This module gives it the same treatment
//! [`crate::denial_journal`] already got — [`event_journal`]'s [`Journal`],
//! [`JournalLine`] and [`verify_chain`] over a [`PermissionRecord`] payload —
//! reusing the code rather than standing a third journal up beside it.
//!
//! # Two files, one encoding
//!
//! The grant log and the deny log stay **separate files**: merging them would
//! change *when* denial evidence is written (the denial journal is opt-in via
//! [`crate::denial_journal::DENIAL_JOURNAL_PATH_ENV`]; the permission log is
//! unconditional, matching its existing behavior) and would touch the denial
//! journal's own path, env var and tests for no behavioral gain. What this
//! module shares with `denial_journal` — the chain shape, the pre-chain
//! rotation, the append machinery — comes from [`event_journal`] itself, so
//! the two files are one encoding, not two.
//!
//! # Still a review artifact, not authority
//!
//! Same invariant as before migration: nothing reads this log back into
//! authority. The one reader (`/permissions audit`) displays it to a human.

use std::path::Path;

use crate::event_journal::{self, JournalLine};
use crate::PermissionRecord;

/// The chain's reader-side vocabulary, re-exported so a caller does not have
/// to know this journal is built on the event journal's machinery.
pub use crate::event_journal::{head_path, read_head, verify_chain, ChainBreak};

/// One chained permission-decision line: the record, its parent link, and the
/// address that covers both.
pub type PermissionLine = JournalLine<PermissionRecord>;

/// Append one decision as a chained line, rotating a pre-chain flat log aside
/// on first use and advancing the head ref beside it.
///
/// # Errors
///
/// Propagates a filesystem or canonical-encoding failure.
pub fn append_record(path: &Path, record: PermissionRecord) -> anyhow::Result<PermissionLine> {
    event_journal::rotate_pre_chain::<PermissionRecord>(path)?;
    let mut journal = event_journal::resume(path);
    event_journal::append_to(&mut journal, path, record)
}

/// Parse an append-only journal. Corrupt/partial lines are skipped, same as
/// [`denial_journal::read_jsonl`](crate::denial_journal::read_jsonl) — the
/// gap they leave behind is a [`ChainBreak::BrokenLink`] at the line after,
/// which [`verify_chain`] reports rather than hiding.
#[must_use]
pub fn read_jsonl(body: &str) -> Vec<PermissionLine> {
    event_journal::read_jsonl(body)
}

/// Payloads out of chained lines, newest-last (append order) — what a display
/// reader like `/permissions audit` wants, without exposing the chain shape.
#[must_use]
pub fn records(lines: &[PermissionLine]) -> Vec<PermissionRecord> {
    lines.iter().map(|l| l.node.payload().clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_journal::Journal;
    use crate::DenialKind;

    fn rec(target: &str) -> PermissionRecord {
        PermissionRecord::new(
            "conv-1",
            "run_command",
            DenialKind::Exec,
            target,
            "allow",
            "once",
        )
    }

    fn chain(n: usize) -> (Journal, Vec<PermissionLine>) {
        let mut journal = Journal::new();
        let lines = (0..n)
            .map(|i| journal.append(rec(&format!("cmd-{i}"))).expect("append"))
            .collect();
        (journal, lines)
    }

    #[test]
    fn an_intact_chain_of_grants_verifies() {
        let (journal, lines) = chain(4);
        let head = journal.head().expect("head").to_string();
        assert_eq!(verify_chain(&lines, Some(&head)), vec![]);
    }

    /// RED FIRST: a hand-edited line breaks `verify_chain`, exactly the
    /// property the flat file never had.
    #[test]
    fn a_hand_edited_line_fails_verification() {
        let (_, mut lines) = chain(3);
        lines[1].node.payload.target = "tampered".to_string();
        assert!(verify_chain(&lines, None).contains(&ChainBreak::Edited { index: 1 }));
    }

    #[test]
    fn a_deleted_record_fails_verification_though_every_survivor_is_intact() {
        let (_, mut lines) = chain(4);
        lines.remove(1);
        let breaks = verify_chain(&lines, None);
        assert!(breaks.contains(&ChainBreak::BrokenLink { index: 1 }));
        assert!(lines.iter().all(PermissionLine::is_intact));
    }

    #[test]
    fn a_truncated_tail_is_caught_only_by_the_head_ref() {
        let (journal, mut lines) = chain(5);
        let head = journal.head().expect("head").to_string();
        lines.truncate(2);
        assert_eq!(verify_chain(&lines, None), vec![]);
        assert_eq!(
            verify_chain(&lines, Some(&head)),
            vec![ChainBreak::Truncated {
                expected_head: head
            }],
        );
    }

    /// One encoding, not two: a legacy flat `PermissionRecord` line is not
    /// read back as a chained one.
    #[test]
    fn a_pre_chain_line_is_not_read_as_a_chained_record() {
        let legacy = serde_json::to_string(&rec("legacy")).unwrap();
        assert!(read_jsonl(&legacy).is_empty());
    }

    #[test]
    fn append_record_creates_dirs_and_chains_across_calls() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join("permission-log.jsonl");
        let first = append_record(&path, rec("npm")).unwrap();
        let second = append_record(&path, rec("docs.rs")).unwrap();
        assert_eq!(second.parent().unwrap().to_string(), first.id);

        let body = std::fs::read_to_string(&path).unwrap();
        let lines = read_jsonl(&body);
        assert_eq!(lines.len(), 2);
        assert_eq!(records(&lines)[1].target, "docs.rs");

        let head = read_head(&path).unwrap();
        assert_eq!(head, second.id);
        assert_eq!(verify_chain(&lines, Some(&head)), vec![]);
    }

    /// A flat pre-existing log is rotated aside once, then chaining starts
    /// clean — mirrors `denial_journal`'s migration behavior exactly.
    #[test]
    fn a_pre_existing_flat_log_is_rotated_aside_on_first_chained_append() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("permission-log.jsonl");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&rec("old")).unwrap()),
        )
        .unwrap();

        append_record(&path, rec("new")).unwrap();

        let pre_chain = event_journal::pre_chain_path(&path);
        assert!(pre_chain.exists(), "old flat log kept, moved aside");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines = read_jsonl(&body);
        assert_eq!(
            lines.len(),
            1,
            "chain starts fresh, does not swallow the flat line"
        );
        assert_eq!(records(&lines)[0].target, "new");
    }
}
