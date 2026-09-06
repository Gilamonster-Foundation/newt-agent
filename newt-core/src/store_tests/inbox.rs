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
