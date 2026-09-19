//! #2451: strict task completion must run on every wire, with either harness.
use super::*;

async fn strict_cases(wire: &str, smart: bool) {
    // The old-path red used the already-existing cumulative Relentless level.
    // Exercise the new selector itself here; terminal regressions below retain
    // Relentless coverage through the same captured policy.
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Resolute);
    let no_exec = Caveats {
        exec: crate::caveats::Scope::none(),
        ..Caveats::top()
    };
    let cases: &[(&str, &str, &[Step], &str, Caveats)] = &[
        (
            "unexecuted",
            PASSING_CHECK,
            &[Step::RejectedBatch(PASSING_CHECK)],
            "verification_incomplete",
            Caveats::top(),
        ),
        (
            "never run",
            PASSING_CHECK,
            &[],
            "verification_incomplete",
            Caveats::top(),
        ),
        (
            "failed",
            FAILING_CHECK,
            &[Step::Run(FAILING_CHECK)],
            "repair_exhausted",
            Caveats::top(),
        ),
        (
            "timed out",
            "sleep 5",
            &[Step::Run("sleep 5")],
            "repair_exhausted",
            Caveats::top(),
        ),
        (
            "no checks",
            "",
            &[],
            "verification_incomplete",
            Caveats::top(),
        ),
        (
            "fresh pass",
            PASSING_CHECK,
            &[Step::Run(PASSING_CHECK)],
            "completed",
            Caveats::top(),
        ),
        (
            "read after pass",
            PASSING_CHECK,
            &[Step::Run(PASSING_CHECK), Step::Run("ls")],
            "completed",
            Caveats::top(),
        ),
        (
            "denied",
            PASSING_CHECK,
            &[Step::Run(PASSING_CHECK)],
            "verification_incomplete",
            no_exec,
        ),
        (
            "unavailable",
            "newt-absent-check-2451 --verify",
            &[Step::Run("newt-absent-check-2451 --verify")],
            "verification_incomplete",
            Caveats::top(),
        ),
        (
            "masked",
            PASSING_CHECK,
            &[Step::Run("sh -c 'exit 0' | cat")],
            "verification_incomplete",
            Caveats::top(),
        ),
        (
            "stale",
            PASSING_CHECK,
            &[
                Step::Run(PASSING_CHECK),
                Step::Run("sh -c 'echo changed > notes.txt'"),
            ],
            "verification_incomplete",
            Caveats::top(),
        ),
    ];
    for (name, check, script, expected, caveats) in cases {
        let run = run_turn_configured(
            Turn {
                wire,
                smart,
                outcomes: false,
                check,
                script,
                cancel: None,
                max_tool_rounds: 8,
                caveats: caveats.clone(),
                env: if *name == "timed out" {
                    &[
                        ("NEWT_SELF_VERIFY", "0"),
                        ("NEWT_DISABLE_OCAP", "1"),
                        ("NEWT_HOST_EXEC_TIMEOUT_SECS", "1"),
                    ]
                } else {
                    &[
                        ("NEWT_SELF_VERIFY", "0"),
                        ("NEWT_SHELL_ENGINE", "safe-subset"),
                    ]
                },
                workspace_task: None,
            },
            |ctx| {
                ctx.action_nudges = false;
                ctx.prompt_disposition = PromptDisposition::Act;
            },
        )
        .await;
        if *name == "stale" {
            assert_eq!(
                run.notes_bytes.as_deref(),
                Some("changed\n"),
                "fixture mutated its own workspace"
            );
        }
        assert_eq!(run.reason, *expected, "{wire} smart={smart}: {name}");
        let expected_status = match *name {
            "unexecuted" => Some(self_verify::CheckStatus::Unexecuted),
            "stale" => Some(self_verify::CheckStatus::Stale),
            "masked" => Some(self_verify::CheckStatus::Unverified),
            "denied" => Some(self_verify::CheckStatus::Denied),
            "unavailable" => Some(self_verify::CheckStatus::Unavailable),
            "failed" => Some(self_verify::CheckStatus::Failed),
            "timed out" => Some(self_verify::CheckStatus::TimedOut),
            "never run" => Some(self_verify::CheckStatus::NeverRun),
            "fresh pass" | "read after pass" => Some(self_verify::CheckStatus::Passed),
            _ => None,
        };
        if let Some(expected_status) = expected_status {
            let report = run
                .signals
                .iter()
                .rev()
                .find_map(|signal| match signal {
                    observability::BehaviorSignal::Verification { report, .. } => Some(report),
                    _ => None,
                })
                .unwrap();
            assert!(
                report
                    .checks
                    .iter()
                    .any(|check| check.status == expected_status),
                "{name}: {report:?}"
            );
        }
        if *expected != "completed" {
            assert!(
                run.answer.contains("verification"),
                "{name}: harness must identify unverified completion: {}",
                run.answer
            );
        }
        assert!(
            run.signals
                .iter()
                .any(|signal| matches!(signal, observability::BehaviorSignal::Verification { .. })),
            "{name}: policy must emit its decision"
        );
    }
    // These controls use the same scripted premature answer and disabled
    // advisory switches. Only the captured typed policy changes.
    for (level, disposition) in [
        (crate::Tenacity::Normal, PromptDisposition::Act),
        (crate::Tenacity::Relentless, PromptDisposition::Research),
        (crate::Tenacity::Relentless, PromptDisposition::Explain),
    ] {
        let _control = crate::tenacity::scoped_effective_tenacity(level);
        let run = run_turn_configured(
            Turn {
                wire,
                smart,
                outcomes: false,
                check: PASSING_CHECK,
                script: &[],
                cancel: None,
                max_tool_rounds: 8,
                caveats: Caveats::top(),
                env: &[("NEWT_SELF_VERIFY", "0")],
                workspace_task: None,
            },
            |ctx| {
                ctx.action_nudges = false;
                ctx.prompt_disposition = disposition;
            },
        )
        .await;
        assert!(
            run.reason.is_null() || run.reason == "completed",
            "{wire} smart={smart}: {level:?}/{disposition:?}: {}",
            run.reason
        );
        assert!(!run
            .signals
            .iter()
            .any(|signal| matches!(signal, observability::BehaviorSignal::Verification { .. })));
    }
}

