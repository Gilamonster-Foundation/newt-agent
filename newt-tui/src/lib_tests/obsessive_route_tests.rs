//! UNCOMPILED / UNRUN. Mount as a cfg(test) child of newt-tui/src/lib.rs.
//! Calls the actual production route, not commands::settings directly.
use newt_core::cognition::{cli_cognition, CognitionOverride};
use newt_core::initiative::{cli_initiative, Initiative};
use newt_core::psyche;
use newt_core::runtime::drain_preference_actions;
use newt_core::settings_receipt::{SettingReceipt, SettingValue};
use newt_core::tenacity::cli_tenacity;

fn reset_route_inputs() {
    psyche::restore_obsessive_selection(None);
    let _ = drain_preference_actions();
    newt_core::runtime::record_cli_preference_axes(Default::default());
    newt_core::cognition::set_cli_cognition(CognitionOverride::Off);
    newt_core::tenacity::clear_cli_tenacity();
    newt_core::initiative::set_cli_initiative(Initiative::Eager);
}

fn selectors() -> (
    CognitionOverride,
    Option<newt_core::Tenacity>,
    Option<Initiative>,
) {
    (cli_cognition(), cli_tenacity(), cli_initiative())
}

fn route(input: &str) {
    let no_question = |_: &newt_core::interaction_surface::SurfaceInteraction| -> newt_core::HumanQuestionOutcome {
        panic!("an explicit Obsessive route must not open the settings form or psyche panel")
    };
    assert!(
        super::dispatch_slash_with_ask(input, ".", false, false, false, Some(&no_question))
            .unwrap()
    );
}

fn receipt_path(root: &std::path::Path) -> std::path::PathBuf {
    let path = root.join("receipts.jsonl");
    // Every caller owns GlobalSettingsGuard, which restores this key.
    std::env::set_var(newt_core::settings_receipt::RECEIPT_PATH_ENV, &path);
    path
}

fn read_receipts(path: &std::path::Path) -> Vec<SettingReceipt> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => panic!("fixture receipt read failed: {error}"),
    };
    let receipts = newt_core::settings_receipt::read_jsonl(&body);
    assert_eq!(
        receipts.len(),
        body.lines().filter(|line| !line.trim().is_empty()).count(),
        "malformed receipt lines must not silently disappear in tolerant readback"
    );
    assert!(receipts.iter().all(SettingReceipt::is_intact));
    receipts
}

/// The requested short command reaches the real reducer and restores exact inputs.
#[test]
fn obsessive_top_level_route_restores_original_selectors_and_marks_actions() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_route_inputs();
    let original = selectors();
    let crew_before = std::env::var_os("NEWT_TEAM");
    route("/obsessive on");
    assert!(
        psyche::obsessive_selection().is_some(),
        "missing top-level route"
    );
    assert_eq!(
        cli_cognition(),
        CognitionOverride::Set(psyche::OBSESSIVE_COGNITION)
    );
    assert_eq!(cli_tenacity(), Some(psyche::OBSESSIVE_TENACITY));
    assert_eq!(cli_initiative(), original.2);
    assert!(drain_preference_actions().obsessive.is_some());
    route("/obsessive off");
    assert!(psyche::obsessive_selection().is_none());
    assert_eq!(selectors(), original);
    assert!(drain_preference_actions().obsessive.is_some());
    assert_eq!(std::env::var_os("NEWT_TEAM"), crew_before);
}

/// Bare /obsessive is the reversible toggle, not bare /psyche's panel/status.
#[test]
fn obsessive_top_level_bare_route_toggles_without_asking() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_route_inputs();
    let original = selectors();
    route("/obsessive");
    assert!(psyche::obsessive_selection().is_some());
    route("/obsessive");
    assert!(psyche::obsessive_selection().is_none());
    assert_eq!(selectors(), original);
}

/// Each real transition writes once; repeated explicit setters write nothing.
#[test]
fn obsessive_top_level_idempotence_preserves_original_and_receipt_count() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_route_inputs();
    let root = tempfile::tempdir().unwrap();
    let path = receipt_path(root.path());
    route("/obsessive on");
    let original =
        psyche::obsessive_selection().expect("route must engage before no-op checks count");
    let _ = drain_preference_actions();
    let once = read_receipts(&path);
    assert_eq!(once.len(), 1);
    route("/obsessive on");
    assert_eq!(psyche::obsessive_selection(), Some(original));
    assert!(drain_preference_actions().is_empty());
    assert_eq!(read_receipts(&path), once);
    route("/obsessive off");
    let _ = drain_preference_actions();
    let twice = read_receipts(&path);
    assert_eq!(twice.len(), 2);
    route("/obsessive off");
    assert!(drain_preference_actions().is_empty());
    assert_eq!(read_receipts(&path), twice);
}

