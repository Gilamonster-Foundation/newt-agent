use super::*;

/// The A3/W6 attach-inject inbox: enqueue is FIFO and idempotent, dequeue is
/// exactly-once and non-blocking on empty, and both are workspace-fenced —
/// the properties the interactive-attach seam rests on (D2: the web writes
/// only the inbox; the REPL alone writes turns).
#[test]
fn inbox_inject_take_is_exactly_once_fifo_idempotent_and_fenced() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("session", None).unwrap();

    // Empty inbox → non-blocking None (the REPL poll never stalls).
    assert_eq!(store.take_injected_prompt(&conv).unwrap(), None);

    // Enqueue two; dequeue FIFO, exactly-once.
    assert_eq!(
        store.inject_prompt(&conv, "first", None).unwrap(),
        InjectOutcome::Enqueued
    );
    assert_eq!(
        store.inject_prompt(&conv, "second", None).unwrap(),
        InjectOutcome::Enqueued
    );
    assert_eq!(
        store.take_injected_prompt(&conv).unwrap().unwrap().body,
        "first"
    );
    assert_eq!(
        store.take_injected_prompt(&conv).unwrap().unwrap().body,
        "second"
    );
    assert_eq!(
        store.take_injected_prompt(&conv).unwrap(),
        None,
        "drained exactly once"
    );

    // Idempotency: the same idem_key is a no-op, not a second enqueue.
    assert_eq!(
        store.inject_prompt(&conv, "again", Some("k1")).unwrap(),
        InjectOutcome::Enqueued
    );
    assert_eq!(
        store.inject_prompt(&conv, "again", Some("k1")).unwrap(),
        InjectOutcome::Duplicate
    );
    assert_eq!(
        store.take_injected_prompt(&conv).unwrap().unwrap().body,
        "again"
    );
    assert_eq!(
        store.take_injected_prompt(&conv).unwrap(),
        None,
        "the idem duplicate did not enqueue twice"
    );

    // link_inbox_delivery records the receipt back-link without error.
    store.inject_prompt(&conv, "linked", None).unwrap();
    let taken = store.take_injected_prompt(&conv).unwrap().unwrap();
    store.link_inbox_delivery(&taken.id, "receipt-123").unwrap();

    // Workspace fence: a store on ANOTHER workspace can neither inject into
    // nor take from this conversation.
    let ws_b = tempfile::tempdir().unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
    assert!(
        store_b.inject_prompt(&conv, "cross-ws", None).is_err(),
        "cross-workspace inject is rejected"
    );
    store.inject_prompt(&conv, "mine", None).unwrap();
    assert_eq!(
        store_b.take_injected_prompt(&conv).unwrap(),
        None,
        "cross-workspace take sees nothing"
    );
    assert!(
        store.take_injected_prompt(&conv).unwrap().is_some(),
        "the owning workspace still dequeues it"
    );
}

/// Expiry drops an offer from the pending set and synthesizes NOTHING —
/// retargeted from `permission_request_expires_on_created_tick_ttl`.
#[test]
fn an_expired_offer_is_not_pending() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("s", None).unwrap();
    store.claim_clock = || 1_000;
    let (definition, _instance) = test_offer(&store, &conv);
    let id = store
        .publish_interaction_offer(
            &conv,
            &definition,
            crate::interaction_offer::OfferDanger::High,
            &[newt_interaction::Audience::Web],
        )
        .unwrap();
    assert!(store.pending_interaction_offer(&conv).unwrap().is_some());

    // One tick inside the window is still answerable...
    store.claim_clock = || 999 + ConversationStore::PERMISSION_REQUEST_TTL_NANOS;
    assert!(store.pending_interaction_offer(&conv).unwrap().is_some());
    // ...and AT the deadline it is gone, without anything having
    // answered. The boundary is inherited deliberately from the row
    // this replaces: `published_tick > now - TTL`, so an offer expires
    // exactly at `published + TTL`.
    store.claim_clock = || 1_000 + ConversationStore::PERMISSION_REQUEST_TTL_NANOS;
    assert!(store.pending_interaction_offer(&conv).unwrap().is_none());
    assert_eq!(store.take_interaction_decision(&conv, &id).unwrap(), None);
}

