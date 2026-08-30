use super::*;
// The store/gate fixtures already exist next door; a second copy of
// them would drift from the ones the pre-B0b tests use, and then the
// parity these tests claim would be against a different fixture.
use super::permission_prompt_tests::{publish_low_danger, scripted_gate, store_and_conv};
use newt_core::interaction_gate::{authorize_action, mint_offer, now_tick, permission_registry};
use newt_core::{AnswerOutcome, Caveats, DenialKind, PermissionGate, PermissionRequest};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Barrier};

fn request() -> PermissionRequest {
    PermissionRequest {
        tool: "run_command".into(),
        kind: DenialKind::Exec,
        target: "bash".into(),
        reason: String::new(),
    }
}

fn offer(
    audience: Audience,
) -> (
    InteractionDefinition,
    newt_interaction::InteractionInstance,
    newt_interaction::Lifecycle,
) {
    let definition = permission_definition(
        &request(),
        &danger::DangerTable::builtin(),
        audience.clone(),
    );
    let (instance, lifecycle) =
        mint_offer(&definition, "ws", "conv-1", &[audience], now_tick()).expect("mints");
    (definition, instance, lifecycle)
}

/// The two wall clocks are now ONE relationship, asserted.
#[test]
fn the_gate_timeout_is_shorter_than_the_store_ttl() {
    let gate_nanos = i64::try_from(WEB_DECISION_TIMEOUT.as_nanos()).expect("fits");
    let store_ttl = newt_core::ConversationStore::PERMISSION_REQUEST_TTL_NANOS;
    assert!(
        gate_nanos < store_ttl,
        "the gate must give up while the offer is still answerable: \
             gate {gate_nanos}ns vs store TTL {store_ttl}ns"
    );
    // ...and an offer carries the store's number, so the two cannot
    // drift apart again.
    let (_d, instance, _l) = offer(Audience::Terminal);
    assert_eq!(instance.ttl_ticks, store_ttl);
}

/// An offer past its TTL authorizes nothing — the fail-closed default
/// is a denial produced by refusing every response, never a synthesized
/// allow.
#[test]
fn an_expired_permission_denies_by_default() {
    let (definition, instance, lifecycle) = offer(Audience::Terminal);
    assert!(!newt_interaction::Lifecycle::has_elapsed(
        &instance,
        instance.provenance.minted_tick
    ));
    assert!(newt_interaction::Lifecycle::has_elapsed(
        &instance,
        instance.provenance.minted_tick + instance.ttl_ticks
    ));

    let expired = lifecycle.expire().expect("expires");
    let registry = permission_registry(Audience::Terminal);
    for action in [PromptChoice::AllowOnce, PromptChoice::AllowSession] {
        assert!(
            authorize_action(
                &definition,
                &instance,
                &expired,
                "ws",
                &registry,
                action,
                Audience::Terminal
            )
            .is_err(),
            "an expired offer authorized {action:?}"
        );
    }
}

/// The registry is the CALLER's and is not derived from the form, so
/// an action the form offers but the gate cannot execute is refused —
/// and the durable grants stay terminal-only even against a form that
/// offered them.
#[test]
fn the_registry_is_independent_of_the_form() {
    let (definition, instance, lifecycle) = offer(Audience::Terminal);
    // Empty registry: nothing is executable, so nothing authorizes.
    assert!(authorize_action(
        &definition,
        &instance,
        &lifecycle,
        "ws",
        &[],
        PromptChoice::AllowOnce,
        Audience::Terminal
    )
    .is_err());

    // The web registry refuses a durable grant even when handed a
    // TERMINAL form that offers it.
    let low = PermissionRequest {
        tool: "http".into(),
        kind: DenialKind::Net,
        target: "https://example.com/api".into(),
        reason: String::new(),
    };
    let terminal_form =
        permission_definition(&low, &danger::DangerTable::builtin(), Audience::Terminal);
    let (inst, life) =
        mint_offer(&terminal_form, "ws", "conv-1", &[Audience::Web], now_tick()).expect("mints");
    assert!(
        authorize_action(
            &terminal_form,
            &inst,
            &life,
            "ws",
            &permission_registry(Audience::Web),
            PromptChoice::AllowPermanent,
            Audience::Web
        )
        .is_err(),
        "the web authorized a durable grant"
    );
}

/// The fence is supplied by the CALLER, so a mismatch is detectable
/// rather than a tautology.
#[test]
fn a_foreign_workspace_cannot_authorize() {
    let (definition, instance, lifecycle) = offer(Audience::Terminal);
    let registry = permission_registry(Audience::Terminal);
    assert!(authorize_action(
        &definition,
        &instance,
        &lifecycle,
        "ws",
        &registry,
        PromptChoice::AllowOnce,
        Audience::Terminal
    )
    .is_ok());
    assert!(
        authorize_action(
            &definition,
            &instance,
            &lifecycle,
            "ws-elsewhere",
            &registry,
            PromptChoice::AllowOnce,
            Audience::Terminal
        )
        .is_err(),
        "a foreign workspace key authorized a decision"
    );
}

