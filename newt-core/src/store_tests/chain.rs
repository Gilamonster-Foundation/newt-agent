use super::*;

fn insert_prompt_lineage_for_test(
    store: &ConversationStore,
    conversation_id: &str,
    depth: usize,
) -> (PromptId, PromptId) {
    assert!(depth >= 1);
    store
        .create_with_id(conversation_id, "lineage test", None)
        .unwrap();
    let writer = store.writer_fingerprint().to_string();
    let root_id = PromptId::new();
    let root = PromptReceipt::new(
        root_id,
        conversation_id.to_string(),
        writer.clone(),
        1,
        None,
        None,
        root_id,
        root_id,
        PromptOrigin::Operator,
        b"root".to_vec(),
        b"root".to_vec(),
        1,
    );
    let conn = store.lock_conn();
    let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
    insert_prompt_receipt(&tx, &root).unwrap();
    let mut previous_id = root_id;
    for index in 1..depth {
        let id = PromptId::new();
        let text = format!("retry-{index}").into_bytes();
        let retry = PromptReceipt::new(
            id,
            conversation_id.to_string(),
            writer.clone(),
            i64::try_from(index + 1).unwrap(),
            Some(previous_id),
            Some(previous_id),
            root_id,
            root_id,
            PromptOrigin::HarnessRetry,
            text.clone(),
            text,
            i64::try_from(index + 1).unwrap(),
        );
        insert_prompt_receipt(&tx, &retry).unwrap();
        previous_id = id;
    }
    tx.commit().unwrap();
    (root_id, previous_id)
}

#[test]
fn prompt_chronology_is_automatic_but_objective_parentage_is_explicit() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conv = "prompt-ancestry";

    let first = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator("first", "first"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    assert_eq!(first.root_prompt_id(), first.id());
    assert_eq!(first.previous_prompt_id(), None);
    assert_eq!(first.parent_prompt_id(), None);

    // A normal new operator prompt is chronologically after `first`, but
    // is a new objective root. Chronology must never silently become
    // semantic parentage.
    let second = store
        .begin_prompt(
            conv,
            "ignored on existing conversation",
            None,
            crate::prompt::NewPrompt::operator("second", "second"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    assert_eq!(second.previous_prompt_id(), Some(first.id()));
    assert_eq!(second.parent_prompt_id(), None);
    assert_eq!(second.root_prompt_id(), second.id());

    // A harness retry is an explicit child. It inherits the validated
    // parent root, while the active operator prompt remains `first`.
    let retry = store
        .begin_prompt(
            conv,
            "ignored",
            None,
            crate::prompt::NewPrompt::harness_retry("retry", "retry", first.id()),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    assert_eq!(retry.previous_prompt_id(), Some(second.id()));
    assert_eq!(retry.parent_prompt_id(), Some(first.id()));
    assert_eq!(retry.root_prompt_id(), first.id());

    let context = store
        .turn_prompt_context(conv, retry.id())
        .unwrap()
        .expect("retry context");
    assert_eq!(context.submitted_prompt().id(), retry.id());
    assert_eq!(context.active_operator_prompt().id(), first.id());

    assert_eq!(
        store.prompt_chain(conv).unwrap(),
        vec![first, second, retry]
    );
}

#[test]
fn mutable_receipt_order_cannot_reparent_the_verified_prompt_chain() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conv = "prompt-order-tamper";
    let first = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator("first", "first"),
        )
        .unwrap()
        .submitted()
        .id();
    let second = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator("second", "second"),
        )
        .unwrap()
        .submitted()
        .id();

    // Swap only the unhashed SQLite presentation order. Both receipts and
    // their hashed predecessor links remain individually valid.
    store
        .lock_conn()
        .execute(
            "UPDATE prompt_receipts
                    SET receipt_order = CASE id WHEN ?1 THEN 2002 WHEN ?2 THEN 2001 END
                  WHERE id IN (?1, ?2)",
            rusqlite::params![first.to_string(), second.to_string()],
        )
        .unwrap();

    let latest_error = store.latest_prompt(conv).unwrap_err().to_string();
    assert!(
        latest_error.contains("prompt chronology mismatch"),
        "{latest_error}"
    );
    let append_error = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator("third", "third"),
        )
        .unwrap_err()
        .to_string();
    assert!(
        append_error.contains("prompt chronology mismatch"),
        "{append_error}"
    );
    let receipt_count: i64 = store
        .lock_conn()
        .query_row(
            "SELECT COUNT(*) FROM prompt_receipts WHERE conversation_id = ?1",
            [conv],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(receipt_count, 2, "failed append must roll back completely");
}

