use super::*;

// =========================================================================
// Part 4 — 17.5: StoreRecallSource, the `recall` model tool's store backend
// (workspace-fenced by the store, current conversation excluded).
// =========================================================================

#[test]
fn store_recall_source_excludes_the_current_conversation() {
    use newt_core::{RecallSource as _, StoreRecallSource};

    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    // The conversation the model is "in" — its turns must never come back.
    let current = store.create("current work", None).unwrap();
    store
        .append_turn(&current, "fix the tokio panic", "patched the tokio panic")
        .unwrap();
    // A past conversation on the same topic — this is what recall is for.
    let past = store.create("past work", None).unwrap();
    store
        .append_turn(
            &past,
            "we hit a tokio panic in retry",
            "bounded the retries",
        )
        .unwrap();

    // The raw store search sees both conversations …
    let raw = store.search("tokio panic", 10).unwrap();
    assert!(raw.iter().any(|h| h.conversation_id == current));
    assert!(raw.iter().any(|h| h.conversation_id == past));

    // … the recall source sees only the past one.
    let source = StoreRecallSource::new(&store, &current);
    let hits = source.search("tokio panic", 5).unwrap();
    assert!(!hits.is_empty(), "the past conversation must match");
    assert!(
        hits.iter().all(|h| h.conversation_id == past),
        "current conversation leaked into recall: {hits:?}"
    );
}

#[test]
fn store_recall_source_truncates_to_limit_after_exclusion() {
    use newt_core::{RecallSource as _, StoreRecallSource};

    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let current = store.create("current", None).unwrap();
    store
        .append_turn(&current, "fts5 ranking question", "fts5 ranking answer")
        .unwrap();
    for i in 0..4 {
        let id = store.create(&format!("past {i}"), None).unwrap();
        store
            .append_turn(&id, &format!("fts5 ranking case {i}"), "noted")
            .unwrap();
    }

    let source = StoreRecallSource::new(&store, &current);
    let hits = source.search("fts5 ranking", 2).unwrap();
    assert_eq!(hits.len(), 2, "limit applies after exclusion: {hits:?}");
    assert!(hits.iter().all(|h| h.conversation_id != current));
}

#[test]
fn this_conversation_recent_returns_the_current_conversations_own_last_turns() {
    // #714: the OPPOSITE of search's filter — `resume_context` must read THIS
    // conversation's own pre-interrupt turns (the affordance recall refuses).
    use newt_core::{RecallSource as _, StoreRecallSource};

    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let current = store.create("resumed work", None).unwrap();
    for i in 0..5 {
        store
            .append_turn(&current, &format!("ask {i}"), &format!("reply {i}"))
            .unwrap();
    }
    // A different conversation must NOT leak in (this is a self-read, fenced to
    // the current id — the mirror of search's exclusion).
    let other = store.create("other work", None).unwrap();
    store.append_turn(&other, "unrelated", "unrelated").unwrap();

    let source = StoreRecallSource::new(&store, &current);
    let hits = source.this_conversation_recent(3).unwrap();
    assert_eq!(hits.len(), 3, "last `limit` turns only: {hits:?}");
    // All hits belong to THIS conversation — the opposite of recall's filter.
    assert!(
        hits.iter().all(|h| h.conversation_id == current),
        "another conversation leaked into the self-read: {hits:?}"
    );
    // Oldest-first within the window (turns 2,3,4 of 0..5), seq = 1-based pos.
    assert!(hits[0].snippet.contains("ask 2") && hits[0].snippet.contains("reply 2"));
    assert!(hits[2].snippet.contains("ask 4"));
    assert_eq!(hits[0].seq, 3, "1-based position of turn index 2");
    assert_eq!(hits[2].seq, 5);

    // limit 0 → empty (the guard), and a limit past the end clamps to all turns.
    assert!(source.this_conversation_recent(0).unwrap().is_empty());
    assert_eq!(source.this_conversation_recent(99).unwrap().len(), 5);
}
