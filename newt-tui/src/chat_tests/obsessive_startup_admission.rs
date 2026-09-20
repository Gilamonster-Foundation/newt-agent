//! UNCOMPILED / UNRUN; proposed production API adapters are not mounted yet.
//! Mount as a child of chat_tests/prompt_ingress.rs; reuse prompt_store().
//! Both startup and admission MUST be used by production before these count.
use super::*;
use newt_core::cognition::CognitionOverride;
use newt_core::lifecycle::{self, LifecycleEvent};
use std::sync::{Arc, Mutex};

fn reset_startup_inputs() {
    newt_core::psyche::restore_obsessive_selection(None);
    let _ = newt_core::runtime::drain_preference_actions();
    newt_core::runtime::record_cli_preference_axes(Default::default());
    newt_core::cognition::set_cli_cognition(CognitionOverride::Unset);
    newt_core::tenacity::clear_cli_tenacity();
    newt_core::initiative::clear_cli_initiative();
    // Every test owns GlobalSettingsGuard, which restores these keys.
    std::env::remove_var("NEWT_PROVIDER");
    std::env::remove_var("NEWT_DGX_MODEL");
}

/// Locals-only setup for the real startup owner. The test does NOT transfer
/// PinRestore, assign pin_degraded, inspect a marker to choose admission, or
/// invoke the unguarded begin_model_prompt helper.
fn restore_startup(
    store: &newt_core::ConversationStore,
    pin_id: &str,
    outcome: crate::StartupConversation,
    tabs: &mut crate::tabs::TabSet,
) {
    let cfg = newt_core::ResolvedConfig::unrequested(newt_core::Config {
        default_backend: Some("fixture".into()),
        backends: vec![newt_core::BackendConfig {
            name: "fixture".into(),
            endpoint: "http://backend.test:1".into(),
            model: Some("fixture-model".into()),
            kind: Some(newt_core::BackendKind::Openai),
            ..Default::default()
        }],
        ..Default::default()
    });
    let baseline = crate::PreferenceBaseline::snapshot(None, None);
    let mut choice = crate::resolve_backend_choice(&cfg).unwrap();
    let mut inf_url = choice.url.clone();
    let mut inf_model = choice.active_model.clone().unwrap_or_default();
    let mut inf_kind = choice.kind;
    let mut inf_key = choice.api_key.clone();
    let mut inf_context_window = choice.context_window;
    let mut pending = newt_core::PreferenceActions::default();
    let mut base_provider = None;
    let mut base_model = None;
    // ADAPTER NEEDED: third argument installs the actual restore result into
    // the actual active tab INSIDE the production startup owner.
    crate::apply_startup_preference_pin(
        outcome,
        crate::ConversationPreferenceSwitch {
            store: Some(store),
            conversation_id: pin_id,
            baseline: &baseline,
            persona: None,
            pending: &mut pending,
            base_provider: &mut base_provider,
            base_model: &mut base_model,
            cfg: &cfg,
            choice: &mut choice,
            inf_url: &mut inf_url,
            inf_model: &mut inf_model,
            inf_kind: &mut inf_kind,
            inf_key: &mut inf_key,
            inf_context_window: &mut inf_context_window,
            color: false,
            verbose: false,
        },
        tabs.active_mut(),
    );
}

fn seed_valid_on(store: &newt_core::ConversationStore, id: &str) {
    store
        .create_with_id(id, "existing conversation", None)
        .unwrap();
    store
        .append_turn(id, "existing question", "existing answer")
        .unwrap();
    assert!(store
        .merge_preference_actions(
            id,
            &newt_core::PreferenceActions {
                obsessive: Some(Some(newt_core::psyche::ObsessiveSelection {
                    cognition: CognitionOverride::Off,
                    tenacity: None,
                })),
                ..Default::default()
            }
        )
        .unwrap());
    assert!(store
        .preference_pin(id)
        .unwrap()
        .unwrap()
        .obsessive
        .is_some());
    assert!(store.load_verified(id).is_ok());
}

fn corrupt_existing_inverse(store: &newt_core::ConversationStore, id: &str) -> String {
    let valid = store.preference_pin(id).unwrap().unwrap();
    let mut changed = serde_json::to_value(valid).unwrap();
    let cid = changed["obsessive"]["id"].clone();
    changed["obsessive"]["inverse"]["original"]["cognition"] =
        serde_json::to_value(CognitionOverride::Unset).unwrap();
    assert_eq!(changed["obsessive"]["id"], cid);
    // CID verification must be reached: this is still structurally valid.
    let _: newt_core::OperatorPreferencePin = serde_json::from_value(changed.clone()).unwrap();
    let bytes = changed.to_string();
    store.set_raw_preference_pin_for_test(id, &bytes).unwrap();
    assert!(store.preference_pin(id).is_err());
    assert!(store.load_verified(id).is_ok());
    bytes
}