#[test]
fn concurrent_store_connections_serialize_prompt_predecessors() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let conv = "concurrent-prompt-append";
    let seed_store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let seed = seed_store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator("seed", "seed"),
        )
        .unwrap()
        .submitted()
        .id();
    drop(seed_store);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut workers = Vec::new();
    for label in ["left", "right"] {
        let root_path = root.path().to_path_buf();
        let workspace_path = workspace.path().to_path_buf();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let store = ConversationStore::new(root_path, workspace_path, 100).unwrap();
            barrier.wait();
            store
                .begin_prompt(
                    conv,
                    "title",
                    None,
                    crate::prompt::NewPrompt::operator(label, label),
                )
                .unwrap()
                .submitted()
                .id()
        }));
    }
    barrier.wait();
    let appended: Vec<PromptId> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();

    let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let chain = reopened.prompt_chain(conv).unwrap();
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0].id(), seed);
    assert_eq!(chain[1].previous_prompt_id(), Some(seed));
    assert_eq!(chain[2].previous_prompt_id(), Some(chain[1].id()));
    assert!(appended.contains(&chain[1].id()));
    assert!(appended.contains(&chain[2].id()));
}

#[test]
fn prompt_lineage_accepts_the_documented_depth_boundary() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "lineage-at-boundary";
    let (root_id, leaf_id) =
        insert_prompt_lineage_for_test(&store, conversation_id, MAX_PROMPT_LINEAGE_DEPTH);

    let context = store
        .turn_prompt_context(conversation_id, leaf_id)
        .unwrap()
        .expect("a lineage exactly at the documented limit is valid");
    assert_eq!(context.active().id(), root_id);
}

#[test]
fn prompt_lineage_rejects_a_deeper_retry_before_inserting_it() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "lineage-over-boundary";
    let (_, leaf_id) =
        insert_prompt_lineage_for_test(&store, conversation_id, MAX_PROMPT_LINEAGE_DEPTH);

    let error = store
        .begin_prompt(
            conversation_id,
            "ignored",
            None,
            crate::prompt::NewPrompt::harness_retry("too deep", "too deep", leaf_id),
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("maximum prompt lineage depth"), "{error}");
    assert_eq!(
        store.prompt_chain(conversation_id).unwrap().len(),
        MAX_PROMPT_LINEAGE_DEPTH
    );
}

#[test]
fn prompt_lineage_rejects_a_persisted_chain_over_the_depth_limit() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "persisted-lineage-over-boundary";
    let (_, leaf_id) =
        insert_prompt_lineage_for_test(&store, conversation_id, MAX_PROMPT_LINEAGE_DEPTH + 1);

    let error = store
        .turn_prompt_context(conversation_id, leaf_id)
        .unwrap_err()
        .to_string();
    assert!(error.contains("exceeds the maximum depth"), "{error}");
}

#[test]
fn prompt_lineage_cycle_is_detected_without_recursion() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conversation_id = "lineage-cycle";
    store
        .create_with_id(conversation_id, "cycle test", None)
        .unwrap();

    let writer = store.writer_fingerprint().to_string();
    let root_id = PromptId::new();
    let retry_a_id = PromptId::new();
    let retry_b_id = PromptId::new();
    let root_receipt = PromptReceipt::new(
        root_id,
        conversation_id.to_string(),
        writer.clone(),
        1,
        None,
        None,
        root_id,
        root_id,
        PromptOrigin::Operator,
        b"root".to_vec(),
        b"root".to_vec(),
        1,
    );
    let retry_a = PromptReceipt::new(
        retry_a_id,
        conversation_id.to_string(),
        writer.clone(),
        2,
        Some(root_id),
        Some(retry_b_id),
        root_id,
        root_id,
        PromptOrigin::HarnessRetry,
        b"retry-a".to_vec(),
        b"retry-a".to_vec(),
        2,
    );
    let retry_b = PromptReceipt::new(
        retry_b_id,
        conversation_id.to_string(),
        writer,
        3,
        Some(retry_a_id),
        Some(retry_a_id),
        root_id,
        root_id,
        PromptOrigin::HarnessRetry,
        b"retry-b".to_vec(),
        b"retry-b".to_vec(),
        3,
    );
    {
        let conn = store.lock_conn();
        let tx =
            rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        tx.execute_batch("PRAGMA defer_foreign_keys = ON").unwrap();
        insert_prompt_receipt(&tx, &root_receipt).unwrap();
        insert_prompt_receipt(&tx, &retry_a).unwrap();
        insert_prompt_receipt(&tx, &retry_b).unwrap();
        tx.commit().unwrap();
    }

    let error = store
        .turn_prompt_context(conversation_id, retry_b_id)
        .unwrap_err()
        .to_string();
    assert!(error.contains("prompt parent cycle detected"), "{error}");
}

