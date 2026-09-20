//! Resolving config for a VALUE must not republish process-global runtime
//! settings. `Config::resolve` also publishes them (initiative rounds, output
//! budgets, lifecycle overrides), so a reader that only wants one value used to
//! rewrite what a running turn had captured, and raced with every test or
//! caller that owns those globals.

use crate::initiative::{self, Initiative, InitiativeConfig, InitiativeRounds};

fn marker() -> InitiativeConfig {
    InitiativeConfig {
        default: Some(Initiative::Eager),
        families: Default::default(),
        rounds: InitiativeRounds {
            patient: 91,
            measured: 92,
            decisive: 93,
            eager: 94,
        },
    }
}

fn published_patient_rounds() -> Option<usize> {
    initiative::initiative_config().map(|c| c.rounds.patient)
}

/// The readers that only need a config value: the thinking mode (read every
/// turn), the memory window and the soul-file override.
#[test]
fn value_readers_do_not_republish_runtime_settings() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    crate::process_env::remove_var("NEWT_THINKING");
    initiative::set_initiative_config(marker());

    let _ = crate::agentic::thinking_mode();
    assert_eq!(published_patient_rounds(), Some(91), "thinking_mode");
    let _ = crate::memory::RollingWindow::from_config();
    assert_eq!(published_patient_rounds(), Some(91), "memory window");
    let _ = crate::memory::SoulProvider::from_config();
    assert_eq!(published_patient_rounds(), Some(91), "soul provider");
}

/// Control: the publishing resolve really does overwrite the marker, and the
/// unpublished one does not. Without it the test above could pass vacuously.
#[test]
fn resolve_publishes_and_resolve_unpublished_does_not() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    initiative::set_initiative_config(marker());
    let _ = crate::Config::resolve_unpublished();
    assert_eq!(published_patient_rounds(), Some(91));
    let _ = crate::Config::resolve();
    assert_ne!(
        published_patient_rounds(),
        Some(91),
        "control: resolve() must publish, or this suite proves nothing"
    );
}
