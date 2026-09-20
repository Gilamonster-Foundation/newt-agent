//! Reversible effort posture exercised through the production text handler.

use newt_core::cognition::{cli_cognition, CognitionOverride};
use newt_core::initiative::{cli_initiative, Initiative};
use newt_core::role_profile::Cognition;
use newt_core::runtime::drain_preference_actions;
use newt_core::tenacity::{cli_tenacity, effective_tenacity, Tenacity};

fn selectors() -> (CognitionOverride, Option<Tenacity>, Option<Initiative>) {
    (cli_cognition(), cli_tenacity(), cli_initiative())
}

fn choose(cognition: &str, tenacity: &str, initiative: &str) {
    for command in [
        format!("cognition {cognition}"),
        format!("tenacity {tenacity}"),
        format!("initiative {initiative}"),
    ] {
        let _ = super::psyche_command(&command);
    }
    let _ = drain_preference_actions();
}

/// Control: the production text route distinguishes auto, off and explicit levels.
#[test]
fn obsessive_text_ordinary_selector_control() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    choose("off", "normal", "auto");
    assert_eq!(
        selectors(),
        (CognitionOverride::Off, Some(Tenacity::Normal), None)
    );
    choose("auto", "auto", "eager");
    assert_eq!(
        selectors(),
        (CognitionOverride::Unset, None, Some(Initiative::Eager))
    );
}

/// Regression: the old handler ignores `off` and engages the posture instead.
#[test]
fn obsessive_text_off_while_off_is_noop() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    choose("rational", "normal", "eager");
    let before = selectors();
    let _ = super::psyche_command("obsessive off");
    assert_eq!(selectors(), before);
    assert!(drain_preference_actions().is_empty());
}

fn round_trip(cognition: &str, tenacity: &str, initiative: &str) {
    choose(cognition, tenacity, initiative);
    let before = selectors();
    let _ = super::psyche_command("obsessive on");
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    assert_eq!(cli_initiative(), before.2);
    let _ = super::psyche_command("obsessive off");
    assert_eq!(
        selectors(),
        before,
        "restore selectors, not resolved values"
    );
}

/// Regression: auto selections must return to normal default resolution on off.
#[test]
fn obsessive_text_restores_auto_selectors() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    round_trip("auto", "auto", "auto");
}

/// Regression: explicit cognition off must survive a temporary posture overlay.
#[test]
fn obsessive_text_restores_explicit_off() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    round_trip("off", "normal", "patient");
}

/// Regression: leaving obsessive must restore the operator's explicit selections.
#[test]
fn obsessive_text_restores_explicit_levels() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    round_trip("rational", "resolute", "eager");
}

/// Regression: repeated on must not overwrite the original restore snapshot.
#[test]
fn obsessive_text_repeated_on_preserves_original() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    choose("off", "auto", "eager");
    let original = selectors();
    let _ = super::psyche_command("obsessive on");
    let _ = drain_preference_actions();
    let _ = super::psyche_command("obsessive on");
    assert!(drain_preference_actions().is_empty(), "idempotent setter");
    let _ = super::psyche_command("obsessive off");
    assert_eq!(selectors(), original);
}

/// Regression: the retained bare alias must use the reversible toggle.
#[test]
fn obsessive_text_bare_alias_toggles_back() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    choose("rational", "normal", "eager");
    let original = selectors();
    let _ = super::psyche_command("obsessive");
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    let _ = super::psyche_command("obsessive");
    assert_eq!(selectors(), original);
}

fn locked_text_setter(command: &str) {
    choose("rational", "normal", "eager");
    let _ = super::psyche_command("obsessive on");
    let active = selectors();
    let _ = drain_preference_actions();
    let message = super::psyche_command(command);
    assert_eq!(
        selectors(),
        active,
        "a text setter cannot defeat the overlay"
    );
    assert!(
        drain_preference_actions().is_empty(),
        "refusal cannot create a pin"
    );
    assert!(
        message.contains("obsessive") && message.contains("off"),
        "{message}"
    );
}

/// Regression: cognition text edits cannot silently replace an active posture.
#[test]
fn obsessive_text_locks_cognition() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    locked_text_setter("cognition thoughtful");
}

/// Regression: tenacity text edits cannot silently replace an active posture.
#[test]
fn obsessive_text_locks_tenacity() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    locked_text_setter("tenacity normal");
}

/// Regression: initiative stays selected as before, and its text row is locked too.
#[test]
fn obsessive_text_locks_initiative() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    locked_text_setter("initiative patient");
}