/// A minimal published offer, minted under `store`'s own workspace
/// fence so the fence check is exercised rather than sidestepped.
fn test_offer(
    store: &ConversationStore,
    conv: &str,
) -> (
    newt_interaction::InteractionDefinition,
    newt_interaction::InteractionInstance,
) {
    let question = crate::Question::<crate::PermissionAction> {
        markdown: "\u{2298} run_command wants to run `bash`".to_string(),
        actions: vec![
            crate::Action::new(crate::PermissionAction::AllowOnce, "a", "allow once"),
            crate::Action::new(crate::PermissionAction::Deny, "d", "deny (default)"),
        ],
        note: None,
    };
    let definition = crate::interaction_adapter::question_to_definition(&question).unwrap();
    let (instance, _) = crate::interaction_gate::mint_offer(
        &definition,
        store.workspace_fence(),
        conv,
        &[newt_interaction::Audience::Web],
        // The store's OWN clock, so the offer's TTL window is the one
        // the store measures against rather than a fixed literal that
        // is already ancient by wall time.
        store.claim_tick(),
    )
    .unwrap();
    (definition, instance)
}

/// The offer transport's core contract (B0b-2, #1846), retargeted from
/// `permission_channel_publish_answer_take_race_and_fence`: publish,
/// read back, workspace fence, answer exactly once, and a local cancel
/// that arrives after an answer loses to it.
#[test]
fn interaction_offer_publish_answer_take_race_and_fence() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let other_ws = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("s", None).unwrap();
    let (definition, _instance) = test_offer(&store, &conv);

    let id = store
        .publish_interaction_offer(
            &conv,
            &definition,
            crate::interaction_offer::OfferDanger::Low,
            &[newt_interaction::Audience::Web],
        )
        .unwrap();
    let pending = store.pending_interaction_offer(&conv).unwrap().unwrap();
    assert_eq!(pending.instance_id, id);
    assert_eq!(pending.danger, crate::interaction_offer::OfferDanger::Low);

    // Another workspace cannot see it.
    let foreign = ConversationStore::new(root.path(), other_ws.path(), 100).unwrap();
    assert!(foreign.pending_interaction_offer(&conv).unwrap().is_none());

    // Answering wins once; a second answer loses and the first stands.
    assert_eq!(
        store
            .answer_interaction_offer(
                &conv,
                &id,
                crate::PermissionAction::AllowOnce,
                newt_interaction::Audience::Web
            )
            .unwrap(),
        AnswerOutcome::Answered
    );
    assert_eq!(
        store
            .answer_interaction_offer(
                &conv,
                &id,
                crate::PermissionAction::Deny,
                newt_interaction::Audience::Web
            )
            .unwrap(),
        AnswerOutcome::AlreadyResolved
    );
    assert_eq!(
        store.take_interaction_decision(&conv, &id).unwrap(),
        Some(crate::PermissionAction::AllowOnce)
    );
    // ...and it is no longer pending.
    assert!(store.pending_interaction_offer(&conv).unwrap().is_none());
    // A local cancel after an answer LOSES: the answer stands.
    assert!(!store.cancel_interaction_offer(&conv, &id).unwrap());
}

