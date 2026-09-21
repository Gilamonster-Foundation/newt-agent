//! The tool paths that read config for a value must not republish the
//! process-global runtime settings (see `config_tests/reads_do_not_publish.rs`).

use super::*;
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
        ..Default::default()
    }
}

fn published_patient_rounds() -> Option<usize> {
    initiative::initiative_config().map(|c| c.rounds.patient)
}

#[tokio::test]
async fn find_by_category_and_use_skill_do_not_republish_runtime_settings() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    let ws = tempfile::TempDir::new().unwrap();
    touch(ws.path(), "a.rs");
    initiative::set_initiative_config(marker());

    let found = run_tool(
        "find",
        serde_json::json!({ "category": "source", "type": "f" }),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;
    assert!(found.contains("a.rs"), "the find really ran: {found}");
    assert_eq!(published_patient_rounds(), Some(91), "find");

    let _ = run_tool(
        "use_skill",
        serde_json::json!({ "name": "no-such-skill" }),
        ws.path(),
        &caveats_rw(ws.path()),
        None,
    )
    .await;
    assert_eq!(published_patient_rounds(), Some(91), "use_skill");
}
