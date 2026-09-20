//! Reversible posture at the real panel keyboard and commit boundaries.
use super::*;
use crate::panel::{Flow, Screen};
use newt_core::runtime::drain_preference_actions;
use newt_core::test_guard::GlobalSettingsGuard;

fn selectors() -> (CognitionOverride, Option<Tenacity>, Option<Initiative>) {
    (cli_cognition(), cli_tenacity(), cli_initiative())
}

fn arrange() {
    for (field, value) in [
        (crate::settings_form::Field::Cognition, "off"),
        (crate::settings_form::Field::Tenacity, "auto"),
        (crate::settings_form::Field::Initiative, "eager"),
    ] {
        crate::settings_form::apply_and_record(field, value, "/settings").unwrap();
    }
    let _ = drain_preference_actions();
}

fn screen() -> PsycheScreen<impl FnMut(&str, &str, bool) -> SaveResult> {
    PsycheScreen {
        state: super::tests::panel(None, Vec::new(), Initiative::Measured),
        persist: |name: &str, _: &str, _: bool| SaveResult::Saved { name: name.into() },
    }
}

fn select<P: FnMut(&str, &str, bool) -> SaveResult>(screen: &mut PsycheScreen<P>, label: &str) {
    // Navigate the actual key table; no new row variant or state mutation is
    // required to compile this regression against the old panel.
    for _ in 0..=ROWS.len() {
        if screen
            .state
            .view_rows()
            .iter()
            .any(|row| row.selected && row.label == label)
        {
            return;
        }
        assert!(matches!(screen.key(Key::Down), Flow::Stay));
    }
    panic!("the panel has no selectable {label} row");
}

fn command<P: FnMut(&str, &str, bool) -> SaveResult>(
    screen: &mut PsycheScreen<P>,
    text: &str,
) -> Flow {
    assert!(matches!(screen.key(Key::Char(':')), Flow::Stay));
    for ch in text.chars() {
        assert!(matches!(screen.key(Key::Char(ch)), Flow::Stay));
    }
    screen.key(Key::Enter)
}

/// Control: keyboard draft edits only become live at the existing close boundary.
#[test]
fn obsessive_panel_ordinary_draft_control() {
    let _guard = GlobalSettingsGuard::acquire();
    arrange();
    let before = selectors();
    let mut panel = screen();
    select(&mut panel, "tenacity");
    assert!(matches!(panel.key(Key::Right), Flow::Stay));
    assert_eq!(selectors(), before);
    assert!(drain_preference_actions().is_empty());
    assert!(matches!(panel.key(Key::Enter), Flow::Close(true)));
    assert!(matches!(
        close_outcome(true, &panel.state),
        PanelOutcome::Applied { .. }
    ));
    assert_ne!(selectors().1, before.1);
}

fn locked_row(label: &str) {
    arrange();
    crate::commands::settings::dispatch(
        "psyche",
        "obsessive",
        "/psyche obsessive on",
        ".",
        false,
        false,
    )
    .unwrap();
    let live = selectors();
    let _ = drain_preference_actions();
    let mut panel = screen();
    select(&mut panel, label);
    let before = (
        panel.state.cognition,
        panel.state.tenacity,
        panel.state.initiative,
    );
    for key in [Key::Left, Key::Right] {
        assert!(matches!(panel.key(key), Flow::Stay));
        assert_eq!(
            (
                panel.state.cognition,
                panel.state.tenacity,
                panel.state.initiative
            ),
            before,
            "locked keys must not dirty even the draft",
        );
    }
    let rows = panel.state.view_rows();
    let row = rows.iter().find(|row| row.label == label).unwrap();
    assert!(!row.editable);
    assert!(
        row.provenance.contains("obsessive"),
        "a textual lock reason is required"
    );
    assert!(matches!(
        close_outcome(true, &panel.state),
        PanelOutcome::Cancelled
    ));
    assert_eq!(selectors(), live);
    assert!(drain_preference_actions().is_empty());
}

/// Regression: active obsessive cognition cannot be changed by arrow keys.
#[test]
fn obsessive_panel_locks_cognition() {
    let _guard = GlobalSettingsGuard::acquire();
    locked_row("cognition");
}

