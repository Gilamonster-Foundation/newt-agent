//! UNCOMPILED / UNRUN. Mount as a child of tab_switch_tests/state_machine_tests.rs.
//! Uses its real Harness. No hand-installed degradation or alternative verifier.
use super::*;
use newt_core::cognition::{cli_cognition, CognitionOverride};
use newt_core::psyche::{self, ObsessiveSelection};
use newt_core::tenacity::cli_tenacity;

fn reset_lifecycle_inputs() {
    psyche::restore_obsessive_selection(None);
    let _ = newt_core::runtime::drain_preference_actions();
    newt_core::runtime::record_cli_preference_axes(Default::default());
    newt_core::cognition::set_cli_cognition(CognitionOverride::Off);
    newt_core::tenacity::clear_cli_tenacity();
    newt_core::initiative::clear_cli_initiative();
    // Every caller owns the existing GlobalSettingsGuard.
    std::env::remove_var("NEWT_PROVIDER");
    std::env::remove_var("NEWT_DGX_MODEL");
}

fn lifecycle_toggle(value: &str) {
    crate::commands::settings::dispatch(
        "psyche",
        "obsessive",
        &format!("/psyche obsessive {value}"),
        ".",
        false,
        false,
    )
    .unwrap();
}

fn raw_pin(raw: &rusqlite::Connection, id: &str) -> String {
    raw.query_row(
        "SELECT preference_pin FROM conversations WHERE id = ?1",
        [id],
        |row| row.get(0),
    )
    .unwrap()
}

/// Seed two REAL durable rows before the fault. No toggle creates a ghost row.
/// Return with A active, unchanged ordinary pin bytes, and an UPDATE-only fault.
fn failing_two_tabs() -> (
    Harness,
    TabSet,
    rusqlite::Connection,
    String,
    String,
    String,
) {
    reset_lifecycle_inputs();
    let (mut h, mut tabs) = Harness::new(&["sol"]);
    let a = h.active_conversation_id.clone();
    h.store.create_with_id(&a, "A", None).unwrap();
    h.store
        .append_turn(&a, "existing question", "existing answer")
        .unwrap();
    let b = h.durable("B");
    h.open_tab_on(&mut tabs, &b);
    activate_tab(&mut h.ctx(), &mut tabs, 0).unwrap();
    assert!(h.store.load_verified(&a).is_ok());
    let raw = rusqlite::Connection::open(h._root.path().join("conversations.db")).unwrap();
    let before = raw_pin(&raw, &a);
    let quoted = a.replace('\'', "''");
    raw.execute_batch(&format!(
        "CREATE TRIGGER fail_obsessive_lifecycle AFTER UPDATE OF preference_pin ON conversations \
         WHEN NEW.id = '{quoted}' BEGIN SELECT RAISE(ABORT, 'obsessive_lifecycle_update'); END;"
    ))
    .unwrap();
    // Prove the owned resource is genuinely failing the real production writer,
    // after decode/validation; no invalid-pin error may masquerade as SQL failure.
    let ordinary = h.store.preference_pin(&a).unwrap().unwrap();
    let error = h.store.update_preference_pin(&a, &ordinary).unwrap_err();
    assert!(format!("{error:#}").contains("obsessive_lifecycle_update"));
    assert_eq!(raw_pin(&raw, &a), before);
    assert!(h.store.load_verified(&a).is_ok());
    (h, tabs, raw, a, b, before)
}

fn captured_on() -> ObsessiveSelection {
    lifecycle_toggle("on");
    let original = psyche::obsessive_selection().expect("real on command captured inverse");
    assert_eq!(original.cognition, CognitionOverride::Off);
    assert_eq!(original.tenacity, None);
    original
}

/// Failed durable flush must degrade its owning tab, not just print a warning.
#[test]
fn obsessive_failed_durable_flush_marks_owner_degraded_and_retains_original() {
    let _guard = guard();
    let (mut h, mut tabs, raw, a, _b, before) = failing_two_tabs();
    let original = captured_on();
    h.ctx().deactivate(&mut tabs); // actual production flush/stash owner
    assert_eq!(raw_pin(&raw, &a), before);
    assert_eq!(h.active_conversation_id, a);
    assert!(
        tabs.active().pin_degraded.is_some(),
        "failed durable write needs visible refusal state"
    );
    assert_eq!(h.pending.obsessive, Some(Some(original)));
    assert!(
        h.store.load_verified(&a).is_ok(),
        "pin failure must not damage valid turns"
    );
}