/// Count same-session TurnStarted events and receipt rows AT emission. This
/// observes the production lifecycle event rather than predicting its order.
fn observe_starts(
    root: &std::path::Path,
    tabs: &crate::tabs::TabSet,
) -> (lifecycle::Subscription, Arc<Mutex<Vec<i64>>>) {
    let raw = Mutex::new(rusqlite::Connection::open(root.join("state/conversations.db")).unwrap());
    let conversation = tabs.active().conversation_id().to_owned();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let output = seen.clone();
    let sub = lifecycle::subscribe_session(tabs.active().session_id().as_str(), move |event| {
        if event.event == LifecycleEvent::TurnStarted {
            let count = raw
                .lock()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM prompt_receipts WHERE conversation_id = ?1",
                    [&conversation],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap();
            output.lock().unwrap().push(count);
        }
    });
    (sub, seen)
}

fn accepted_resume_control(with_overlay: bool) {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_startup_inputs();
    let (tmp, store, id) = prompt_store();
    if with_overlay {
        seed_valid_on(&store, &id);
    } else {
        store.create_with_id(&id, "ordinary", None).unwrap();
        store
            .append_turn(&id, "ordinary question", "ordinary answer")
            .unwrap();
    }
    store.claim(&id).unwrap();
    let mut tabs = crate::tabs::TabSet::new(lifecycle::new_session_id(), &id);
    let _scope = lifecycle::scoped_active_session(tabs.active().session_id());
    let (_subscription, starts) = observe_starts(tmp.path(), &tabs);
    let before = store.prompt_chain(&id).unwrap().len();
    restore_startup(
        &store,
        &id,
        crate::StartupConversation::ResumedHeld,
        &mut tabs,
    );
    let ephemeral = newt_core::agentic::SessionPromptStore::default();
    // ADAPTER NEEDED: actual final-model-input owner, no test-side gate.
    let admitted = admit_model_prompt(
        tabs.active(),
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral,
        },
        "inspect",
        None,
        b"  inspect  ",
        b"inspect",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    let ModelPromptAdmission::Accepted(context) = admitted else {
        panic!("ordinary and valid resume must reach actual durable admission");
    };
    assert!(tabs.active().pin_degraded.is_none());
    assert_eq!(
        newt_core::psyche::obsessive_selection().is_some(),
        with_overlay
    );
    let receipt = context.submitted_prompt().receipt();
    assert_eq!(receipt.raw_text(), b"  inspect  ");
    assert_eq!(receipt.model_text(), b"inspect");
    assert_eq!(receipt.origin(), newt_core::PromptOrigin::Operator);
    assert_eq!(receipt.root_prompt_id(), receipt.id());
    receipt.verify_integrity().unwrap();
    assert_eq!(store.prompt_chain(&id).unwrap().len(), before + 1);
    assert_eq!(
        *starts.lock().unwrap(),
        vec![before as i64],
        "one TurnStarted precedes admission, preserving existing accepted-input ordering"
    );
}

#[test]
fn obsessive_startup_ordinary_resume_admits_exact_operator_prompt() {
    accepted_resume_control(false);
}

#[test]
fn obsessive_startup_verified_inverse_resume_admits_exact_operator_prompt() {
    accepted_resume_control(true);
}

/// Invalid inverse + valid turns must refuse before a new receipt or TurnStarted.
#[test]
fn obsessive_startup_corrupt_inverse_reaches_actual_admission_refusal() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_startup_inputs();
    let (tmp, store, id) = prompt_store();
    seed_valid_on(&store, &id);
    let pin_bytes = corrupt_existing_inverse(&store, &id);
    store.claim(&id).unwrap();
    let mut tabs = crate::tabs::TabSet::new(lifecycle::new_session_id(), &id);
    let _scope = lifecycle::scoped_active_session(tabs.active().session_id());
    let (_subscription, starts) = observe_starts(tmp.path(), &tabs);
    let before = store.prompt_chain(&id).unwrap();
    restore_startup(
        &store,
        &id,
        crate::StartupConversation::ResumedHeld,
        &mut tabs,
    );
    let ephemeral = newt_core::agentic::SessionPromptStore::default();
    let outcome = admit_model_prompt(
        tabs.active(),
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral,
        },
        "must not be accepted",
        None,
        b"do work",
        b"do work",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    let ModelPromptAdmission::Refused(reason) = outcome else {
        panic!("startup corruption escaped the actual pre-admission owner");
    };
    assert!(
        tabs.active().pin_degraded.is_some(),
        "startup owner must retain degradation"
    );
    assert!(
        reason.contains("/tab retry"),
        "refusal preserves recovery guidance: {reason}"
    );
    assert!(starts.lock().unwrap().is_empty());
    assert_eq!(store.prompt_chain(&id).unwrap(), before);
    assert!(store.load_verified(&id).is_ok());
    let raw = rusqlite::Connection::open(tmp.path().join("state/conversations.db")).unwrap();
    let after: String = raw
        .query_row(
            "SELECT preference_pin FROM conversations WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, pin_bytes, "refusal must not rewrite evidence");
}