/// Invalid full tails cannot be truncated to a valid arg1 and mutate the overlay.
#[test]
fn obsessive_top_level_invalid_arguments_are_read_only() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_route_inputs();
    let root = tempfile::tempdir().unwrap();
    let path = receipt_path(root.path());
    let original = selectors();
    for invalid in [
        "/obsessive banana",
        "/obsessive on extra",
        "/obsessive off extra",
    ] {
        route(invalid);
        assert!(psyche::obsessive_selection().is_none());
        assert_eq!(selectors(), original);
        assert!(drain_preference_actions().is_empty());
        assert!(read_receipts(&path).is_empty());
    }
    // Positive route control prevents the inactive unknown-route no-op from
    // masquerading as complete argument validation.
    route("/obsessive on");
    let captured = psyche::obsessive_selection().expect("real route prerequisite");
    let _ = drain_preference_actions();
    let before = read_receipts(&path);
    for invalid in ["/obsessive banana", "/obsessive off extra"] {
        route(invalid);
        assert_eq!(psyche::obsessive_selection(), Some(captured));
        assert!(drain_preference_actions().is_empty());
        assert_eq!(read_receipts(&path), before);
    }
}

/// Both doors use the same canonical setting, and retain the actual typed route.
#[test]
fn obsessive_top_level_and_long_routes_each_record_their_actual_via_once() {
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_route_inputs();
    let root = tempfile::tempdir().unwrap();
    let path = receipt_path(root.path());
    for input in [
        "/obsessive on",
        "/psyche obsessive off",
        "/psyche obsessive on",
        "/obsessive off",
    ] {
        route(input);
    }
    let receipts = read_receipts(&path);
    assert_eq!(
        receipts.len(),
        4,
        "one settings receipt per real transition, no router duplicate"
    );
    for (receipt, (via, from, to)) in receipts.iter().zip([
        ("/obsessive", "off", "on"),
        ("/psyche obsessive", "on", "off"),
        ("/psyche obsessive", "off", "on"),
        ("/obsessive", "on", "off"),
    ]) {
        assert_eq!(
            receipt.change.setting, "psyche",
            "reuse existing canonical recorder"
        );
        assert_eq!(
            receipt.change.via, via,
            "do not disguise short-route provenance as long spelling"
        );
        assert_eq!(receipt.change.from, SettingValue::from(from));
        assert_eq!(receipt.change.to, SettingValue::from(to));
    }
}

/// The requested new slash action must be counted, not hidden as a panel alias.
#[test]
fn obsessive_top_level_registry_names_its_real_surface_and_receipt_owner() {
    use crate::slash_registry::{self, Disposition, Family, Receipt, Surface};
    let command = slash_registry::lookup("obsessive").expect("register the actual new route");
    assert_eq!(command.name, "obsessive");
    assert_eq!(command.surface, Surface::Slash);
    assert_eq!(command.family, Family::Tuning);
    assert_eq!(command.disposition, Disposition::Keep);
    assert_eq!(command.receipt, Receipt::Journal);
    assert_eq!(slash_registry::lookup("psyche").unwrap().name, "psyche");
    assert_ne!(
        command.name,
        slash_registry::lookup("psyche").unwrap().name,
        "bare psyche is not semantically identical to bare obsessive"
    );
    assert_eq!(
        slash_registry::slash_tokens()
            .iter()
            .filter(|token| **token == "obsessive")
            .count(),
        1
    );
}

/// Discoverability names the exact short restoration command requested by the operator.
#[test]
fn obsessive_top_level_help_lists_on_off_and_original_restoration() {
    let row = crate::help_lines()
        .iter()
        .find(|line| line.split_whitespace().next() == Some("/obsessive"))
        .expect("top-level help and completion corpus must list /obsessive");
    assert!(row.contains("on") && row.contains("off"), "{row}");
    let page = crate::help::command_help_page("obsessive").expect("dedicated command help");
    assert!(page.contains("/obsessive off"), "{page}");
    assert!(
        page.contains("original") && page.contains("restor"),
        "{page}"
    );
}

/// The actual rich palette must complete the new help-derived command, not submit it.
#[cfg(feature = "rich-tui")]
#[test]
fn obsessive_top_level_palette_completes_without_mutating_state() {
    use crate::palette::{palette_step, PaletteState, PaletteStep};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let _guard = newt_core::test_guard::GlobalSettingsGuard::acquire();
    reset_route_inputs();
    let original = selectors();
    let mut palette = PaletteState::from_corpus();
    palette.on_buffer_change("", "/");
    palette.on_buffer_change("/", "/obsess");
    assert!(palette.is_open());
    let completion = palette_step(
        &mut palette,
        &KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
    );
    assert_eq!(completion, PaletteStep::CompleteTo("/obsessive ".into()));
    assert_eq!(selectors(), original);
    assert!(psyche::obsessive_selection().is_none());
    assert!(drain_preference_actions().is_empty());
}
