// UNCOMPILED / UNRUN. Mount as a cfg(test) child of store.rs.
// ADAPTER NEEDED until merge_preference_actions(id, &actions) is mounted.
// All minting uses that production atomic producer; no test-side verifier/hash.
use super::*;
use crate::cognition::CognitionOverride;
use crate::psyche::ObsessiveSelection;
use crate::{OperatorPreferencePin, PreferenceActions};

include!("obsessive_required_tenacity.rs");

struct PinDb {
    root: tempfile::TempDir,
    workspace: tempfile::TempDir,
    store: ConversationStore,
}
impl PinDb {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        Self {
            root,
            workspace,
            store,
        }
    }
    fn create(&self, id: &str) {
        self.store.create_with_id(id, "fixture", None).unwrap();
    }
    fn raw_pin(&self, id: &str) -> String {
        self.store
            .lock_conn()
            .query_row(
                "SELECT preference_pin FROM conversations WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }
    fn pin(&self, id: &str, original: ObsessiveSelection) -> OperatorPreferencePin {
        assert!(self
            .store
            .merge_preference_actions(
                id,
                &PreferenceActions {
                    obsessive: Some(Some(original)),
                    ..Default::default()
                }
            )
            .expect("ATOMIC PRODUCER prerequisite: real owner-bound pin must commit"));
        let pin = self
            .store
            .preference_pin(id)
            .expect("POSITIVE CONTROL: actual reader must accept the produced pin")
            .unwrap();
        assert_eq!(
            pin.obsessive
                .as_ref()
                .unwrap()
                .verified_selection(id, &self.store.workspace_id)
                .unwrap(),
            original
        );
        pin
    }
    fn restore_raw(&self, id: &str, value: &serde_json::Value) {
        self.store
            .set_raw_preference_pin_for_test(id, &value.to_string())
            .unwrap();
    }
}
fn original_a() -> ObsessiveSelection {
    ObsessiveSelection {
        cognition: CognitionOverride::Off,
        tenacity: None,
    }
}
fn original_b() -> ObsessiveSelection {
    ObsessiveSelection {
        cognition: CognitionOverride::Unset,
        tenacity: Some(crate::Tenacity::Resolute),
    }
}

/// Legacy ordinary pins do not acquire an inferred posture or original snapshot.
#[test]
fn obsessive_pin_legacy_empty_is_ordinary_and_unchanged() {
    let db = PinDb::new();
    db.create("ordinary");
    db.store
        .set_raw_preference_pin_for_test("ordinary", "{}")
        .unwrap();
    let pin = db.store.preference_pin("ordinary").unwrap().unwrap();
    assert_eq!(pin, OperatorPreferencePin::default());
    assert!(pin.obsessive.is_none());
    assert_eq!(db.raw_pin("ordinary"), "{}");
}

/// A valid minted pin MUST pass before malformed/tamper negatives count.
#[test]
fn obsessive_pin_actual_producer_roundtrips_after_reopen() {
    let db = PinDb::new();
    db.create("held");
    let expected = db.pin("held", original_a());
    let bytes = db.raw_pin("held");
    let reopened = ConversationStore::new(db.root.path(), db.workspace.path(), 100).unwrap();
    assert_eq!(reopened.preference_pin("held").unwrap(), Some(expected));
    assert_eq!(
        db.raw_pin("held"),
        bytes,
        "read must not rewrite history/pin"
    );
}

/// Legal selector substitution passes serde but fails the real CID-reading gate.
#[test]
fn obsessive_pin_well_formed_original_edit_with_same_cid_is_refused() {
    let db = PinDb::new();
    db.create("a");
    db.create("b");
    let mut edited = serde_json::to_value(db.pin("a", original_a())).unwrap();
    let other = serde_json::to_value(db.pin("b", original_b())).unwrap();
    let identity = edited["obsessive"]["id"].clone();
    edited["obsessive"]["inverse"]["original"] = other["obsessive"]["inverse"]["original"].clone();
    assert_eq!(edited["obsessive"]["id"], identity);
    let typed: OperatorPreferencePin = serde_json::from_value(edited.clone())
        .expect("must remain well-formed: unknown fields/enum rejection is not CID proof");
    let before = db.raw_pin("a");
    assert!(
        db.store.update_preference_pin("a", &typed).is_err(),
        "production writer accepted edited inverse"
    );
    assert_eq!(db.raw_pin("a"), before, "refused writer changed bytes");
    db.restore_raw("a", &edited);
    assert!(
        db.store.preference_pin("a").is_err(),
        "production reader accepted same-CID selector edit"
    );
    assert_eq!(
        db.raw_pin("a"),
        edited.to_string(),
        "reader must preserve corrupt evidence"
    );
}

/// Invalid and wrong-codec identities are checked after a real valid control.
#[test]
fn obsessive_pin_bad_identity_profiles_are_refused_without_repair() {
    let db = PinDb::new();
    db.create("a");
    let valid = serde_json::to_value(db.pin("a", original_a())).unwrap();
    for identity in [
        "not-a-cid".to_string(),
        content_addressable::RawContentId::from_content(b"fixture").to_string(),
    ] {
        let mut edited = valid.clone();
        edited["obsessive"]["id"] = serde_json::json!(identity);
        // The envelope stores String; this proves it got beyond old unknown-key rejection.
        let typed: OperatorPreferencePin = serde_json::from_value(edited.clone()).unwrap();
        db.restore_raw("a", &valid);
        assert!(db.store.update_preference_pin("a", &typed).is_err());
        assert_eq!(db.raw_pin("a"), valid.to_string());
        db.restore_raw("a", &edited);
        assert!(db.store.preference_pin("a").is_err());
        assert_eq!(db.raw_pin("a"), edited.to_string());
    }
}

/// Missing original is malformed active evidence, not an ordinary off record.
#[test]
fn obsessive_pin_missing_original_is_refused_after_valid_control() {
    let db = PinDb::new();
    db.create("a");
    let mut edited = serde_json::to_value(db.pin("a", original_a())).unwrap();
    edited["obsessive"]["inverse"]
        .as_object_mut()
        .unwrap()
        .remove("original");
    db.restore_raw("a", &edited);
    assert!(db.store.preference_pin("a").is_err());
    assert_eq!(db.raw_pin("a"), edited.to_string());
}

/// An intact pin from another conversation cannot be transplanted by writer or raw SQL.
#[test]
fn obsessive_pin_foreign_conversation_is_refused() {
    let db = PinDb::new();
    db.create("a");
    db.create("b");
    let pin = db.pin("a", original_a());
    let before = db.raw_pin("b");
    assert!(db.store.update_preference_pin("b", &pin).is_err());
    assert_eq!(db.raw_pin("b"), before);
    db.restore_raw("b", &serde_json::to_value(pin).unwrap());
    assert!(db.store.preference_pin("b").is_err());
}

/// Same conversation id in separate real databases isolates the workspace binding.
#[test]
fn obsessive_pin_foreign_workspace_with_same_conversation_is_refused() {
    let a = PinDb::new();
    let b = PinDb::new();
    a.create("same-conversation");
    b.create("same-conversation");
    assert_ne!(a.store.workspace_id, b.store.workspace_id);
    let pin = a.pin("same-conversation", original_a());
    let before = b.raw_pin("same-conversation");
    assert!(b
        .store
        .update_preference_pin("same-conversation", &pin)
        .is_err());
    assert_eq!(b.raw_pin("same-conversation"), before);
    b.restore_raw("same-conversation", &serde_json::to_value(pin).unwrap());
    assert!(b.store.preference_pin("same-conversation").is_err());
}

/// Atomic actions merge current unrelated axes, never a stale caller snapshot.
#[test]
fn obsessive_pin_atomic_merge_preserves_unrelated_axes_and_addressed_off() {
    let db = PinDb::new();
    db.create("a");
    let base = OperatorPreferencePin {
        backend: Some("backend".into()),
        model: Some("model".into()),
        initiative: Some(crate::Initiative::Patient),
        ..Default::default()
    };
    db.store.update_preference_pin("a", &base).unwrap();
    let activity_and_tip = || {
        db.store
            .lock_conn()
            .query_row(
                "SELECT activity_tick, tip_hash FROM conversations WHERE id = 'a'",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .unwrap()
    };
    let before_activity = activity_and_tip();
    db.pin("a", original_a());
    assert!(db
        .store
        .merge_preference_actions(
            "a",
            &PreferenceActions {
                model: Some(Some("new-model".into())),
                ..Default::default()
            }
        )
        .unwrap());
    let pin = db.store.preference_pin("a").unwrap().unwrap();
    assert_eq!(pin.backend, base.backend);
    assert_eq!(pin.model.as_deref(), Some("new-model"));
    assert_eq!(pin.initiative, base.initiative);
    assert_eq!(
        pin.obsessive
            .as_ref()
            .unwrap()
            .verified_selection("a", &db.store.workspace_id)
            .unwrap(),
        original_a()
    );
    assert!(db
        .store
        .merge_preference_actions(
            "a",
            &PreferenceActions {
                obsessive: Some(None),
                ..Default::default()
            }
        )
        .unwrap());
    let off = db.store.preference_pin("a").unwrap().unwrap();
    let off_state = off.obsessive.as_ref().expect("acted off remains addressed");
    assert_eq!(off_state.mode(), crate::psyche::ObsessiveMode::Off);
    assert_eq!(
        off_state
            .verified_selection("a", &db.store.workspace_id)
            .unwrap(),
        original_a()
    );
    assert_eq!(off.backend, base.backend);
    assert_eq!(off.model.as_deref(), Some("new-model"));
    assert_eq!(off.initiative, base.initiative);
    assert_eq!(
        activity_and_tip(),
        before_activity,
        "posture metadata must not become turn activity/history"
    );
}

/// RAISE after a real UPDATE proves rollback of bytes; absence is a separate false result.
#[test]
fn obsessive_pin_atomic_update_failure_rolls_back_and_absent_row_is_not_success() {
    let db = PinDb::new();
    db.create("a");
    let action = PreferenceActions {
        obsessive: Some(Some(original_a())),
        ..Default::default()
    };
    assert!(!db
        .store
        .merge_preference_actions("rowless", &action)
        .unwrap());
    let before = db.raw_pin("a");
    db.store.lock_conn().execute_batch("CREATE TRIGGER fail_obsessive AFTER UPDATE OF preference_pin ON conversations BEGIN SELECT RAISE(ABORT, 'obsessive_fixture_update'); END;").unwrap();
    let error = db.store.merge_preference_actions("a", &action).unwrap_err();
    assert!(
        format!("{error:#}").contains("obsessive_fixture_update"),
        "UPDATE never reached fixture trigger: {error:#}"
    );
    assert_eq!(db.raw_pin("a"), before);
    db.store
        .lock_conn()
        .execute_batch("DROP TRIGGER fail_obsessive;")
        .unwrap();
    assert!(db.store.merge_preference_actions("a", &action).unwrap());
    assert_eq!(
        db.store
            .preference_pin("a")
            .unwrap()
            .unwrap()
            .obsessive
            .as_ref()
            .unwrap()
            .verified_selection("a", &db.store.workspace_id)
            .unwrap(),
        original_a()
    );
}

/// Schema is checked explicitly; malformed JSON remains a decode refusal.
/// Neither negative substitutes for the well-formed same-CID selector test.
#[test]
fn obsessive_pin_schema_change_and_malformed_json_preserve_evidence() {
    let db = PinDb::new();
    db.create("a");
    let valid = serde_json::to_value(db.pin("a", original_a())).unwrap();
    let mut wrong_schema = valid.clone();
    wrong_schema["obsessive"]["inverse"]["schema"] =
        serde_json::json!("unrecognized-fixture-schema");
    let _: OperatorPreferencePin = serde_json::from_value(wrong_schema.clone()).unwrap();
    for raw in [wrong_schema.to_string(), "{malformed".into()] {
        db.store.set_raw_preference_pin_for_test("a", &raw).unwrap();
        assert!(db.store.preference_pin("a").is_err());
        assert_eq!(db.raw_pin("a"), raw);
    }
}

/// The acted mode is addressed with its selectors, not an unverified side flag.
#[test]
fn obsessive_pin_mode_change_with_old_cid_is_refused() {
    let db = PinDb::new();
    db.create("mode");
    let mut value = serde_json::to_value(db.pin("mode", original_a())).unwrap();
    value["obsessive"]["inverse"]["mode"] = serde_json::json!("off");
    value["cognition"] = serde_json::json!("off");
    value["tenacity"] = serde_json::Value::Null;
    let typed: OperatorPreferencePin = serde_json::from_value(value.clone()).unwrap();
    assert!(db.store.update_preference_pin("mode", &typed).is_err());
    db.restore_raw("mode", &value);
    assert!(db.store.preference_pin("mode").is_err());
    assert_eq!(db.raw_pin("mode"), value.to_string());
}

/// Missing mode cannot turn a prior explicit choice into an inferred default.
#[test]
fn obsessive_pin_missing_mode_is_refused() {
    let db = PinDb::new();
    db.create("mode");
    let mut value = serde_json::to_value(db.pin("mode", original_a())).unwrap();
    value["obsessive"]["inverse"]
        .as_object_mut()
        .unwrap()
        .remove("mode");
    db.restore_raw("mode", &value);
    assert!(db.store.preference_pin("mode").is_err());
    assert_eq!(db.raw_pin("mode"), value.to_string());
}

/// While off, later accepted dial edits update the addressed current selection.
#[test]
fn obsessive_pin_off_follows_only_later_acted_selectors() {
    let db = PinDb::new();
    db.create("off");
    db.pin("off", original_a());
    db.store
        .merge_preference_actions(
            "off",
            &PreferenceActions {
                obsessive: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    let previous = db.store.preference_pin("off").unwrap().unwrap();
    let selected = original_b();
    db.store
        .merge_preference_actions(
            "off",
            &PreferenceActions {
                cognition: Some(selected.cognition),
                tenacity: Some(selected.tenacity),
                model: Some(Some("new-model".into())),
                ..Default::default()
            },
        )
        .unwrap();
    let next = db.store.preference_pin("off").unwrap().unwrap();
    assert_ne!(next.obsessive, previous.obsessive);
    let state = next.obsessive.as_ref().unwrap();
    assert_eq!(state.mode(), crate::psyche::ObsessiveMode::Off);
    assert_eq!(
        state
            .verified_selection("off", &db.store.workspace_id)
            .unwrap(),
        selected
    );
    assert_eq!(next.cognition, None);
    assert_eq!(next.tenacity, selected.tenacity);
    assert_eq!(next.model.as_deref(), Some("new-model"));
}

/// No stored original and no acted selectors is missing evidence, not auto.
#[test]
fn obsessive_pin_off_without_original_or_acted_axes_refuses_without_write() {
    let db = PinDb::new();
    db.create("off");
    let before = db.raw_pin("off");
    assert!(db
        .store
        .merge_preference_actions(
            "off",
            &PreferenceActions {
                obsessive: Some(None),
                ..Default::default()
            }
        )
        .is_err());
    assert_eq!(db.raw_pin("off"), before);
}