/// A0 freeze (#1823), retargeted (B0b-2, #1846). The old lenient read
/// consumed an unparseable VERDICT string as `Ok(None)`; the new decode
/// path is an unparseable OPTION in the stored Response. The guarantee
/// is the same and deliberately kept: an answer this build cannot read
/// is not an answer, and is never an error or an authorization.
#[test]
fn an_unknown_persisted_option_reads_as_none_not_error() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("s", None).unwrap();
    let (definition, _instance) = test_offer(&store, &conv);
    let id = store
        .publish_interaction_offer(
            &conv,
            &definition,
            crate::interaction_offer::OfferDanger::Low,
            &[newt_interaction::Audience::Web],
        )
        .unwrap();
    store
        .answer_interaction_offer(
            &conv,
            &id,
            crate::PermissionAction::AllowOnce,
            newt_interaction::Audience::Web,
        )
        .unwrap();

    // Rewrite the stored body to name an option this build does not
    // know — the shape an older or forked writer could leave behind.
    store
            .lock_conn()
            .execute(
                "UPDATE interaction_offers SET response_json = replace(response_json, 'allow_once', 'allow_twice') WHERE instance_id = ?1",
                [&id],
            )
            .unwrap();

    // LENIENT, deliberately, and unchanged from the verdict path this
    // replaces: an unreadable answer reads as "no answer", never as an
    // error and never as an authorization. Tightening it (erroring, or
    // treating it as a deny) is a change a later slice must list.
    assert_eq!(store.take_interaction_decision(&conv, &id).unwrap(), None);
    // ...and the audit fact survives regardless: the row still records
    // WHICH surface answered.
    assert_eq!(
        store.interaction_answered_by(&conv, &id).unwrap(),
        Some(newt_interaction::Audience::Web)
    );
}

fn flock_task(event: &str, body: &str) -> NewFlockTask {
    NewFlockTask {
        body: body.to_owned(),
        flock_cid: "flock-1".into(),
        event_cid: event.to_owned(),
        evidence_cid: format!("evidence-of-{event}"),
        evidence: format!("{{\"event\":\"{event}\"}}"),
    }
}

/// A flock task is deduped by (flock, event) across restarts, even once it is
/// delivered or rejected, and the web's dequeue never sees it.
#[test]
fn flock_tasks_dedupe_across_restarts_and_stay_out_of_the_web_queue() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("session", None).unwrap();
    assert_eq!(
        store
            .enqueue_flock_task(&conv, &flock_task("e1", "do e1"))
            .unwrap(),
        InjectOutcome::Enqueued
    );
    assert_eq!(
        store
            .enqueue_flock_task(&conv, &flock_task("e1", "do e1"))
            .unwrap(),
        InjectOutcome::Duplicate
    );
    store.inject_prompt(&conv, "from the web", None).unwrap();
    // each queue sees only its own rows
    assert_eq!(
        store.take_injected_prompt(&conv).unwrap().unwrap().body,
        "from the web"
    );
    assert_eq!(store.take_injected_prompt(&conv).unwrap(), None);
    let first = store.peek_flock_task(&conv).unwrap().unwrap();
    assert_eq!(first.task.body, "do e1");
    // peeking leaves it queued
    assert_eq!(store.peek_flock_task(&conv).unwrap().unwrap().id, first.id);

    // a restart: a fresh store on the same database; replaying the same
    // event is a duplicate whether it is queued, delivered or rejected
    drop(store);
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(
        store
            .enqueue_flock_task(&conv, &flock_task("e1", "do e1"))
            .unwrap(),
        InjectOutcome::Duplicate
    );
    store.reject_flock_task(&first).unwrap();
    assert_eq!(
        store.peek_flock_task(&conv).unwrap(),
        None,
        "rejected is never offered"
    );
    assert_eq!(
        store
            .enqueue_flock_task(&conv, &flock_task("e1", "do e1"))
            .unwrap(),
        InjectOutcome::Duplicate
    );
    // another event of the same flock, and the same event of another flock, are new
    assert_eq!(
        store
            .enqueue_flock_task(&conv, &flock_task("e2", "do e2"))
            .unwrap(),
        InjectOutcome::Enqueued
    );
    let mut other = flock_task("e1", "other flock");
    other.flock_cid = "flock-2".into();
    assert_eq!(
        store.enqueue_flock_task(&conv, &other).unwrap(),
        InjectOutcome::Enqueued
    );
    // FIFO
    let next = store.peek_flock_task(&conv).unwrap().unwrap();
    assert_eq!(next.task.event_cid, "e2");
    assert!(next.seq > first.seq);
}

