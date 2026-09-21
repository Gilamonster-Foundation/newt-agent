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
/// turn), the memory window, the soul-file override, the markdown mode and the
/// compaction-trigger policy.
#[test]
fn value_readers_do_not_republish_runtime_settings() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    for key in ["NEWT_THINKING", "NEWT_MARKDOWN", "NEWT_COMPACTION_TRIGGER"] {
        crate::process_env::remove_var(key);
    }
    initiative::set_initiative_config(marker());
    let mut notices = Vec::new();
    let mut report = |notice: crate::tty::Notice<'static>| notices.push(notice);

    let _ = crate::agentic::thinking_mode(&mut report);
    assert_eq!(published_patient_rounds(), Some(91), "thinking_mode");
    let _ = crate::memory::RollingWindow::from_config(&mut report);
    assert_eq!(published_patient_rounds(), Some(91), "memory window");
    let _ = crate::memory::SoulProvider::from_config(&mut report);
    assert_eq!(published_patient_rounds(), Some(91), "soul provider");
    let _ = crate::config::session_markdown_mode(&mut report);
    assert_eq!(published_patient_rounds(), Some(91), "markdown mode");
    let _ = crate::config::session_compaction_trigger_policy(&mut report);
    assert_eq!(
        published_patient_rounds(),
        Some(91),
        "compaction trigger policy"
    );
}

/// Control: publishing really does overwrite the marker, and the unpublished
/// resolve does not. Without it the test above could pass vacuously. Publishes
/// a default config directly: `resolve()` reads whatever cwd and environment
/// other tests have pinned, and can fail, which would make the control flaky.
#[test]
fn publishing_rewrites_the_marker_and_resolve_unpublished_does_not() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    initiative::set_initiative_config(marker());
    let _ = crate::Config::resolve_unpublished(&mut |_| {});
    assert_eq!(published_patient_rounds(), Some(91));
    crate::Config::default().publish_runtime_settings();
    assert_ne!(
        published_patient_rounds(),
        Some(91),
        "control: publishing must rewrite the marker, or this suite proves nothing"
    );
}
