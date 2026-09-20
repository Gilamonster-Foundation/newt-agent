//! Actual failed check → corrective edit → fresh pass under standalone/strict pursuit.
use super::*;

async fn repaired(wire: &str, smart: bool, level: crate::Tenacity) {
    let _tenacity = crate::tenacity::scoped_tenacity_settings(level, Default::default());
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "original\n").unwrap();
    const CHECK: &str = "sh -c 'printf check >> attempts; grep -q repaired notes.txt'";
    let task = instruction(CHECK);
    let allowance = run_allowance::RunAllowance::new(12);
    let (auxiliary, complete) = actual_auxiliary().await;
    let run = run_turn_with_auxiliary(
        Turn {
            wire,
            smart,
            outcomes: false,
            check: CHECK,
            script: &[
                Step::Run(CHECK),
                Step::Done,
                Step::Tool(
                    "edit_file",
                    r#"{"path":"notes.txt","old_string":"original","new_string":"repaired"}"#,
                ),
                Step::Run(CHECK),
                Step::Done,
            ],
            cancel: None,
            max_tool_rounds: 8,
            caveats: Caveats::top(),
            env: &[
                ("NEWT_SELF_VERIFY", "0"),
                ("NEWT_SHELL_ENGINE", "safe-subset"),
            ],
            workspace_task: Some((workspace.path(), &task)),
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
        },
        Some(complete),
        Some(&allowance),
    )
    .await;
    assert_eq!(run.notes_bytes.as_deref(), Some("repaired\n"));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("attempts")).unwrap(),
        "checkcheck"
    );
    assert_eq!(run.bodies.len(), 5);
    assert_eq!(
        allowance.remaining(),
        7 - auxiliary.received_requests().await.unwrap().len() as u32
    );
    assert_eq!(run.reason, "completed");
    let admissions: Vec<_> = run
        .signals
        .iter()
        .filter_map(|signal| match signal {
            observability::BehaviorSignal::Recovery {
                decision,
                retries_used,
                verification_used,
                ..
            } if decision == "admitted" => Some((*retries_used, *verification_used)),
            _ => None,
        })
        .collect();
    assert_eq!(admissions, [(1, 1)]);
    if level.requires_verification() {
        assert!(run.signals.iter().any(|signal| matches!(signal,
            observability::BehaviorSignal::Verification { report, decision, .. }
            if decision == "accept" && report.checks.iter().any(|check| check.status == self_verify::CheckStatus::Passed))));
    }
}

macro_rules! cases {
    ($module:ident, $wire:literal, $smart:literal) => {
        mod $module {
            use super::*;
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_standalone_failed_check_repaired() {
                repaired($wire, $smart, crate::Tenacity::Grit).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_resolute_failed_check_repaired() {
                repaired($wire, $smart, crate::Tenacity::Resolute).await;
            }
        }
    };
}
cases!(chat, "openai", false);
cases!(chat_smart, "openai", true);
cases!(responses, "responses", false);
cases!(responses_smart, "responses", true);
cases!(anthropic, "anthropic", false);
cases!(anthropic_smart, "anthropic", true);
cases!(ollama, "ollama", false);
cases!(ollama_smart, "ollama", true);
