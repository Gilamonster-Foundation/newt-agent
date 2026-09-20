//! Post-revert queue boundary, preserving the existing profile retry policy.
use super::*;

/// Preserve the profile cap and ledger-only rollback while proposing shared
/// recovery before queueing. The next actual model admission spends the unit.
pub(super) fn queue(
    budget: &mut u32,
    maximum: u32,
    parent: Option<newt_core::TurnPromptContext>,
    corrective: String,
    owner: Option<&newt_core::agentic::turn_admission::TurnAdmission>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> (Option<PendingRetry>, String) {
    match retry_step(*budget) {
        RetryStep::Reprompt => {
            let Some(parent) = parent else {
                return (None, "\n✗ retry: corrective input was not queued because the turn has no prompt receipt".to_string());
            };
            if let Some(owner) = owner.filter(|owner| owner.enabled()) {
                if let Err(reason) = owner.propose(
                    newt_core::agentic::turn_admission::CorrectionCause::ImportRepair,
                    owner.remaining_rounds().0 > 0,
                    cancel,
                ) {
                    let why = if reason == newt_core::TurnEndReason::Cancelled {
                        "the turn was cancelled"
                    } else {
                        "the captured recovery or model-work allowance is exhausted"
                    };
                    return (
                        None,
                        format!("\n✗ retry: stopped because {why}; file(s) left reverted"),
                    );
                }
            }
            *budget -= 1;
            (Some(PendingRetry { text: corrective, parent: Box::new(parent) }),
                format!("\n↻ retry: re-prompting the model to ground the rewrite ({budget} re-prompt(s) remaining)"))
        }
        RetryStep::GiveUp => (
            None,
            format!("\n✗ retry: gave up after {maximum} re-prompt(s) — file(s) left reverted"),
        ),
    }
}
