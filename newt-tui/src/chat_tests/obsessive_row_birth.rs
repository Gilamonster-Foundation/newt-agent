//! Prepared, uncompiled, unrun. Mount below chat::prompt_ingress_tests.
//! Uses the real prompt boundary and existing store; no test-side pin flush
//! after row birth. Pending input needs a mechanical signature adaptation
//! when production ingress accepts it; never recapture ambient selectors.
use super::*;
use newt_core::cognition::CognitionOverride;
use newt_core::psyche::{self, ObsessiveSelection};

fn rowless_on(store: &newt_core::ConversationStore, id: &str) -> newt_core::PreferenceActions {
    psyche::restore_obsessive_selection(None);
    let _ = newt_core::runtime::drain_preference_actions();
    newt_core::cognition::set_cli_cognition(CognitionOverride::Off);
    newt_core::tenacity::clear_cli_tenacity();
    crate::settings_form::apply_obsessive(true, "/psyche");
    let original = ObsessiveSelection {
        cognition: CognitionOverride::Off,
        tenacity: None,
    };
    assert_eq!(psyche::obsessive_selection(), Some(original));
    let mut pending = newt_core::PreferenceActions::default();
    crate::persist_preference_actions(Some(store), id, &mut pending, &mut None, &mut None).unwrap();
    assert_eq!(pending.obsessive, Some(Some(original)));
    assert!(
        !store.exists(id).unwrap(),
        "a toggle must not create an empty conversation"
    );
    pending
}

/// Actual prompt acceptance must carry the already captured inverse through
/// the SAME transaction as the first receipt, before model work is possible.
#[test]
fn obsessive_row_birth_pin_is_visible_when_first_prompt_receipt_is_inserted() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let (root, store, id) = prompt_store();
    let pending = rowless_on(&store, &id);
    let raw = rusqlite::Connection::open(root.path().join("state/conversations.db")).unwrap();
    raw.execute_batch("CREATE TRIGGER require_birth_pin BEFORE INSERT ON prompt_receipts
      WHEN json_type((SELECT preference_pin FROM conversations WHERE id=NEW.conversation_id), '$.obsessive') IS NOT 'object'
      BEGIN SELECT RAISE(ABORT, 'missing_obsessive_at_receipt_birth'); END;").unwrap();
    let ephemeral = newt_core::agentic::SessionPromptStore::default();
    let accepted = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral,
        },
        &id,
        "first prompt",
        None,
        b"first prompt",
        b"first prompt",
        &ModelInputOrigin::Operator,
    )
    .expect("first prompt and its already captured pin must be accepted together");
    accepted
        .submitted_prompt()
        .receipt()
        .verify_integrity()
        .unwrap();
    let pin = store.preference_pin(&id).unwrap().unwrap();
    assert_eq!(
        pin.obsessive.as_ref().unwrap().original(),
        pending.obsessive.unwrap().unwrap()
    );
    assert!(store.load_verified(&id).unwrap().turns.is_empty());
}

/// A real AFTER INSERT receipt fault must roll back row AND inverse together;
/// writing the pin in a separate earlier transaction would leave a ghost row.
#[test]
fn obsessive_row_birth_receipt_failure_leaves_no_row_and_retains_inverse() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let (root, store, id) = prompt_store();
    let pending = rowless_on(&store, &id);
    let retained = pending.clone();
    let raw = rusqlite::Connection::open(root.path().join("state/conversations.db")).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER abort_birth AFTER INSERT ON prompt_receipts
      BEGIN SELECT RAISE(ABORT, 'obsessive_receipt_birth_abort'); END;",
    )
    .unwrap();
    let ephemeral = newt_core::agentic::SessionPromptStore::default();
    let error = begin_model_prompt(
        PromptIngress {
            durable: Some(&store),
            ephemeral: &ephemeral,
        },
        &id,
        "first prompt",
        None,
        b"first prompt",
        b"first prompt",
        &ModelInputOrigin::Operator,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("obsessive_receipt_birth_abort"));
    assert!(!store.exists(&id).unwrap());
    let count: i64 = raw
        .query_row(
            "SELECT count(*) FROM prompt_receipts WHERE conversation_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(pending, retained);
}