/// **Q1's answer, pinned.** With `NEWT_WEB_DECISIONS` unset the gate
/// has no store, so the default terminal path performs NO store write.
/// The second half is the anti-vacuous twin: the same assertion must
/// go the other way when a store IS wired, or it would pass by
/// measuring nothing.
#[test]
fn the_default_terminal_path_performs_no_store_write() {
    let (_r, _w, store, conv) = store_and_conv();
    let prompts = Rc::new(Cell::new(0));
    let mut state = PermissionPromptState::default();
    {
        let mut gate = scripted_gate(
            &mut state,
            Caveats::default(),
            None,
            None,
            vec![PromptChoice::AllowOnce],
            Rc::clone(&prompts),
        );
        gate.conversation_id = conv.clone();
        let _ = gate.ask(&[request()]);
    }
    assert_eq!(prompts.get(), 1, "the terminal prompt did not run");
    assert!(
        store.pending_interaction_offer(&conv).unwrap().is_none(),
        "the DEFAULT terminal path published to the store"
    );

    // Twin: with a store wired, the same read DOES see a row — so the
    // assertion above is measuring something.
    publish_low_danger(&store, &conv);
    assert!(
        store.pending_interaction_offer(&conv).unwrap().is_some(),
        "the pending read cannot see a published offer, so the check above is vacuous"
    );
}

/// A permission resolves exactly once through the controller: the
/// authorization is `validate_response`'s, and the verdict is
/// consumable once.
#[test]
fn a_permission_resolves_exactly_once_through_the_controller() {
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);

    assert_eq!(
        store
            .answer_interaction_offer(&conv, &request_id, PromptChoice::AllowOnce, Audience::Web)
            .unwrap(),
        AnswerOutcome::Answered
    );
    // A second answer finds it already answered — never a second win.
    assert_eq!(
        store
            .answer_interaction_offer(&conv, &request_id, PromptChoice::Deny, Audience::Web)
            .unwrap(),
        AnswerOutcome::AlreadyResolved
    );
    // Reading the answer is IDEMPOTENT, and that is a deliberate
    // change from the row this replaces. "Consume once" was an
    // artifact of the two-phase design: answering used to leave
    // `resolved = 0`, so the gate's poll had to finalize it. A single
    // CAS finalizes at write time, so the answer is a stable fact
    // afterwards rather than a token that can be spent. Exactly-once
    // is unchanged — it is the ANSWER that happens once, asserted
    // above, not the reading of it.
    for _ in 0..3 {
        assert_eq!(
            store.take_interaction_decision(&conv, &request_id).unwrap(),
            Some(PromptChoice::AllowOnce)
        );
    }
    // ...and the offer is no longer answerable by anyone.
    assert!(store.pending_interaction_offer(&conv).unwrap().is_none());
}

/// An action the published form never offered is refused by the store
/// — now through `validate_response`, not through a decode.
#[test]
fn an_undisplayed_action_is_refused_by_the_controller() {
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    // `bash` is high danger, so the web form offers allow_once/deny
    // only. A session allow was never displayed.
    assert_eq!(
        store
            .answer_interaction_offer(
                &conv,
                &request_id,
                PromptChoice::AllowSession,
                Audience::Web
            )
            .unwrap(),
        AnswerOutcome::InvalidAction
    );
    // ...and the request is still open for a legitimate answer.
    assert_eq!(
        store
            .answer_interaction_offer(&conv, &request_id, PromptChoice::AllowOnce, Audience::Web)
            .unwrap(),
        AnswerOutcome::Answered
    );
}

/// **The real race**: two INDEPENDENT connections, one per thread —
/// the shape newt-web actually produces — released together by a
/// `Barrier`. Two threads sharing one store would exercise the
/// in-process `Arc<Mutex<Connection>>` and prove nothing.
#[test]
fn separate_connections_racing_one_permission_resolve_once() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let seed = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conv = seed.create("s", None).unwrap();
    let request_id = publish_low_danger(&seed, &conv);
    drop(seed);

    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for action in [PromptChoice::AllowOnce, PromptChoice::Deny] {
        let root_path = root.path().to_path_buf();
        let ws_path = ws.path().to_path_buf();
        let conv = conv.clone();
        let request_id = request_id.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            // A FRESH store — its own connection, exactly as a web
            // request gets.
            let store = newt_core::ConversationStore::new(root_path, ws_path, 100).unwrap();
            barrier.wait();
            (
                action,
                store.answer_interaction_offer(&conv, &request_id, action, Audience::Web),
            )
        }));
    }
    barrier.wait();
    let outcomes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();

    let winners: Vec<PromptChoice> = outcomes
        .iter()
        .filter(|(_, r)| matches!(r, Ok(AnswerOutcome::Answered)))
        .map(|(a, _)| *a)
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one connection must win; got {outcomes:#?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|(_, r)| matches!(r, Ok(AnswerOutcome::AlreadyResolved))),
        "the loser must be told it lost, not silently succeed: {outcomes:#?}"
    );

    // The loser observes the WINNER's verdict, not its own.
    let reopened = newt_core::ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let observed: PromptChoice = reopened
        .take_interaction_decision(&conv, &request_id)
        .unwrap()
        .expect("a verdict was recorded");
    assert_eq!(
        observed, winners[0],
        "the recorded verdict is not the winner's"
    );
}

/// The loser sees the same terminal fact the winner produced — the
/// property `web_verdict_and_local_control_resolve_exactly_once`
/// pins for the local-abort race, stated for the store race.
#[test]
fn the_loser_observes_the_winners_verdict() {
    let (_r, _w, store, conv) = store_and_conv();
    let request_id = publish_low_danger(&store, &conv);
    assert_eq!(
        store
            .answer_interaction_offer(&conv, &request_id, PromptChoice::AllowOnce, Audience::Web)
            .unwrap(),
        AnswerOutcome::Answered
    );
    // A later answer loses...
    assert_eq!(
        store
            .answer_interaction_offer(&conv, &request_id, PromptChoice::Deny, Audience::Web)
            .unwrap(),
        AnswerOutcome::AlreadyResolved
    );
    // ...and what everyone reads afterwards is the WINNER's verdict.
    let observed: PromptChoice = store
        .take_interaction_decision(&conv, &request_id)
        .unwrap()
        .expect("a verdict");
    assert_eq!(observed, PromptChoice::AllowOnce);
}