/// bug/steering-regressions: this test previously pinned the OPPOSITE
/// contract ("…but_is_itself_active") — a continuation usurped the parent
/// ask as the active operator prompt, so the protected active-prompt card
/// carried decision ceremony ("1: proceed") and mid-turn compaction
/// evicted the real task (live gpt-4.1 + Qwen3-Coder drives, 2026-07-26/27).
/// A continuation refines the parent objective; the parent stays active.
#[test]
fn operator_continuation_inherits_root_and_parent_stays_active() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conv = "operator-continuation";
    let root_prompt = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator("root", "root"),
        )
        .unwrap()
        .submitted()
        .receipt()
        .clone();
    let continuation = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator_continuation(
                "continue",
                "continue",
                root_prompt.id(),
            ),
        )
        .unwrap();
    assert_eq!(continuation.submitted().root_prompt_id(), root_prompt.id());
    assert_eq!(
        continuation.active().id(),
        root_prompt.id(),
        "a continuation must not usurp the parent ask as the active \
             operator prompt — the task the card protects lives there"
    );
}

#[test]
fn retries_preserve_nearest_operator_authority_across_reopen() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let conv = "continuation-retry-authority";

    let (a_id, _b_id, retry_id, retry_again_id) = {
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let a = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("A", "A root objective"),
            )
            .unwrap();
        let b = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator_continuation(
                    "B",
                    "B locked clarification",
                    a.active().id(),
                ),
            )
            .unwrap();
        let retry = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::harness_retry("retry B", "retry B", b.submitted().id()),
            )
            .unwrap();
        let retry_again = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::harness_retry(
                    "retry retry B",
                    "retry retry B",
                    retry.submitted().id(),
                ),
            )
            .unwrap();

        assert_eq!(b.submitted().root_prompt_id(), a.submitted().id());
        // bug/steering-regressions: b is a CONTINUATION of a — a remains
        // the active authority; retries through b resolve to a as well.
        assert_eq!(b.active().id(), a.submitted().id());
        assert_eq!(retry.submitted().root_prompt_id(), a.submitted().id());
        assert_eq!(retry.active().id(), a.submitted().id());
        assert_eq!(retry_again.active().id(), a.submitted().id());

        // Simulate receipts written by the v1 schema: no persisted active
        // pointer, and the canonical v1 hash. Reopen must recover the same
        // nearest authority by walking explicit parents. Keep the final
        // retry at v2 to prove mixed-version ancestry works too.
        for id in [b.submitted().id(), retry.submitted().id()] {
            let legacy = store
                .load_prompt(id)
                .unwrap()
                .unwrap()
                .into_legacy_v1_for_test();
            let conn = store.lock_conn();
            conn.execute(
                "UPDATE prompt_receipts
                        SET active_operator_id = NULL, receipt_hash = ?2,
                            encoding_version = 1
                      WHERE id = ?1",
                rusqlite::params![id.to_string(), legacy.receipt_hash()],
            )
            .unwrap();
        }
        (
            a.submitted().id(),
            b.submitted().id(),
            retry.submitted().id(),
            retry_again.submitted().id(),
        )
    };

    let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    for retry_id in [retry_id, retry_again_id] {
        let context = reopened
            .turn_prompt_context(conv, retry_id)
            .unwrap()
            .expect("retry receipt survives reopen");
        assert_eq!(context.active().id(), a_id);
        assert_eq!(context.submitted().root_prompt_id(), a_id);
    }
}

