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

/// #2933 PR8: `use_skill` refuses a skill the operator switched off (`[skills] off`),
/// by name, while an installed skill that is on still loads.
#[tokio::test]
async fn use_skill_refuses_a_skill_switched_off() {
    let _guard = crate::test_guard::GlobalSettingsGuard::acquire();
    let _env = crate::process_env::lock();
    let ws = tempfile::TempDir::new().unwrap();
    let skills = tempfile::TempDir::new().unwrap();
    for name in ["alpha", "beta"] {
        let dir = skills.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: the {name} skill\n---\nBody of {name}.\n"),
        )
        .unwrap();
    }
    let config = ws.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[skills]\nsearch = [{:?}]\noff = [\"alpha\"]\n",
            skills.path().display().to_string()
        ),
    )
    .unwrap();
    let saved = std::env::var_os("NEWT_CONFIG");
    crate::process_env::set_var("NEWT_CONFIG", config.to_str().unwrap());
    let mut results = Vec::new();
    for name in ["alpha", "beta"] {
        results.push(
            run_tool(
                "use_skill",
                serde_json::json!({ "name": name }),
                ws.path(),
                &caveats_rw(ws.path()),
                None,
            )
            .await,
        );
    }
    let (off, on) = (results[0].clone(), results[1].clone());
    match saved {
        Some(v) => crate::process_env::set_var("NEWT_CONFIG", &v.to_string_lossy()),
        None => crate::process_env::remove_var("NEWT_CONFIG"),
    }
    assert!(
        off.contains("switched off") && !off.contains("Body of alpha"),
        "{off}"
    );
    assert!(on.contains("Body of beta"), "{on}");
}
