//! #2449: only typed recovery exhaustion changes the Failed classification.
use super::*;
use newt_core::agentic::turn_admission::CorrectionCause;

fn recovery(cause: CorrectionCause, decision: &str) -> newt_core::BehaviorSignal {
    newt_core::BehaviorSignal::Recovery {
        round: 1,
        cause,
        decision: decision.into(),
        retries_used: 0,
        allowance: 0,
        verification_used: 0,
    }
}

#[test]
fn grit_2449_terminal_requires_actual_recovery_refusal_and_clean_attempt() {
    let refused = [recovery(CorrectionCause::ToolFailure, "grit_allowance")];
    let genuine = terminal_with_recovery(true, None, Some(TurnEndReason::Failed), false, &refused);
    assert_eq!(genuine, Terminal::StoppedShort(TurnEndReason::Failed));
    assert_eq!(outcome_label(genuine), "completed");
    assert_eq!(status_label(genuine), "incomplete");
    for (clean, class, evidence) in [
        (false, Some(ErrorClass::Transport), refused.as_slice()),
        (false, Some(ErrorClass::Harness), refused.as_slice()),
        (true, Some(ErrorClass::Harness), refused.as_slice()),
        (true, None, &[]),
    ] {
        assert!(matches!(
            terminal_with_recovery(clean, class, Some(TurnEndReason::Failed), false, evidence),
            Terminal::Failed(_)
        ));
    }
    for signal in [
        recovery(CorrectionCause::ToolFailure, "admitted"),
        recovery(CorrectionCause::FailedCheck, "grit_allowance"),
    ] {
        assert!(matches!(
            terminal_with_recovery(true, None, Some(TurnEndReason::Failed), false, &[signal]),
            Terminal::Failed(_)
        ));
    }
}
