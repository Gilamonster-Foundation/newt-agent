//! Real-filesystem grounding for the chained event journal (#2085).
//!
//! # What this grounds
//!
//! `event_journal`'s unit tier is fully fs-free: it builds chains in memory and
//! tampers with them in memory. That proves [`verify_chain`] reasons correctly
//! about a chain — and proves nothing about whether the bytes that reach a real
//! file reconstruct into the chain that was written.
//!
//! These tests close exactly that gap, which is the whole job of the expensive
//! tier per CLAUDE.md: **mocked stays the gate, and a real-resource test proves
//! the gate is measuring reality.** Specifically they ground:
//!
//! - the in-memory round-trip test (`a_line_round_trips_through_jsonl`) — here
//!   the JSONL actually goes through the filesystem and back;
//! - `Journal::resuming_from`, by resuming from what a previous *process* wrote
//!   rather than from a head handed over in a local variable;
//! - the truncation case, which is the one guarantee that depends on a second
//!   file existing and staying in step with the first.
//!
//! `#[serial]` for the reason CLAUDE.md gives for the whole expensive tier:
//! real-resource tests contend under parallel load and intermittently fail
//! tempdir creation with `Permission denied`, which aborts the entire test
//! binary rather than one test. They do NOT touch `$NEWT_EVENT_JOURNAL` — every
//! path here is an explicit tempdir, so no process-global is involved.

use newt_core::event_journal::{
    self, append_to, read_jsonl, resume, verify_chain, ChainBreak, EventKind, Journal, JournalEvent,
};
use serial_test::serial;
use std::path::Path;

fn event(subject: &str) -> JournalEvent {
    JournalEvent::new(EventKind::Grant, subject, "allow", "/settings")
}

fn body(path: &Path) -> String {
    std::fs::read_to_string(path).expect("journal readable")
}

/// Append three events through a real file, then verify what came back off
/// disk — including against the head ref that was written beside it.
#[test]
#[serial]
fn a_chain_written_to_a_real_file_verifies_when_read_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("events.jsonl");

    let mut journal = Journal::new();
    for i in 0..3 {
        append_to(&mut journal, &path, event(&format!("tool-{i}"))).expect("append");
    }

    let lines = read_jsonl(&body(&path));
    assert_eq!(lines.len(), 3);

    let head = event_journal::read_head(&path).expect("head ref written");
    assert_eq!(
        verify_chain(&lines, Some(&head)),
        vec![],
        "a chain that made a real round trip through the filesystem must verify"
    );
}

/// The head ref is a **separate file**, which is what makes the truncation case
/// detectable at all — a head stored inside the journal would be truncated
/// along with it.
#[test]
#[serial]
fn the_head_ref_lands_in_its_own_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("events.jsonl");

    let mut journal = Journal::new();
    let line = append_to(&mut journal, &path, event("tool")).expect("append");

    let head_file = event_journal::head_path(&path);
    assert!(head_file.exists(), "the ref is its own file");
    assert_ne!(head_file, path);
    assert_eq!(
        std::fs::read_to_string(&head_file)
            .expect("readable")
            .trim(),
        line.id
    );
}

/// Resuming reads the head off disk and extends the existing chain, rather
/// than starting a second one beside it.
#[test]
#[serial]
fn a_second_session_extends_the_chain_it_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("events.jsonl");

    let mut first = Journal::new();
    append_to(&mut first, &path, event("before")).expect("append");

    // `second` is built by `resume(&path)` alone — the head is not handed over
    // from `first`, so the only way it can link correctly is off the disk.
    let mut second = resume(&path);
    append_to(&mut second, &path, event("after")).expect("append");

    let lines = read_jsonl(&body(&path));
    assert_eq!(lines.len(), 2, "one file, one chain");
    let head = event_journal::read_head(&path).expect("head");
    assert_eq!(verify_chain(&lines, Some(&head)), vec![]);
}

/// With the ref deleted, resume falls back to the journal's last line — so a
/// lost ref degrades to "cannot detect truncation" rather than to a forked
/// chain, which would be the worse failure.
#[test]
#[serial]
fn a_lost_head_ref_still_resumes_from_the_last_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("events.jsonl");

    let mut first = Journal::new();
    append_to(&mut first, &path, event("before")).expect("append");
    std::fs::remove_file(event_journal::head_path(&path)).expect("remove ref");
    assert!(event_journal::read_head(&path).is_none());

    let mut second = resume(&path);
    append_to(&mut second, &path, event("after")).expect("append");

    let lines = read_jsonl(&body(&path));
    assert_eq!(verify_chain(&lines, None), vec![], "still one chain");
}

/// The tamper case that motivates the whole design, performed on a real file
/// with a real text editor's worth of damage: delete a line from the middle.
#[test]
#[serial]
fn deleting_a_line_from_the_real_file_is_caught() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("events.jsonl");

    let mut journal = Journal::new();
    for i in 0..4 {
        append_to(&mut journal, &path, event(&format!("tool-{i}"))).expect("append");
    }

    let written = body(&path);
    let kept: Vec<&str> = written
        .lines()
        .enumerate()
        .filter_map(|(i, line)| (i != 1).then_some(line))
        .collect();
    std::fs::write(&path, kept.join("\n")).expect("rewrite");

    let lines = read_jsonl(&body(&path));
    assert_eq!(lines.len(), 3);
    assert!(
        lines
            .iter()
            .all(newt_core::event_journal::JournalLine::is_intact),
        "every surviving line is individually intact — a per-row check passes here"
    );
    assert!(
        verify_chain(&lines, None).contains(&ChainBreak::BrokenLink { index: 1 }),
        "and the chain catches it anyway"
    );
}

/// Truncating the tail of a real file: valid on its own terms, caught only by
/// the ref that was written separately.
#[test]
#[serial]
fn truncating_the_real_file_is_caught_by_the_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("events.jsonl");

    let mut journal = Journal::new();
    for i in 0..5 {
        append_to(&mut journal, &path, event(&format!("tool-{i}"))).expect("append");
    }
    let head = event_journal::read_head(&path).expect("head");

    let kept: Vec<String> = body(&path).lines().take(2).map(str::to_string).collect();
    std::fs::write(&path, kept.join("\n")).expect("rewrite");

    let lines = read_jsonl(&body(&path));
    assert_eq!(verify_chain(&lines, None), vec![], "valid on its own terms");
    assert_eq!(
        verify_chain(&lines, Some(&head)),
        vec![ChainBreak::Truncated {
            expected_head: head
        }],
    );
}

/// The journal writes into a directory that does not exist yet — the first
/// grant of a fresh install must not be the one that is lost.
#[test]
#[serial]
fn the_parent_directory_is_created_on_demand() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nested/deeper/events.jsonl");

    let mut journal = Journal::new();
    append_to(&mut journal, &path, event("tool")).expect("append");

    assert_eq!(read_jsonl(&body(&path)).len(), 1);
}
