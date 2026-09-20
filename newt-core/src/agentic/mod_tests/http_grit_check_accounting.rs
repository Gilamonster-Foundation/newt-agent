//! #2449: executed failed checks keep causal accounting without strict completion.
use super::*;
use crate::agentic::turn_admission::CorrectionCause;

#[derive(Clone, Copy)]
enum Case {
    Explain,
    Zero,
    FinalRound,
    Acknowledged,
}

async fn observed_check(wire: &str, smart: bool, case: Case) {
    let _tenacity = crate::tenacity::scoped_tenacity_settings(
        crate::Tenacity::Grit,
        crate::tenacity::TenacityBudgets {
            grit_retries: if matches!(case, Case::Zero) { 0 } else { 2 },
        },
    );
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("notes.txt"), "operator content\n").unwrap();
    const CHECK: &str = "sh -c 'printf checked > check-marker; exit 1'";
    let task = instruction(CHECK);
    let script = match case {
        Case::Explain => vec![Step::Run(CHECK), Step::Done, Step::Done],
        Case::Zero => vec![Step::Run(CHECK), Step::Done],
        Case::FinalRound => vec![Step::Run(CHECK)],
        Case::Acknowledged => vec![
            Step::Run(CHECK),
            Step::Done,
            Step::Tool("read_file", r#"{"path":"absent-a.txt"}"#),
            Step::Done,
            Step::Tool("read_file", r#"{"path":"absent-b.txt"}"#),
            Step::Done,
        ],
    };
    let allowance = run_allowance::RunAllowance::new(24);
    let (auxiliary, complete) = actual_auxiliary().await;
    let run = run_turn_with_auxiliary(
        Turn {
            wire,
            smart,
            outcomes: false,
            check: CHECK,
            script: &script,
            cancel: None,
            max_tool_rounds: if matches!(case, Case::FinalRound) {
                1
            } else {
                12
            },
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
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("check-marker")).unwrap(),
        "checked"
    );
    assert_eq!(run.notes_bytes.as_deref(), Some("operator content\n"));
    assert!(!workspace.path().join("absent-a.txt").exists());
    assert!(!workspace.path().join("absent-b.txt").exists());
    let requests = match case {
        Case::Explain => 3,
        Case::Zero => 2,
        Case::FinalRound => 1,
        Case::Acknowledged => 6,
    };
    assert_eq!(run.bodies.len(), requests);
    assert_eq!(
        allowance.remaining(),
        24 - requests as u32 - auxiliary.received_requests().await.unwrap().len() as u32
    );
    let admissions: Vec<_> = run
        .signals
        .iter()
        .filter_map(|s| match s {
            observability::BehaviorSignal::Recovery {
                decision,
                cause,
                retries_used,
                verification_used,
                ..
            } if decision == "admitted" => Some((*cause, *retries_used, *verification_used)),
            _ => None,
        })
        .collect();
    match case {
        Case::Explain => {
            assert_eq!(
                run.reason, "completed",
                "Grit permits assessing expected nonzero without a fresh pass"
            );
            assert_eq!(admissions,[(CorrectionCause::FailedCheck,1,1)],"an actually failed check pays both caps even when optional verification is disabled");
        }
        Case::Zero | Case::FinalRound => {
            assert_eq!(
                run.reason, "repair_exhausted",
                "failed-check exhaustion must retain its existing meaning"
            );
            assert!(admissions.is_empty());
            assert!(
                run.answer.contains("incomplete"),
                "the stopped answer must qualify completion"
            );
            assert!(run.signals.iter().any(|s| matches!(s,
                observability::BehaviorSignal::Recovery {cause:CorrectionCause::FailedCheck,decision,retries_used:0,verification_used:0,..} if decision == if matches!(case, Case::FinalRound) { "round_allowance" } else { "grit_allowance" })));
            assert!(run.signals.iter().any(|s| matches!(s,
                observability::BehaviorSignal::Verification {report,..} if report.checks.iter().any(|check| check.status==self_verify::CheckStatus::Failed))));
            if matches!(case, Case::FinalRound) {
                let reported: Vec<_> = run
                    .signals
                    .iter()
                    .filter_map(|s| match s {
                        observability::BehaviorSignal::Verification { repairs_used, .. } => {
                            Some(*repairs_used)
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    reported,
                    [0],
                    "a cap refuses correction without inventing three spent verification units"
                );
            }
        }
        Case::Acknowledged => {
            assert_eq!(
                run.reason, "failed",
                "an old acknowledged check cannot relabel a new unrelated failure"
            );
            assert_eq!(
                admissions,
                [
                    (CorrectionCause::FailedCheck, 1, 1),
                    (CorrectionCause::ToolFailure, 2, 1)
                ]
            );
            assert!(run.signals.iter().any(|s| matches!(s,
                observability::BehaviorSignal::Recovery {cause:CorrectionCause::ToolFailure,decision,retries_used:2,verification_used:1,..} if decision=="grit_allowance")));
        }
    }
}

macro_rules! cases {
    ($module:ident,$wire:literal,$smart:literal) => {
        mod $module {
            use super::*;
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_check_failure_expected_nonzero_pays_both_caps() {
                observed_check($wire, $smart, Case::Explain).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_check_failure_zero_is_repair_exhausted() {
                observed_check($wire, $smart, Case::Zero).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_check_failure_final_round_retains_check_cause() {
                observed_check($wire, $smart, Case::FinalRound).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_check_failure_acknowledged_does_not_relabel_tool_failure() {
                observed_check($wire, $smart, Case::Acknowledged).await;
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
