use super::*;

/// PR #2445: Config::resolve can republish defaults from inside a running turn.
/// The nudge must retain the number captured at its boundary, not reread globals.
#[test]
fn psyche_split_fix_pins_nudge_budget_across_config_republication() {
    let _settings = crate::test_guard::GlobalSettingsGuard::acquire();
    set_cli_initiative(Initiative::Measured);
    set_initiative_config(InitiativeConfig {
        rounds: InitiativeRounds {
            measured: 9,
            ..InitiativeRounds::default()
        },
        ..InitiativeConfig::default()
    });
    let turn = crate::psyche::capture_turn_psyche();
    // The same publication performed by Config::publish_runtime_settings.
    set_initiative_config(InitiativeConfig::default());
    assert_eq!(effective_initiative().read_only_nudge_after(), 9);
    {
        let _nested = crate::psyche::capture_turn_psyche();
        assert_eq!(effective_initiative().read_only_nudge_after(), 9);
    }
    assert_eq!(effective_initiative().read_only_nudge_after(), 9);
    let other_thread = std::thread::spawn(|| effective_initiative().read_only_nudge_after())
        .join()
        .unwrap();
    assert_eq!(
        other_thread, 3,
        "the captured budget must stay on its thread"
    );
    drop(turn);
    assert_eq!(effective_initiative().read_only_nudge_after(), 3);
}