fn fresh_or_refused_control(outcome: crate::StartupConversation) {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_startup_inputs();
    let (tmp, store, held) = prompt_store();
    seed_valid_on(&store, &held);
    corrupt_existing_inverse(&store, &held);
    let replacement = newt_core::new_conversation_id();
    store.claim(&replacement).unwrap();
    let mut tabs = crate::tabs::TabSet::new(lifecycle::new_session_id(), &replacement);
    let _scope = lifecycle::scoped_active_session(tabs.active().session_id());
    let (_subscription, starts) = observe_starts(tmp.path(), &tabs);
    // Deliberately stale sw id: existing startup outcome gate MUST ignore it.
    restore_startup(&store, &held, outcome, &mut tabs);
    assert!(
        !store.exists(&replacement).unwrap(),
        "startup must not manufacture a row"
    );
    let ephemeral = newt_core::agentic::SessionPromptStore::default();
    let admitted = admit_model_prompt(
        tabs.active(),
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral,
        },
        "new work",
        Some("coder"),
        b"new work",
        b"new work",
        &ModelInputOrigin::Operator,
    )
    .unwrap();
    assert!(matches!(admitted, ModelPromptAdmission::Accepted(_)));
    assert!(tabs.active().pin_degraded.is_none());
    assert!(newt_core::psyche::obsessive_selection().is_none());
    let row = store.load_verified(&replacement).unwrap();
    assert!(
        row.turns.is_empty(),
        "row birth admits a prompt, not a completed turn"
    );
    assert_eq!(row.persona.as_deref(), Some("coder"));
    assert_eq!(store.prompt_chain(&replacement).unwrap().len(), 1);
    assert!(
        store.preference_pin(&held).is_err(),
        "foreign invalid evidence remains untouched"
    );
    assert_eq!(*starts.lock().unwrap(), vec![0]);
}

#[test]
fn obsessive_fresh_startup_ignores_unowned_pin_and_materializes_only_at_admission() {
    fresh_or_refused_control(crate::StartupConversation::Fresh);
}

#[test]
fn obsessive_refused_resume_ignores_held_pin_and_admits_only_replacement() {
    fresh_or_refused_control(crate::StartupConversation::ResumedRefused);
}

/// Existing semantics: admissible input announces Working before durable errors.
#[test]
fn obsessive_admission_preserves_durable_error_and_started_order() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_startup_inputs();
    let (tmp, store, id) = prompt_store();
    store.create_with_id(&id, "ordinary", None).unwrap();
    let mut tabs = crate::tabs::TabSet::new(lifecycle::new_session_id(), &id);
    let _scope = lifecycle::scoped_active_session(tabs.active().session_id());
    let (_subscription, starts) = observe_starts(tmp.path(), &tabs);
    restore_startup(
        &store,
        &id,
        crate::StartupConversation::ResumedHeld,
        &mut tabs,
    );
    let raw = rusqlite::Connection::open(tmp.path().join("state/conversations.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER fail_startup_admission BEFORE INSERT ON prompt_receipts BEGIN SELECT RAISE(ABORT, 'startup_admission_insert'); END;").unwrap();
    let ephemeral = newt_core::agentic::SessionPromptStore::default();
    let result = admit_model_prompt(
        tabs.active(),
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral,
        },
        "new work",
        None,
        b"work",
        b"work",
        &ModelInputOrigin::Operator,
    );
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("real prompt INSERT must fail"),
    };
    assert!(format!("{error:#}").contains("startup_admission_insert"));
    assert!(store.prompt_chain(&id).unwrap().is_empty());
    assert_eq!(*starts.lock().unwrap(), vec![0]);
}
