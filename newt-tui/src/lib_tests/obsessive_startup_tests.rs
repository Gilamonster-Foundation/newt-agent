// UNCOMPILED / UNRUN. Append inside lib_tests/resumed_preference_tests.rs,
// reusing its Session, reset_globals, cfg_with, store_in helpers.
// These exercise real settings -> persist -> store reader -> restore seams.
// They do not replace required TabSet/rowless/degraded-lifecycle integration.

/// Off chosen during this invocation overrides its old launch-on baseline.
#[test]
fn obsessive_explicit_off_survives_conversation_switch_after_launch_on() {
    let _guard = GlobalSettingsGuard::acquire();
    newt_core::psyche::restore_obsessive_selection(None);
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    // SAFETY: GlobalSettingsGuard owns/restores process environment.
    unsafe {
        std::env::set_var("NEWT_SETTINGS_RECEIPTS", root.path().join("receipts.jsonl"));
    }
    let store = store_in(root.path(), ws.path());
    let a = durable_conversation(&store, "acted off");
    let b = durable_conversation(&store, "unacted baseline");
    let cfg = cfg_with(&["sol"]);
    set_cli_cognition(CognitionOverride::Off);
    clear_cli_tenacity();
    assert!(newt_core::psyche::set_obsessive(true));
    record_cli_preference_axes(PreferenceAxes {
        cognition: true,
        tenacity: true,
        ..Default::default()
    });
    let baseline = PreferenceBaseline::snapshot(None, None);
    let mut session = Session::new(&cfg);
    assert_eq!(
        crate::settings_form::apply_obsessive(false, "review-test"),
        "obsessive: off"
    );
    session.drain(Some(&store), &a);
    assert!(session.pending.is_empty());
    assert!(newt_core::psyche::obsessive_selection().is_none());
    session.switch_to(Some(&store), &b, &baseline, &cfg);
    assert!(
        newt_core::psyche::obsessive_selection().is_some(),
        "unacted tab inherits launch"
    );
    session.switch_to(Some(&store), &a, &baseline, &cfg);
    assert!(
        newt_core::psyche::obsessive_selection().is_none(),
        "acted off must survive reactivation"
    );
    assert_eq!(cli_cognition(), CognitionOverride::Off);
    assert_eq!(cli_tenacity(), None, "auto must clear forced Relentless");
}

/// A later invocation's explicit launch-on still wins at actual startup resume.
#[test]
fn obsessive_new_explicit_launch_wins_over_prior_stored_off_at_startup() {
    let _guard = GlobalSettingsGuard::acquire();
    newt_core::psyche::restore_obsessive_selection(None);
    reset_globals();
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    // SAFETY: GlobalSettingsGuard owns/restores process environment.
    unsafe {
        std::env::set_var("NEWT_SETTINGS_RECEIPTS", root.path().join("receipts.jsonl"));
    }
    record_cli_preference_axes(PreferenceAxes::default());
    let store = store_in(root.path(), ws.path());
    let id = durable_conversation(&store, "stored off");
    let cfg = cfg_with(&["sol"]);
    let mut previous = Session::new(&cfg);
    assert_eq!(
        crate::settings_form::apply_obsessive(true, "review-test"),
        "obsessive: on"
    );
    previous.drain(Some(&store), &id);
    assert_eq!(
        crate::settings_form::apply_obsessive(false, "review-test"),
        "obsessive: off"
    );
    previous.drain(Some(&store), &id);
    // Fresh invocation selection, exercised through the existing startup gate.
    assert!(newt_core::psyche::set_obsessive(true));
    record_cli_preference_axes(PreferenceAxes {
        cognition: true,
        tenacity: true,
        ..Default::default()
    });
    let baseline = PreferenceBaseline::snapshot(None, None);
    let mut current = Session::new(&cfg);
    current.switch_at_startup(
        StartupConversation::ResumedHeld,
        Some(&store),
        &id,
        &baseline,
        &cfg,
    );
    assert!(newt_core::psyche::obsessive_selection().is_some());
    assert_eq!(
        cli_cognition(),
        CognitionOverride::Set(newt_core::psyche::OBSESSIVE_COGNITION)
    );
    assert_eq!(cli_tenacity(), Some(newt_core::psyche::OBSESSIVE_TENACITY));
}