/// Regression: repeated off remains a no-op after successful restoration.
#[test]
fn obsessive_text_repeated_off_is_noop() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    round_trip("rational", "normal", "patient");
    let original = selectors();
    let _ = drain_preference_actions();
    let _ = super::psyche_command("obsessive off");
    assert_eq!(selectors(), original);
    assert!(drain_preference_actions().is_empty());
    assert_eq!(original.0, CognitionOverride::Set(Cognition::Rational));
}

fn locked_settings_form(field: crate::settings_form::Field, value: &str) {
    choose("rational", "normal", "eager");
    let _ = super::psyche_command("obsessive on");
    let active = selectors();
    let _ = drain_preference_actions();
    let result = crate::settings_form::apply_and_record(field, value, "/settings");
    assert_eq!(
        selectors(),
        active,
        "the shared writer must enforce the lock"
    );
    assert!(drain_preference_actions().is_empty());
    let message = result.expect_err("an active posture refuses independent dial edits");
    assert!(
        message.contains("obsessive") && message.contains("off"),
        "{message}"
    );
}

/// Regression: the shared /settings route cannot bypass the cognition lock.
#[test]
fn obsessive_settings_locks_cognition() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    locked_settings_form(crate::settings_form::Field::Cognition, "thoughtful");
}

/// Regression: the shared /settings route cannot bypass the tenacity lock.
#[test]
fn obsessive_settings_locks_tenacity() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    locked_settings_form(crate::settings_form::Field::Tenacity, "normal");
}

/// Regression: the shared /settings route also preserves initiative's selection.
#[test]
fn obsessive_settings_locks_initiative() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    locked_settings_form(crate::settings_form::Field::Initiative, "patient");
}

/// Regression: the posture selects executable Exhaustive, beyond the former Meticulous level.
#[test]
fn obsessive_text_selects_exhaustive_without_changing_crew_gate() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    choose("off", "normal", "patient");
    let crew = std::env::var_os("NEWT_TEAM");
    let message = super::psyche_command("obsessive on");
    assert_eq!(
        newt_core::cognition::effective_cognition().map(|level| level.label()),
        Some("exhaustive"),
    );
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    assert_eq!(cli_initiative(), Some(Initiative::Patient));
    assert_eq!(std::env::var_os("NEWT_TEAM"), crew);
    assert!(
        message.contains("launch"),
        "crew is a startup gate: {message}"
    );
}

/// Regression: restored auto follows the current persona, not its pre-toggle resolved value.
#[test]
fn obsessive_text_restored_auto_resolves_changed_defaults() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    choose("auto", "auto", "auto");
    newt_core::cognition::set_persona_cognition(Some(Cognition::Rational));
    newt_core::tenacity::set_persona_tenacity(Some(Tenacity::Normal));
    newt_core::initiative::set_persona_initiative(Some(Initiative::Patient));
    let _ = super::psyche_command("obsessive on");
    newt_core::cognition::set_persona_cognition(Some(Cognition::Thoughtful));
    newt_core::tenacity::set_persona_tenacity(Some(Tenacity::Resolute));
    newt_core::initiative::set_persona_initiative(Some(Initiative::Eager));
    // Initiative retains auto even while the overlay is engaged.
    assert_eq!(cli_initiative(), None);
    assert_eq!(
        newt_core::initiative::effective_initiative(),
        Initiative::Eager
    );
    let _ = super::psyche_command("obsessive off");
    assert_eq!(selectors(), (CognitionOverride::Unset, None, None));
    assert_eq!(
        newt_core::cognition::effective_cognition(),
        Some(Cognition::Thoughtful)
    );
    assert_eq!(effective_tenacity(), Tenacity::Resolute);
}

/// Regression: toggling settings cannot rewrite an already captured turn's effort policy.
#[test]
fn obsessive_text_preserves_captured_turn_then_restores_selectors() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    choose("off", "normal", "patient");
    let before = selectors();
    let captured = newt_core::psyche::capture_turn_psyche();
    let _ = super::psyche_command("obsessive on");
    assert_eq!(newt_core::cognition::effective_cognition(), None);
    assert_eq!(effective_tenacity(), Tenacity::Normal);
    assert_eq!(
        newt_core::initiative::effective_initiative(),
        Initiative::Patient
    );
    drop(captured);
    assert_eq!(effective_tenacity(), Tenacity::Relentless);
    let _ = super::psyche_command("obsessive off");
    assert_eq!(selectors(), before);
}