/// A materialized tab must retain its failed transition through real activation.
#[test]
fn obsessive_failed_durable_transition_survives_switch_away_and_back() {
    let _guard = guard();
    let (mut h, mut tabs, raw, a, b, before) = failing_two_tabs();
    let original = captured_on();
    activate_tab(&mut h.ctx(), &mut tabs, 1).unwrap();
    assert_eq!(h.active_conversation_id, b);
    assert!(
        h.pending.is_empty(),
        "A's action must not become B's pending write"
    );
    assert!(
        psyche::obsessive_selection().is_none(),
        "A must not own B's overlay"
    );
    assert_eq!(raw_pin(&raw, &a), before);
    activate_tab(&mut h.ctx(), &mut tabs, 0).unwrap();
    assert_eq!(h.active_conversation_id, a);
    assert_eq!(
        h.pending.obsessive,
        Some(Some(original)),
        "materialized-tab switch lost original"
    );
    assert!(tabs.active().pin_degraded.is_some());
    assert_eq!(raw_pin(&raw, &a), before);
    assert_eq!(tabs.len(), 2);
    assert!(h
        .store
        .preference_pin(&b)
        .unwrap()
        .unwrap()
        .obsessive
        .is_none());
}

/// /tab retry must retain failure, then write the exact inverse before clearing it.
#[test]
fn obsessive_actual_tab_retry_commits_retained_transition_before_clearing_degradation() {
    let _guard = guard();
    let (mut h, mut tabs, raw, a, _b, before) = failing_two_tabs();
    let original = captured_on();
    h.ctx().deactivate(&mut tabs);
    assert_eq!(
        h.pending.obsessive,
        Some(Some(original)),
        "fixture requires actual retained failure"
    );
    handle_tab_action(TabAction::Retry, &mut h.ctx(), &mut tabs);
    assert_eq!(
        raw_pin(&raw, &a),
        before,
        "still-failing retry changed durable bytes"
    );
    assert_eq!(
        h.pending.obsessive,
        Some(Some(original)),
        "reread-only retry discarded pending original"
    );
    assert!(
        tabs.active().pin_degraded.is_some(),
        "failed retry must not claim success"
    );
    raw.execute_batch("DROP TRIGGER fail_obsessive_lifecycle;")
        .unwrap();
    handle_tab_action(TabAction::Retry, &mut h.ctx(), &mut tabs);
    assert!(
        h.pending.is_empty(),
        "successful commit must settle pending state"
    );
    assert!(tabs.active().pin_degraded.is_none());
    assert!(
        h.store
            .preference_pin(&a)
            .unwrap()
            .unwrap()
            .obsessive
            .is_some(),
        "actual mandatory reader must see the committed overlay before success"
    );
    assert_eq!(psyche::obsessive_selection(), Some(original));
    lifecycle_toggle("off");
    assert_eq!(cli_cognition(), original.cognition);
    assert_eq!(cli_tenacity(), original.tenacity);
    assert!(h.store.load_verified(&a).is_ok());
}

/// Closing the active tab cannot discard an UPDATE-failed durable transition.
#[test]
fn obsessive_active_close_refuses_to_lose_unsaved_durable_transition() {
    let _guard = guard();
    let (mut h, mut tabs, raw, a, _b, before) = failing_two_tabs();
    let original = captured_on();
    let claims_before: Vec<String> = tabs
        .claimed_conversations()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let result = close_tab(&mut h.ctx(), &mut tabs, 0);
    assert!(
        result.is_err(),
        "close must refuse the unsaved transition, not remove its owner"
    );
    assert_eq!(tabs.len(), 2);
    assert_eq!(h.active_conversation_id, a);
    assert_eq!(tabs.active_index(), 0);
    assert_eq!(
        tabs.claimed_conversations(),
        claims_before.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(h.pending.obsessive, Some(Some(original)));
    assert!(tabs.active().pin_degraded.is_some());
    assert_eq!(raw_pin(&raw, &a), before);
}

/// Closing an inactive failed tab also refuses; checking only live pending is insufficient.
#[test]
fn obsessive_inactive_close_command_refuses_to_drop_retained_transition() {
    let _guard = guard();
    let (mut h, mut tabs, raw, a, b, before) = failing_two_tabs();
    let original = captured_on();
    activate_tab(&mut h.ctx(), &mut tabs, 1).unwrap();
    assert_eq!(h.active_conversation_id, b);
    handle_tab_action(TabAction::Close(Some(0)), &mut h.ctx(), &mut tabs);
    assert_eq!(
        tabs.len(),
        2,
        "command closed the inactive owner of unsaved evidence"
    );
    assert_eq!(h.active_conversation_id, b);
    assert_eq!(raw_pin(&raw, &a), before);
    activate_tab(&mut h.ctx(), &mut tabs, 0).unwrap();
    assert_eq!(h.pending.obsessive, Some(Some(original)));
    assert!(tabs.active().pin_degraded.is_some());
}
