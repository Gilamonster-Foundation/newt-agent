// UNCOMPILED / UNRUN. Mount as a cfg(test) child module of newt-tui/src/lib.rs.
// Uses the ACTUAL persist_preference_actions and an owned real SQLite database.
// No proposed pin constructor or parallel verification implementation.
use newt_core::cognition::{set_cli_cognition, CognitionOverride};
use newt_core::psyche::{self, ObsessiveSelection};
use newt_core::runtime::{drain_preference_actions, mark_obsessive_choice};
use newt_core::{ConversationStore, OperatorPreferencePin, PreferenceActions, Tenacity};

/// An actual failed SQL UPDATE must retain the exact original; retry folds
/// against the latest unrelated axes and clears pending only after success.
#[test]
fn obsessive_persist_sql_failure_retains_original_and_retry_commits_current_axes() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    psyche::restore_obsessive_selection(None);
    let _ = drain_preference_actions();
    set_cli_cognition(CognitionOverride::Off);
    newt_core::tenacity::clear_cli_tenacity();
    let original = ObsessiveSelection {
        cognition: CognitionOverride::Off,
        tenacity: None,
    };
    assert!(psyche::set_obsessive(true));
    assert_eq!(psyche::obsessive_selection(), Some(original));
    mark_obsessive_choice(Some(original));

    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("owned posture", None).unwrap();
    let base = OperatorPreferencePin {
        backend: Some("backend".into()),
        model: Some("before-model".into()),
        initiative: Some(newt_core::Initiative::Patient),
        ..Default::default()
    };
    store.update_preference_pin(&id, &base).unwrap();
    // Existing TUI tests already have rusqlite and use this owned DB hatch.
    let raw = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    let read_bytes = || {
        raw.query_row(
            "SELECT preference_pin FROM conversations WHERE id = ?1",
            [&id],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    let before = read_bytes();
    raw.execute_batch("CREATE TRIGGER fail_obsessive AFTER UPDATE OF preference_pin ON conversations BEGIN SELECT RAISE(ABORT, 'obsessive_fixture_update'); END;").unwrap();
    let mut pending = PreferenceActions::default();
    let mut provider = None;
    let mut model = None;
    let failure = super::persist_preference_actions(
        Some(&store),
        &id,
        &mut pending,
        &mut provider,
        &mut model,
    )
    .expect_err(
        "the real SQL trigger must be reached; no-op/unknown-field refusal is not this red",
    );
    assert!(
        failure.contains("obsessive_fixture_update"),
        "wrong failure stage: {failure}"
    );
    assert_eq!(
        read_bytes(),
        before,
        "SQLite failure changed durable pin bytes"
    );
    assert_eq!(
        pending.obsessive,
        Some(Some(original)),
        "the host lost the inverse before persistence succeeded"
    );
    let retained = pending.clone();

    raw.execute_batch("DROP TRIGGER fail_obsessive;").unwrap();
    // Another successful ordinary pin change occurs before retry. The retry
    // must merge against this row, not a pre-failure cached pin snapshot.
    let mut later = base.clone();
    later.model = Some("later-model".into());
    store.update_preference_pin(&id, &later).unwrap();
    // Ambient changes cannot replace the ORIGINAL held by the failed action.
    set_cli_cognition(CognitionOverride::Set(
        newt_core::role_profile::Cognition::Rational,
    ));
    newt_core::tenacity::set_cli_tenacity(Tenacity::Normal);
    assert_eq!(pending, retained);
    super::persist_preference_actions(Some(&store), &id, &mut pending, &mut provider, &mut model)
        .unwrap();
    assert!(
        pending.is_empty(),
        "successful commit must settle the pending transition"
    );
    let pin = store.preference_pin(&id).unwrap().unwrap();
    assert_eq!(pin.backend, base.backend);
    assert_eq!(pin.model.as_deref(), Some("later-model"));
    assert_eq!(pin.initiative, base.initiative);
    let workspace_key: String = raw
        .query_row(
            "SELECT workspace_key FROM conversations WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        pin.obsessive
            .as_ref()
            .expect("retry omitted on-state evidence")
            .verified_selection(&id, &workspace_key)
            .unwrap(),
        original
    );
    // Reopen grounds durable resume input, not just the same connection cache.
    let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    assert_eq!(reopened.preference_pin(&id).unwrap(), Some(pin));
}

/// Absence is deliberately rowless, not a successful durable write or failure
/// that may discard its original selection before the materialization owner acts.
#[test]
fn obsessive_persist_rowless_retains_original_without_creating_a_row() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    psyche::restore_obsessive_selection(None);
    let _ = drain_preference_actions();
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let original = ObsessiveSelection {
        cognition: CognitionOverride::Unset,
        tenacity: None,
    };
    let mut pending = PreferenceActions {
        obsessive: Some(Some(original)),
        ..Default::default()
    };
    let mut provider = None;
    let mut model = None;
    super::persist_preference_actions(
        Some(&store),
        "rowless",
        &mut pending,
        &mut provider,
        &mut model,
    )
    .unwrap();
    assert_eq!(pending.obsessive, Some(Some(original)));
    assert!(!store.exists("rowless").unwrap());
    assert!(store.preference_pin("rowless").unwrap().is_none());
}