macro_rules! wire_case {
    ($name:ident, $wire:literal, $smart:literal) => {
        /// #2451: an Act completion requires a fresh observed check, even when
        /// both advisory switches are off. Grounds the pure policy in real loops.
        #[tokio::test]
        #[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
        async fn $name() {
            strict_cases($wire, $smart).await;
        }
    };
}
wire_case!(resolute_2451_openai, "openai", false);
wire_case!(resolute_2451_anthropic, "anthropic", false);
wire_case!(resolute_2451_ollama, "ollama", false);
wire_case!(resolute_2451_responses, "responses", false);
wire_case!(resolute_2451_smart_openai, "openai", true);
wire_case!(resolute_2451_smart_anthropic, "anthropic", true);
wire_case!(resolute_2451_smart_ollama, "ollama", true);
wire_case!(resolute_2451_smart_responses, "responses", true);

/// #2451: provider refusal is terminal; strict pursuit may not call it success
/// or spend another inference trying to overcome the provider's decline.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn resolute_2451_provider_refusal_is_incomplete_without_retry() {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Relentless);
    for wire in ["anthropic", "responses"] {
        for smart in [false, true] {
            let run = run_turn_configured(
                Turn {
                    wire,
                    smart,
                    outcomes: false,
                    check: PASSING_CHECK,
                    script: &[Step::Refusal],
                    cancel: None,
                    max_tool_rounds: 8,
                    caveats: Caveats::top(),
                    env: &[("NEWT_SELF_VERIFY", "0")],
                    workspace_task: None,
                },
                |ctx| {
                    ctx.action_nudges = false;
                    ctx.prompt_disposition = PromptDisposition::Act;
                },
            )
            .await;
            assert_eq!(
                run.reason, "verification_incomplete",
                "{wire} smart={smart}"
            );
            assert!(run.answer.contains("verification"));
            assert!(
                run.answer.contains("decline"),
                "{wire} smart={smart}: {:?}",
                run.answer
            );
            assert_eq!(run.bodies.len(), 1, "never retry a refusal");
        }
    }
}

/// #2451: a tools-disabled cap summary can say done; the harness must visibly
/// qualify it and preserve the configured cap instead of silently extending it.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn resolute_2451_cap_summary_is_visibly_incomplete() {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Relentless);
    for wire in ["openai", "anthropic", "ollama", "responses"] {
        for smart in [false, true] {
            let run = run_turn_configured(
                Turn {
                    wire,
                    smart,
                    outcomes: false,
                    check: PASSING_CHECK,
                    script: &[Step::Run("ls")],
                    cancel: None,
                    max_tool_rounds: 1,
                    caveats: Caveats::top(),
                    env: &[("NEWT_SELF_VERIFY", "0")],
                    workspace_task: None,
                },
                |ctx| {
                    ctx.action_nudges = false;
                    ctx.prompt_disposition = PromptDisposition::Act;
                },
            )
            .await;
            assert!(
                run.at_cap,
                "{wire} smart={smart}: respected the explicit cap"
            );
            assert!(
                run.answer.contains("verification"),
                "{wire} smart={smart}: {}",
                run.answer
            );
            assert_eq!(
                run.reason, "verification_incomplete",
                "{wire} smart={smart}"
            );
        }
    }
}

/// #2451: ground freshness in a byte change in the exact workspace being hashed.
#[tokio::test]
#[serial_test::serial(anthropic_loop_env, newt_self_verify_env)]
async fn resolute_2451_stale_workspace_write() {
    let _tenacity = crate::tenacity::scoped_effective_tenacity(crate::Tenacity::Relentless);
    let run = run_turn_configured(
        Turn {
            wire: "openai",
            smart: false,
            outcomes: false,
            check: PASSING_CHECK,
            script: &[
                Step::Run(PASSING_CHECK),
                Step::Run("sh -c 'echo changed > notes.txt'"),
            ],
            cancel: None,
            max_tool_rounds: 8,
            caveats: Caveats::top(),
            env: &[
                ("NEWT_SELF_VERIFY", "0"),
                ("NEWT_SHELL_ENGINE", "safe-subset"),
            ],
            workspace_task: None,
        },
        |ctx| {
            ctx.action_nudges = false;
            ctx.prompt_disposition = PromptDisposition::Act;
        },
    )
    .await;
    assert_eq!(run.notes_bytes.as_deref(), Some("changed\n"));
    assert_eq!(run.reason, "verification_incomplete");
}
