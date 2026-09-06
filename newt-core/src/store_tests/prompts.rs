use super::*;

// Durable prompt receipts: write-before-work provenance. These tests are
// intentionally store-level because a receipt must survive even when no
// assistant turn is ever appended.
#[test]
fn prompt_receipt_is_byte_exact_and_survives_an_incomplete_turn() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let conversation_id = "prompt-byte-exact";

    let receipt = {
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let prompt = crate::prompt::NewPrompt::operator(
            b"raw\0bytes\xff".to_vec(),
            "model text\nwith Unicode: \u{1f9ad}".as_bytes().to_vec(),
        );
        store
            .begin_prompt(conversation_id, "prompt title", None, prompt)
            .unwrap()
            .submitted()
            .receipt()
            .clone()
    };

    // Reopen the database: the prompt is durable despite there being no
    // completed `turns` row at all.
    let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let loaded = reopened
        .load_prompt_in_conversation(conversation_id, receipt.id())
        .unwrap()
        .expect("prompt receipt survives a failed/interrupted turn");
    assert_eq!(loaded.raw_text(), b"raw\0bytes\xff");
    assert_eq!(
        loaded.model_text_utf8().unwrap(),
        "model text\nwith Unicode: \u{1f9ad}"
    );
    loaded.verify_integrity().unwrap();
    assert!(reopened.load(conversation_id).unwrap().turns.is_empty());
}

#[test]
fn prompt_reads_are_conversation_and_workspace_fenced_and_delete_cascades() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

    let a = store_a
        .begin_prompt(
            "conversation-a",
            "A",
            None,
            crate::prompt::NewPrompt::operator("secret-a", "secret-a"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    let cross_workspace_append = store_b
        .begin_prompt(
            "conversation-a",
            "foreign",
            None,
            crate::prompt::NewPrompt::operator("intruder", "intruder"),
        )
        .unwrap_err()
        .to_string();
    assert!(
        cross_workspace_append.contains("belongs to another workspace"),
        "{cross_workspace_append}"
    );
    let b = store_b
        .begin_prompt(
            "conversation-b",
            "B",
            None,
            crate::prompt::NewPrompt::operator("secret-b", "secret-b"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();

    assert!(store_b.load_prompt(a.id()).unwrap().is_none());
    assert!(store_a
        .load_prompt_in_conversation("conversation-a", b.id())
        .unwrap()
        .is_none());
    assert!(store_b.latest_prompt("conversation-a").unwrap().is_none());
    assert!(store_b.prompt_chain("conversation-a").unwrap().is_empty());
    assert!(store_b
        .turn_prompt_context("conversation-a", a.id())
        .unwrap()
        .is_none());
    assert!(store_a.previous_prompt(b.id()).unwrap().is_none());

    store_a.delete("conversation-a").unwrap();
    assert!(store_a.load_prompt(a.id()).unwrap().is_none());
}

#[test]
fn begin_prompt_rolls_back_lazy_conversation_when_ancestry_is_invalid() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let missing = crate::prompt::PromptId::new();

    let err = store
        .begin_prompt(
            "atomic-invalid-parent",
            "must roll back",
            None,
            crate::prompt::NewPrompt::harness_retry("raw", "model", missing),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not in conversation"), "{err}");
    assert!(!store.exists("atomic-invalid-parent").unwrap());
    assert!(store.load_prompt(missing).unwrap().is_none());
}

#[test]
fn begin_prompt_rejects_non_utf8_model_bytes_before_creating_a_conversation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let error = store
        .begin_prompt(
            "invalid-model-encoding",
            "must not persist",
            None,
            crate::prompt::NewPrompt::operator(b"raw may be bytes".to_vec(), vec![0xff]),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("not valid UTF-8"), "{error}");
    assert!(!store.exists("invalid-model-encoding").unwrap());
}

#[test]
fn prompt_retention_never_prunes_the_receipt_it_just_accepted() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 1).unwrap();

    store
        .begin_prompt(
            "older-conversation",
            "older",
            None,
            crate::prompt::NewPrompt::operator("older", "older"),
        )
        .unwrap();

    // Simulate an existing writer clock lagging another writer's observed
    // activity. Without an explicit exclusion, the newly accepted row's
    // low tick makes it the apparent oldest retention victim.
    {
        let conn = store.lock_conn();
        conn.execute(
            "UPDATE conversations SET activity_tick = 100 WHERE id = ?1",
            ["older-conversation"],
        )
        .unwrap();
        conn.execute(
            "UPDATE writer_clock SET last_tick = 0 WHERE writer_fingerprint = ?1",
            [store.writer_fingerprint()],
        )
        .unwrap();
    }

    let accepted = store
        .begin_prompt(
            "newly-accepted",
            "new",
            None,
            crate::prompt::NewPrompt::operator("new", "new"),
        )
        .unwrap();

    assert!(store.exists("newly-accepted").unwrap());
    assert!(store
        .load_prompt_in_conversation("newly-accepted", accepted.submitted().id())
        .unwrap()
        .is_some());
    assert!(!store.exists("older-conversation").unwrap());
}

#[test]
fn post_commit_prune_failure_does_not_report_prompt_failure() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 1).unwrap();
    store
        .begin_prompt(
            "retention-first",
            "first",
            None,
            crate::prompt::NewPrompt::operator("first", "first"),
        )
        .unwrap();

    // Make the cap's live-owner exclusion query fail deterministically
    // after the next receipt commits. Prompt acceptance must remain Ok.
    {
        let conn = store.lock_conn();
        conn.execute_batch("ALTER TABLE live_owners RENAME TO broken_live_owners")
            .unwrap();
    }
    let accepted = store
        .begin_prompt(
            "retention-second",
            "second",
            None,
            crate::prompt::NewPrompt::operator("second", "second"),
        )
        .expect("post-commit housekeeping cannot negate prompt acceptance");
    let loaded = store
        .load_prompt(accepted.submitted().id())
        .unwrap()
        .expect("committed receipt remains readable");
    assert_eq!(loaded.model_text_utf8().unwrap(), "second");
}

#[test]
fn prompt_load_rejects_tampered_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let receipt = store
        .begin_prompt(
            "tamper",
            "title",
            None,
            crate::prompt::NewPrompt::operator("raw", "model"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    {
        let conn = store.lock_conn();
        conn.execute(
            "UPDATE prompt_receipts SET model_text = ?2 WHERE id = ?1",
            rusqlite::params![receipt.id().to_string(), b"changed".as_slice()],
        )
        .unwrap();
    }
    let err = store.load_prompt(receipt.id()).unwrap_err().to_string();
    assert!(err.contains("model-text digest mismatch"), "{err}");
}