#[test]
fn retry_rejects_hashed_active_pointer_that_disagrees_with_parent() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conv = "tampered-retry-authority";
    let a = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator("A", "A"),
        )
        .unwrap();
    // b is a FRESH operator prompt (not a continuation): under the
    // bug/steering-regressions contract a continuation's authority IS its
    // parent, so pointing a retry-of-a-continuation at A would agree with
    // the recomputed walk. A fresh b keeps the forgery a real disagreement.
    let b = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::operator("B", "B"),
        )
        .unwrap();
    let retry = store
        .begin_prompt(
            conv,
            "title",
            None,
            crate::prompt::NewPrompt::harness_retry("retry", "retry", b.submitted().id()),
        )
        .unwrap();

    // Rehash the row after pointing it at A. Cryptographic row integrity
    // alone therefore passes; semantic validation against the explicit
    // parent B must still reject the authority substitution.
    let forged = retry
        .submitted()
        .receipt()
        .clone()
        .with_active_operator_for_test(a.submitted().id());
    {
        let conn = store.lock_conn();
        conn.execute(
            "UPDATE prompt_receipts
                    SET active_operator_id = ?2, receipt_hash = ?3
                  WHERE id = ?1",
            rusqlite::params![
                forged.id().to_string(),
                forged.active_operator_id().unwrap().to_string(),
                forged.receipt_hash(),
            ],
        )
        .unwrap();
    }
    let error = store
        .turn_prompt_context(conv, forged.id())
        .unwrap_err()
        .to_string();
    assert!(error.contains("disagrees with parent authority"), "{error}");
}

#[test]
fn writer_fingerprint_is_stable_per_install_and_distinct_across_installs() {
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    let first = load_or_create_writer_fingerprint(root_a.path()).unwrap();
    let again = load_or_create_writer_fingerprint(root_a.path()).unwrap();
    let other = load_or_create_writer_fingerprint(root_b.path()).unwrap();
    assert_eq!(first, again, "fingerprint must be stable per install");
    assert_ne!(first, other, "two installs must not share a fingerprint");
    assert_eq!(first.len(), 64, "blake3 hex");
}

// --- load_turn: the by-(conv, seq) read for memory_fetch (#319) --------

/// `load_turn` returns one past turn verbatim, addressed by the §6 seq the
/// model saw in a recall hit; an unknown seq / conversation is `Ok(None)`
/// (labelled absence, never an error — the `memory_fetch` tool contract).
#[test]
fn load_turn_reads_one_turn_by_seq_and_misses_are_none() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conv = store.create("t", None).unwrap();
    store
        .append_turn(&conv, "the question", "the answer")
        .unwrap();

    // The seq the model would paste comes from a recall hit.
    let hits = store.search("question", 5).unwrap();
    assert_eq!(hits.len(), 1);
    let seq = hits[0].seq;

    let turn = store.load_turn(&conv, seq).unwrap().expect("turn exists");
    assert_eq!(turn.user, "the question");
    assert_eq!(turn.assistant, "the answer");

    // Unknown seq → None, not an error.
    assert!(store.load_turn(&conv, seq + 9_999).unwrap().is_none());
    // Unknown conversation id → None, not an error (no cross-ws leak path).
    assert!(store.load_turn("no-such-conv", seq).unwrap().is_none());
}

/// `session_change_index` is the shared coequal-refresh cursor (K6): a
/// follower diffs successive snapshots to learn what changed. A new turn
/// bumps exactly the touched conversation's tick; a new conversation appears;
/// the scan spans workspaces. Diffing two snapshots is how the web cockpit /
/// RichTUI dock overview refresh without re-reading whole conversations.
#[test]
fn session_change_index_tracks_appends_and_new_sessions_across_workspaces() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

    let a = store_a.create("in A", None).unwrap();
    store_a.append_turn(&a, "q1", "r1").unwrap();
    let b = store_b.create("in B", None).unwrap();
    store_b.append_turn(&b, "q1", "r1").unwrap();

    // Snapshot 1 (from EITHER handle) spans both workspaces.
    let snap1: std::collections::HashMap<String, i64> = store_a
        .session_change_index()
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(snap1.len(), 2, "both workspaces' conversations are indexed");
    assert!(snap1.contains_key(&a) && snap1.contains_key(&b));

    // A new turn on A bumps A's tick and leaves B's tick unchanged.
    store_a.append_turn(&a, "q2", "r2").unwrap();
    let snap2: std::collections::HashMap<String, i64> = store_a
        .session_change_index()
        .unwrap()
        .into_iter()
        .collect();
    assert!(snap2[&a] > snap1[&a], "an append advances the touched tick");
    assert_eq!(
        snap2[&b], snap1[&b],
        "an untouched conversation's tick holds"
    );

    // A brand-new conversation appears in the next snapshot (diff = new id).
    let c = store_b.create("also in B", None).unwrap();
    store_b.append_turn(&c, "q", "r").unwrap();
    let snap3: std::collections::HashMap<String, i64> = store_a
        .session_change_index()
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(snap3.len(), 3);
    assert!(
        !snap2.contains_key(&c) && snap3.contains_key(&c),
        "a session that appeared between snapshots is a diff the follower can see"
    );
}