#[test]
fn a_flock_task_is_workspace_fenced() {
    let root = tempfile::tempdir().unwrap();
    let (ws_a, ws_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
    let conv = a.create("session", None).unwrap();
    assert!(b.enqueue_flock_task(&conv, &flock_task("e1", "x")).is_err());
    a.enqueue_flock_task(&conv, &flock_task("e1", "x")).unwrap();
    assert_eq!(b.peek_flock_task(&conv).unwrap(), None);
}

/// Accepting mints a `flock`-origin receipt (never `operator`) and marks the
/// row delivered with that receipt's id in one transaction.
#[test]
fn accepting_a_flock_task_mints_a_flock_receipt_and_marks_delivery_together() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("session", None).unwrap();
    store
        .enqueue_flock_task(&conv, &flock_task("e1", "the brief"))
        .unwrap();
    let task = store.peek_flock_task(&conv).unwrap().unwrap();

    let ctx = store
        .accept_flock_task(&conv, "session", None, &task)
        .unwrap();
    let receipt = store
        .load_prompt(ctx.submitted_prompt().id())
        .unwrap()
        .unwrap();
    assert_eq!(receipt.origin(), PromptOrigin::Flock);
    assert_eq!(receipt.raw_text(), b"the brief");
    assert_eq!(
        receipt.parent_prompt_id(),
        None,
        "it roots its own objective"
    );
    assert_eq!(receipt.root_prompt_id(), receipt.id());
    assert_eq!(
        store.latest_prompt(&conv).unwrap().unwrap().id(),
        receipt.id()
    );
    // delivered, with the back-link
    assert_eq!(store.peek_flock_task(&conv).unwrap(), None);
    let conn = store.lock_conn();
    let (delivered, link): (i64, Option<String>) = conn
        .query_row(
            "SELECT delivered, delivered_receipt_id FROM conversation_inbox WHERE id = ?1",
            [&task.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(delivered, 1);
    assert_eq!(link.as_deref(), Some(receipt.id().to_string().as_str()));
    drop(conn);

    // a second accept of the same row mints nothing more
    assert!(store
        .accept_flock_task(&conv, "session", None, &task)
        .is_err());
    let receipts: i64 = store
        .lock_conn()
        .query_row("SELECT COUNT(*) FROM prompt_receipts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(receipts, 1, "a refused accept left a receipt behind");
}

/// The dequeue -> receipt crash window: when the receipt cannot be minted the
/// row is still queued (nothing was marked), and when the row cannot be
/// marked the receipt is rolled back. Neither half survives alone.
#[test]
fn a_failed_accept_leaves_no_receipt_and_the_task_still_queued() {
    let root = tempfile::tempdir().unwrap();
    let (ws_a, ws_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let other = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
    let conv = store.create("session", None).unwrap();
    store
        .enqueue_flock_task(&conv, &flock_task("e1", "the brief"))
        .unwrap();
    let task = store.peek_flock_task(&conv).unwrap().unwrap();

    // receipt side fails (another workspace's fence): the row stays queued
    assert!(other
        .accept_flock_task(&conv, "session", None, &task)
        .is_err());
    assert_eq!(store.peek_flock_task(&conv).unwrap().unwrap().id, task.id);
    assert_eq!(store.latest_prompt(&conv).unwrap(), None);

    // mark side fails (the row was rejected meanwhile): the receipt rolls back
    store.reject_flock_task(&task).unwrap();
    assert!(store
        .accept_flock_task(&conv, "session", None, &task)
        .is_err());
    assert_eq!(
        store.latest_prompt(&conv).unwrap(),
        None,
        "a receipt survived the failed delivery mark"
    );

    // and a task that was peeked but never accepted (the process died) is
    // offered again to the next store
    store
        .enqueue_flock_task(&conv, &flock_task("e2", "second"))
        .unwrap();
    let peeked = store.peek_flock_task(&conv).unwrap().unwrap();
    drop(store);
    let again = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    assert_eq!(again.peek_flock_task(&conv).unwrap().unwrap().id, peeked.id);
}

/// A database made before the `flock` origin existed (its CHECK refuses it)
/// is rebuilt on open with every receipt intact, and then takes flock tasks.
#[test]
fn an_older_database_is_migrated_to_allow_the_flock_origin() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let (conv, old_id) = {
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conv = store.create("session", None).unwrap();
        let ctx = store
            .begin_prompt(&conv, "session", None, NewPrompt::operator("old", "old"))
            .unwrap();
        (conv, ctx.submitted_prompt().id())
    };
    // put the table back as an older binary made it: the narrow CHECK
    {
        let db = rusqlite::Connection::open(root.path().join(DB_FILE)).unwrap();
        let sql: String = db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'prompt_receipts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql.contains("'flock'"), "the fresh schema allows flock");
        db.execute_batch("PRAGMA foreign_keys = OFF; PRAGMA writable_schema = ON;")
            .unwrap();
        db.execute(
            "UPDATE sqlite_master SET sql = replace(sql, ', ''flock''', '') WHERE name = 'prompt_receipts'",
            [],
        )
        .unwrap();
        db.execute_batch("PRAGMA writable_schema = OFF;").unwrap();
    }
    {
        let db = rusqlite::Connection::open(root.path().join(DB_FILE)).unwrap();
        let sql: String = db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'prompt_receipts'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !sql.contains("'flock'"),
            "the fixture is the older table: {sql}"
        );
    }
    // reopening migrates it
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let old = store
        .load_prompt(old_id)
        .unwrap()
        .expect("the old receipt survived");
    assert_eq!(old.raw_text(), b"old");
    assert_eq!(old.origin(), PromptOrigin::Operator);
    store
        .enqueue_flock_task(&conv, &flock_task("e1", "new"))
        .unwrap();
    let task = store.peek_flock_task(&conv).unwrap().unwrap();
    let ctx = store
        .accept_flock_task(&conv, "session", None, &task)
        .unwrap();
    let new = store
        .load_prompt(ctx.submitted_prompt().id())
        .unwrap()
        .unwrap();
    assert_eq!(new.origin(), PromptOrigin::Flock);
    // chronology is intact across the rebuild
    assert_eq!(
        new.previous_prompt_id(),
        Some(old_id),
        "the rebuilt table kept receipt order"
    );
    let violations: i64 = store
        .lock_conn()
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

/// Review 2976 r1 finding 1: a self-rooted flock receipt must be readable by
/// every lineage reader: context rebuild (resume), an operator continuation,
/// a harness retry (durable and ephemeral) and an artifact write.
#[test]
fn a_flock_rooted_objective_can_be_resumed_continued_retried_and_annotated() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = store.create("session", None).unwrap();
    store
        .enqueue_flock_task(&conv, &flock_task("e1", "the brief"))
        .unwrap();
    let task = store.peek_flock_task(&conv).unwrap().unwrap();
    let ctx = store
        .accept_flock_task(&conv, "session", None, &task)
        .unwrap();
    let id = ctx.submitted_prompt().id();

    // resume: the context rebuilds, and the flock receipt is its own authority
    let again = store.turn_prompt_context(&conv, id).unwrap().unwrap();
    assert_eq!(again.submitted_prompt().id(), id);
    assert_eq!(again.active().id(), id);
    assert_eq!(again.active().receipt().origin(), PromptOrigin::Flock);

    // an operator continuation and a harness retry attach to it
    let cont = store
        .begin_prompt(
            &conv,
            "session",
            None,
            NewPrompt::operator_continuation("go on", "go on", id),
        )
        .unwrap();
    assert_eq!(cont.submitted_prompt().receipt().root_prompt_id(), id);
    let retry = store
        .begin_prompt(
            &conv,
            "session",
            None,
            NewPrompt::harness_retry("retry", "retry", id),
        )
        .unwrap();
    assert_eq!(retry.active().id(), id, "authority stays the task");

    // an ephemeral retry from the rebuilt context
    let eph =
        crate::TurnPromptContext::ephemeral_harness_retry(conv.clone(), "r", "r", &again).unwrap();
    assert_eq!(eph.active().id(), id);

    // an artifact can be written against it
    store
        .append_prompt_artifact(
            &conv,
            id,
            crate::NewPromptArtifact::new(
                crate::ArtifactKind::Decision,
                crate::ArtifactRelation::DerivedFrom,
            ),
        )
        .unwrap();
}