/// Regression: active obsessive tenacity cannot be changed by arrow keys.
#[test]
fn obsessive_panel_locks_tenacity() {
    let _guard = GlobalSettingsGuard::acquire();
    locked_row("tenacity");
}

/// Regression: initiative retains its selection but its row is locked too.
#[test]
fn obsessive_panel_locks_initiative() {
    let _guard = GlobalSettingsGuard::acquire();
    locked_row("initiative");
}

/// Regression: cancelling a draft posture never mutates live selectors or pins.
#[test]
fn obsessive_panel_cancel_discards_posture_draft() {
    let _guard = GlobalSettingsGuard::acquire();
    arrange();
    let before = selectors();
    let mut panel = screen();
    select(&mut panel, "obsessive");
    panel.key(Key::Right);
    assert_eq!(selectors(), before);
    assert!(drain_preference_actions().is_empty());
    assert!(matches!(panel.key(Key::Esc), Flow::Close(false)));
    assert_eq!(close_outcome(false, &panel.state), PanelOutcome::Cancelled);
    assert_eq!(selectors(), before);
    assert!(drain_preference_actions().is_empty());
    let reopened = screen();
    let rows = reopened.state.view_rows();
    assert_eq!(
        rows.iter()
            .find(|row| row.label == "obsessive")
            .unwrap()
            .value,
        "off"
    );
}

/// Regression: failed :wq must reach the writer and keep posture entirely provisional.
#[test]
fn obsessive_panel_failed_save_preserves_live_state() {
    let _guard = GlobalSettingsGuard::acquire();
    arrange();
    let before = selectors();
    let called = std::cell::Cell::new(false);
    let mut panel = PsycheScreen {
        state: super::tests::panel(None, Vec::new(), Initiative::Measured),
        persist: |_: &str, _: &str, _: bool| {
            called.set(true);
            SaveResult::Failed("fixture write failure".into())
        },
    };
    select(&mut panel, "obsessive");
    panel.key(Key::Right);
    assert!(matches!(command(&mut panel, "wq fixture"), Flow::Stay));
    assert!(
        called.get(),
        "exercise persistence failure, not a parser refusal"
    );
    assert!(panel
        .state
        .status
        .as_deref()
        .unwrap_or_default()
        .contains("fixture write failure"));
    assert_eq!(selectors(), before);
    assert!(drain_preference_actions().is_empty());
    assert_eq!(close_outcome(false, &panel.state), PanelOutcome::Cancelled);
}

/// Regression: :w saves the draft without applying the posture to the session.
#[test]
fn obsessive_panel_save_without_apply_preserves_live_state() {
    let _guard = GlobalSettingsGuard::acquire();
    arrange();
    let before = selectors();
    let mut panel = screen();
    select(&mut panel, "obsessive");
    panel.key(Key::Right);
    assert!(matches!(command(&mut panel, "w fixture"), Flow::Stay));
    assert_eq!(selectors(), before);
    assert!(drain_preference_actions().is_empty());
    assert_eq!(
        close_outcome(false, &panel.state),
        PanelOutcome::Saved {
            name: "fixture".into()
        }
    );
    assert_eq!(selectors(), before);
}

/// Regression: applying on, reopening, then applying off restores off/auto selectors.
#[test]
fn obsessive_panel_apply_then_restore_original_selectors() {
    let _guard = GlobalSettingsGuard::acquire();
    arrange();
    let before = selectors();
    let mut on = screen();
    select(&mut on, "obsessive");
    on.key(Key::Right);
    assert_eq!(selectors(), before);
    assert!(matches!(
        close_outcome(true, &on.state),
        PanelOutcome::Applied { .. }
    ));
    assert_eq!(
        newt_core::tenacity::effective_tenacity(),
        Tenacity::Relentless
    );
    assert_eq!(cli_initiative(), before.2);
    let _ = drain_preference_actions();
    let active = selectors();
    let mut off = screen();
    select(&mut off, "obsessive");
    off.key(Key::Left);
    assert_eq!(selectors(), active);
    assert!(drain_preference_actions().is_empty());
    assert!(matches!(
        close_outcome(true, &off.state),
        PanelOutcome::Applied { .. }
    ));
    assert_eq!(selectors(), before);
}
