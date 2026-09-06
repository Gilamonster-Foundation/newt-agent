use super::*;

// Spans prompt receipts, the writer clock, and turn-chain integrity.
#[test]
fn prompt_ticks_reseed_the_writer_clock_without_moving_the_turn_chain_tip() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conv = "prompt-clock";

    let first = store
        .begin_prompt(
            conv,
            "clock",
            None,
            crate::prompt::NewPrompt::operator("one", "one"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    let (writer_before, tip_before): (String, String) = {
        let conn = store.lock_conn();
        conn.query_row(
            "SELECT writer_fingerprint, tip_hash FROM conversations WHERE id = ?1",
            [conv],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };

    // Simulate a lost writer_clock row. Reseeding must observe prompt seq,
    // not only completed turns and conversation activity.
    {
        let conn = store.lock_conn();
        conn.execute(
            "DELETE FROM writer_clock WHERE writer_fingerprint = ?1",
            [store.writer_fingerprint()],
        )
        .unwrap();
    }
    let second = store
        .begin_prompt(
            conv,
            "clock",
            None,
            crate::prompt::NewPrompt::operator("two", "two"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    assert!(second.seq() > first.seq());

    let (writer_after, tip_after): (String, String) = {
        let conn = store.lock_conn();
        conn.query_row(
            "SELECT writer_fingerprint, tip_hash FROM conversations WHERE id = ?1",
            [conv],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(writer_after, writer_before);
    assert_eq!(tip_after, tip_before);
}

// Spans prompt retention, owner liveness, and stale-claim reclamation.
#[test]
fn prompt_retention_skips_live_owners_but_reclaims_stale_claims() {
    fn never_live(_owner: &StoredOwner, _now: i64) -> bool {
        false
    }

    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = ConversationStore::new(root.path(), workspace.path(), 1).unwrap();
    let first = store
        .begin_prompt(
            "live-retention-owner",
            "first",
            None,
            crate::prompt::NewPrompt::operator("first", "first"),
        )
        .unwrap();
    assert_eq!(
        store.claim("live-retention-owner").unwrap(),
        ClaimOutcome::Claimed
    );

    store
        .begin_prompt(
            "protected-new-prompt",
            "second",
            None,
            crate::prompt::NewPrompt::operator("second", "second"),
        )
        .unwrap();
    assert!(store.exists("live-retention-owner").unwrap());
    assert!(store.exists("protected-new-prompt").unwrap());
    assert!(store.load_prompt(first.submitted().id()).unwrap().is_some());

    // A crashed owner's row must not pin the conversation forever. The
    // next retention transaction uses the same liveness judgement as
    // `claim`, removes the stale owner, and reclaims the oldest rows.
    store.set_liveness_for_test(never_live);
    store
        .begin_prompt(
            "third-prompt",
            "third",
            None,
            crate::prompt::NewPrompt::operator("third", "third"),
        )
        .unwrap();
    assert!(!store.exists("live-retention-owner").unwrap());
    assert!(!store.exists("protected-new-prompt").unwrap());
    assert!(store.exists("third-prompt").unwrap());
    assert!(store.live_owner("live-retention-owner").unwrap().is_none());
}

// Spans conversation creation and accepted-prompt immutability.
#[test]
fn create_with_id_cannot_implicitly_erase_an_accepted_prompt() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "prompt-cannot-be-replaced";
    let accepted = store
        .begin_prompt(
            conversation_id,
            "original",
            None,
            crate::prompt::NewPrompt::operator("raw", "model"),
        )
        .unwrap();

    let error = store
        .create_with_id(conversation_id, "replacement", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("immutable prompt receipts"), "{error}");
    assert!(store
        .load_prompt_in_conversation(conversation_id, accepted.submitted().id())
        .unwrap()
        .is_some());
    assert_eq!(store.load(conversation_id).unwrap().title, "original");
}
