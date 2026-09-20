//! Standalone Grit, all wires/Smart, with optional verification disabled.
use super::*;

#[derive(Clone, Copy)]
enum Case {
    NoReplay,
    Repair,
    Denied,
    ReadText,
    Transport,
    CumulativeBatch,
}

async fn standalone(wire: &str, smart: bool, case: Case) {
    let _tenacity = crate::tenacity::scoped_tenacity_settings(
        crate::Tenacity::Grit,
        crate::tenacity::TenacityBudgets::default(),
    );
    let workspace = tempfile::tempdir().unwrap();
    let original = if matches!(case, Case::ReadText) {
        "error: operator document\n"
    } else {
        "original\n"
    };
    std::fs::write(workspace.path().join("notes.txt"), original).unwrap();
    let mut caveats = Caveats::top();
    caveats.fs_write = crate::caveats::Scope::Only(
        [workspace.path().to_string_lossy().into_owned()]
            .into_iter()
            .collect(),
    );
    if matches!(case, Case::Denied) {
        caveats.fs_read = crate::caveats::Scope::none();
    }
    const APPEND: &str = "sh -c 'printf once >> attempts; exit 1'";
    let script = match case {
        Case::NoReplay => vec![Step::Run(APPEND), Step::Done, Step::Done],
        Case::CumulativeBatch => vec![
            Step::Tools(&[
                (
                    "run_command",
                    r#"{"command":"sh -c 'printf a >> attempts; exit 1'"}"#,
                ),
                (
                    "run_command",
                    r#"{"command":"sh -c 'printf b >> attempts; exit 1'"}"#,
                ),
            ]),
            Step::Done,
            Step::Tool("read_file", r#"{"path":"notes.txt"}"#),
            Step::Run("sh -c 'printf c >> attempts; exit 1'"),
            Step::Done,
            Step::Tool("read_file", r#"{"path":"notes.txt"}"#),
            Step::Run("sh -c 'printf d >> attempts; exit 1'"),
            Step::Done,
        ],
        Case::Transport => vec![
            Step::Run(APPEND),
            Step::Done,
            Step::HttpError(503),
            Step::Done,
        ],
        Case::Repair => vec![
            Step::Tool(
                "edit_file",
                r#"{"path":"notes.txt","old_string":"missing","new_string":"never applied"}"#,
            ),
            Step::Done,
            Step::Tool(
                "edit_file",
                r#"{"path":"notes.txt","old_string":"original","new_string":"repaired"}"#,
            ),
            Step::Done,
        ],
        Case::Denied | Case::ReadText => vec![
            Step::Tool("read_file", r#"{"path":"notes.txt"}"#),
            Step::Done,
        ],
    };
    let allowance = run_allowance::RunAllowance::new(32);
    let (auxiliary, complete) = actual_auxiliary().await;
    let run = run_turn_with_auxiliary(
        Turn {
            wire,
            smart,
            outcomes: false,
            check: "",
            script: &script,
            cancel: None,
            max_tool_rounds: 16,
            caveats,
            env: &[
                ("NEWT_SELF_VERIFY", "0"),
                ("NEWT_SHELL_ENGINE", "safe-subset"),
                ("NEWT_HTTP_MAX_RETRIES", "1"),
                ("NEWT_HTTP_BACKOFF_BASE_MS", "0"),
                ("NEWT_HTTP_BACKOFF_MAX_MS", "0"),
            ],
            workspace_task: Some((
                workspace.path(),
                "Carry out the requested operation and assess the observed result.",
            )),
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
        run.notes_bytes.as_deref(),
        Some(if matches!(case, Case::Repair) {
            "repaired\n"
        } else {
            original
        })
    );
    if matches!(case, Case::NoReplay | Case::Transport) {
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("attempts")).unwrap(),
            "once",
            "the harness never automatically replays the non-idempotent failed operation"
        );
    }
    if matches!(case, Case::CumulativeBatch) {
        let mut attempts = std::fs::read(workspace.path().join("attempts")).unwrap();
        attempts.sort_unstable();
        assert_eq!(
            attempts, b"abcd",
            "both batch failures and each later failure executed once"
        );
        assert!(
            run.bodies[3].contains("original"),
            "first successful intervening read is returned"
        );
        assert!(
            run.bodies[6].contains("original"),
            "second successful intervening read is returned"
        );
    }
    let (requests, corrections) = match case {
        Case::NoReplay => (3, 1),
        Case::Repair | Case::Transport => (4, 1),
        Case::Denied | Case::ReadText => (2, 0),
        Case::CumulativeBatch => (8, 2),
    };
    assert_eq!(run.bodies.len(), requests);
    assert_eq!(
        allowance.remaining(),
        32 - requests as u32 - auxiliary.received_requests().await.unwrap().len() as u32,
        "every actual inference attempt, including transport retry, debits the same global allowance"
    );
    assert_eq!(
        run.reason,
        if matches!(case, Case::CumulativeBatch) {
            "failed"
        } else {
            "completed"
        },
        "Grit can complete an expected-failure assessment without imposing Resolute checks"
    );
    let admitted = run
        .signals
        .iter()
        .filter(|signal| {
            matches!(signal,
        observability::BehaviorSignal::Recovery { decision, .. } if decision == "admitted")
        })
        .count();
    assert_eq!(
        admitted, corrections,
        "transport retries do not duplicate correction admission"
    );
    assert!(!run.answer.contains("verification incomplete"));
    if matches!(case, Case::CumulativeBatch) {
        let used: Vec<_> = run
            .signals
            .iter()
            .filter_map(|signal| match signal {
                observability::BehaviorSignal::Recovery {
                    decision,
                    retries_used,
                    ..
                } if decision == "admitted" => Some(*retries_used),
                _ => None,
            })
            .collect();
        assert_eq!(
            used,
            [1, 2],
            "batch charges once; successful operations never refill the external-turn allowance"
        );
        assert!(run.signals.iter().any(|signal| matches!(signal,
            observability::BehaviorSignal::Recovery { decision, retries_used: 2, .. } if decision == "grit_allowance")));
    }
}

macro_rules! wire_cases {
    ($module:ident, $wire:literal, $smart:literal) => {
        mod $module {
            use super::*;
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_standalone_no_replay() {
                standalone($wire, $smart, Case::NoReplay).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_standalone_repair() {
                standalone($wire, $smart, Case::Repair).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_standalone_denied() {
                standalone($wire, $smart, Case::Denied).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_standalone_read_text() {
                standalone($wire, $smart, Case::ReadText).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_standalone_transport_retry() {
                standalone($wire, $smart, Case::Transport).await;
            }
            #[tokio::test]
            #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
            async fn grit_2449_standalone_batch_and_success_do_not_refill() {
                standalone($wire, $smart, Case::CumulativeBatch).await;
            }
        }
    };
}
wire_cases!(chat, "openai", false);
wire_cases!(chat_smart, "openai", true);
wire_cases!(responses, "responses", false);
wire_cases!(responses_smart, "responses", true);
wire_cases!(anthropic, "anthropic", false);
wire_cases!(anthropic_smart, "anthropic", true);
wire_cases!(ollama, "ollama", false);
wire_cases!(ollama_smart, "ollama", true);
