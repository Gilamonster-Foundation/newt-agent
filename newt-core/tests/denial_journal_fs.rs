//! Real-filesystem grounding for the chained denial journal (#2085).
//!
//! # What this grounds
//!
//! `denial_journal`'s unit tier is fs-free: it chains records in memory and
//! tampers with them in memory. That proves `verify_chain` reasons correctly
//! about a run of denial records, and proves nothing about the two things this
//! module does to a real file — advancing a **head ref** beside the journal,
//! and **rotating a pre-chain journal aside** on first chained append. Both are
//! fs facts; neither can be observed without a filesystem.
//!
//! It also grounds the module's central claim end-to-end: a denial recorded
//! through the production write path, read back off disk, verifies — and stops
//! verifying when a record is removed from the file with an editor.
//!
//! Expensive tier per CLAUDE.md — real fs, `#[serial]`, because real-resource
//! tests contend under parallel load and fail tempdir creation in ways that
//! abort the whole binary. No process-global env is touched: every path here is
//! an explicit tempdir, so `NEWT_DENIAL_JOURNAL` is never involved.

use newt_core::denial_journal::{
    self, append_record, read_jsonl, verify_chain, ChainBreak, DenialRecord, DenialStage,
};
use serial_test::serial;
use std::path::Path;

fn envelope() -> serde_json::Value {
    serde_json::json!({
        "denied": true,
        "denials": [{
            "kind": "exec",
            "target": "confinement",
            "reason": "exec of \"confinement\" is not within the granted authority",
        }],
    })
}

fn record(command: &str) -> DenialRecord {
    DenialRecord::from_envelope(command, "/workspace", DenialStage::Initial, &envelope())
        .expect("structured denial")
}

fn body(path: &Path) -> String {
    std::fs::read_to_string(path).expect("journal readable")
}

/// Three denials through the real write path, read back and verified against
/// the ref that was written beside them.
#[test]
#[serial]
fn denials_written_to_a_real_file_verify_against_the_head_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("denial-journal.jsonl");

    for i in 0..3 {
        append_record(&path, record(&format!("wc -l {i}.rs"))).expect("append");
    }

    let lines = read_jsonl(&body(&path));
    assert_eq!(lines.len(), 3);
    let head = denial_journal::read_head(&path).expect("head ref written beside the journal");
    assert_eq!(verify_chain(&lines, Some(&head)), vec![]);
}

/// The tamper the journal exists to survive, on a real file: an operator
/// deletes the record showing their grant did not work.
#[test]
#[serial]
fn deleting_a_denial_from_the_real_file_is_caught() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("denial-journal.jsonl");
    for i in 0..3 {
        append_record(&path, record(&format!("wc -l {i}.rs"))).expect("append");
    }

    let text = body(&path);
    let kept: Vec<&str> = text
        .lines()
        .enumerate()
        .filter(|(i, _)| *i != 1)
        .map(|(_, line)| line)
        .collect();
    std::fs::write(&path, kept.join("\n")).expect("rewrite");

    let lines = read_jsonl(&body(&path));
    assert_eq!(lines.len(), 2);
    assert!(
        verify_chain(&lines, None).contains(&ChainBreak::BrokenLink { index: 1 }),
        "a record removed from a real file must break the chain"
    );
}

/// The migration, stated as behaviour: a journal written before the chain is
/// moved aside — not silently mixed into a chain it was never part of, and not
/// deleted.
#[test]
#[serial]
fn a_pre_chain_journal_is_rotated_aside_and_a_fresh_chain_starts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("denial-journal.jsonl");
    // Exactly what the old code wrote: bare records, one per line, no address.
    let legacy = format!(
        "{}\n{}\n",
        serde_json::to_string(&record("wc -l old-a.rs")).unwrap(),
        serde_json::to_string(&record("wc -l old-b.rs")).unwrap(),
    );
    std::fs::write(&path, &legacy).expect("seed legacy journal");

    append_record(&path, record("wc -l new.rs")).expect("append");

    let rotated = denial_journal::pre_chain_path(&path);
    assert_eq!(
        std::fs::read_to_string(&rotated).expect("rotated aside, not deleted"),
        legacy,
        "the pre-chain bytes are preserved verbatim"
    );

    let lines = read_jsonl(&body(&path));
    assert_eq!(lines.len(), 1, "the new journal holds only the new chain");
    let head = denial_journal::read_head(&path).expect("head");
    assert_eq!(verify_chain(&lines, Some(&head)), vec![]);
}

/// Rotation happens once. A second append must extend the chain, not move the
/// journal it just wrote.
#[test]
#[serial]
fn an_already_chained_journal_is_never_rotated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("denial-journal.jsonl");
    append_record(&path, record("wc -l a.rs")).expect("append");
    append_record(&path, record("wc -l b.rs")).expect("append");

    assert!(!denial_journal::pre_chain_path(&path).exists());
    assert_eq!(read_jsonl(&body(&path)).len(), 2);
}

/// A chained journal whose head ref was lost is still a chain, and is resumed
/// from its last line rather than rotated away as if it were pre-chain.
#[test]
#[serial]
fn a_lost_head_ref_resumes_the_chain_instead_of_rotating_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("denial-journal.jsonl");
    append_record(&path, record("wc -l a.rs")).expect("append");
    std::fs::remove_file(denial_journal::head_path(&path)).expect("remove ref");

    append_record(&path, record("wc -l b.rs")).expect("append");

    assert!(!denial_journal::pre_chain_path(&path).exists());
    let lines = read_jsonl(&body(&path));
    assert_eq!(lines.len(), 2);
    assert_eq!(verify_chain(&lines, None), vec![], "still one chain");
}
