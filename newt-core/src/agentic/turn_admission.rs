//! One captured external turn's shared model-work admission.
//!
//! Host continuations retain this owner. A proposed correction is charged only
//! when actual model work reserves its global call; transport retries reuse the
//! committed correction. No tool is replayed here and no authority is granted.
use std::sync::{atomic::AtomicBool, Arc, Mutex};

use super::{observability::BehaviorSignal, run_allowance::RunAllowance};
use crate::tenacity::{Tenacity, TenacityBudgets, ToolRoundLimit};
use crate::{ExecOutcome, TurnEndReason};

/// Captured policy and derivation, reused by the existing settings receipt.
#[derive(Debug, Clone, Copy)]
pub struct TurnPolicy {
    pub tenacity: Tenacity,
    pub budgets: TenacityBudgets,
    pub rounds: ToolRoundLimit,
    pub grace_rounds: usize,
}

impl TurnPolicy {
    pub fn capture(rounds: ToolRoundLimit, grace_rounds: usize) -> Self {
        Self {
            tenacity: crate::tenacity::effective_tenacity(),
            budgets: crate::tenacity::effective_tenacity_budgets(),
            rounds,
            grace_rounds,
        }
    }
}

/// Why the harness is considering another model continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionCause {
    ToolFailure,
    FailedCheck,
    Verification,
    ImportRepair,
}

impl CorrectionCause {
    fn spends_grit(self) -> bool {
        self != Self::Verification
    }
    fn spends_verification(self) -> bool {
        self != Self::ToolFailure
    }
    fn ending(self) -> TurnEndReason {
        match self {
            Self::ToolFailure => TurnEndReason::Failed,
            Self::FailedCheck | Self::ImportRepair => TurnEndReason::RepairExhausted,
            Self::Verification => TurnEndReason::VerificationIncomplete,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingCorrection {
    cause: CorrectionCause,
    failures: u64,
    grit: bool,
    verification: bool,
}

#[derive(Debug, Default)]
struct Progress {
    grit_used: u32,
    verification_used: usize,
    rounds_used: usize,
    round_pending: bool,
    grace_open: bool,
    failures: u64,
    failed_check_epoch: u64,
    acknowledged: u64,
    pending: Option<PendingCorrection>,
    signals: Vec<BehaviorSignal>,
}

/// Transient ownership, never a new persisted store or generated identity.
#[derive(Debug)]
pub struct TurnAdmission {
    pub policy: TurnPolicy,
    run: Option<RunAllowance>,
    verification: super::self_verify::VerificationSettings,
    history: Mutex<Option<super::self_verify::VerificationHistory>>,
    progress: Mutex<Progress>,
}

impl TurnAdmission {
    pub fn new(policy: TurnPolicy, run: Option<RunAllowance>) -> Arc<Self> {
        Arc::new(Self {
            policy,
            run,
            verification: super::self_verify::VerificationSettings::capture(),
            history: Mutex::new(None),
            progress: Mutex::new(Progress::default()),
        })
    }

    pub(crate) fn for_loop(
        retained: Option<Arc<Self>>,
        configured: usize,
        grace_rounds: usize,
        run: Option<&RunAllowance>,
    ) -> Option<Arc<Self>> {
        if let Some(retained) = retained {
            return retained.enabled().then_some(retained);
        }
        if !crate::tenacity::effective_tenacity().recovers_failures() {
            return None;
        }
        let rounds = crate::tenacity::resolve_tool_round_limit(configured, None, None);
        Some(Self::new(
            TurnPolicy::capture(rounds, grace_rounds),
            run.cloned(),
        ))
    }

    fn progress(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn verification_history(&self) -> Option<super::self_verify::VerificationHistory> {
        self.history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn retain_verification_history(
        &self,
        history: super::self_verify::VerificationHistory,
    ) {
        *self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(history);
    }

    /// Reinstall the original external turn's pursuit/settings for a derived
    /// host continuation. Dropping the scope restores the next operator turn.
    pub fn bind_policy(&self) -> TurnPolicyBinding {
        TurnPolicyBinding {
            _tenacity: crate::tenacity::scoped_tenacity_settings(
                self.policy.tenacity,
                self.policy.budgets,
            ),
            _verification: super::self_verify::scoped_verification_settings(self.verification),
        }
    }

    pub fn enabled(&self) -> bool {
        self.policy.tenacity.recovers_failures()
    }

    pub fn rounds_used(&self) -> usize {
        self.progress().rounds_used
    }

    /// Remaining original cap and original grace, never refreshed on re-entry.
    pub fn remaining_rounds(&self) -> (usize, usize) {
        let s = self.progress();
        let base = self.policy.rounds.rounds.saturating_sub(s.rounds_used);
        let hard = self
            .policy
            .rounds
            .rounds
            .saturating_add(self.policy.grace_rounds)
            .saturating_sub(s.rounds_used);
        if s.grace_open {
            (hard, 0)
        } else {
            (base, hard.saturating_sub(base))
        }
    }

    pub(crate) fn begin_round(&self) {
        self.progress().round_pending = true;
    }
    pub(crate) fn open_grace(&self) {
        self.progress().grace_open = true;
    }
    pub(crate) fn verification_used(&self) -> usize {
        self.progress().verification_used
    }

    /// The producer supplies an actual execution fact, never rendered text.
    pub(crate) fn observe(&self, outcome: Option<ExecOutcome>, failed_check: bool) {
        if matches!(outcome, Some(ExecOutcome::Failed | ExecOutcome::TimedOut)) {
            let mut s = self.progress();
            s.failures = s.failures.saturating_add(1);
            if failed_check {
                s.failed_check_epoch = s.failures;
            }
        }
    }

    pub(crate) fn has_unacknowledged_failure(&self) -> bool {
        let s = self.progress();
        s.failures > s.acknowledged
    }

    pub(crate) fn has_unacknowledged_failed_check(&self) -> bool {
        let s = self.progress();
        s.failed_check_epoch > s.acknowledged
    }

    fn signal(&self, s: &mut Progress, cause: CorrectionCause, decision: &str) {
        s.signals.push(BehaviorSignal::Recovery {
            round: s.rounds_used.saturating_sub(1),
            cause,
            decision: decision.to_string(),
            retries_used: s.grit_used,
            allowance: self.policy.budgets.grit_retries,
            verification_used: s.verification_used,
        });
    }

    /// Queue a correction without claiming model work that has not been admitted.
    pub fn propose(
        &self,
        cause: CorrectionCause,
        rounds_left: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<(), TurnEndReason> {
        if super::is_cancelled(cancel) {
            return Err(TurnEndReason::Cancelled);
        }
        let mut s = self.progress();
        let refusal = if !rounds_left {
            Some("round_allowance")
        } else if cause.spends_grit() && s.grit_used >= self.policy.budgets.grit_retries {
            Some("grit_allowance")
        } else if cause.spends_verification()
            && s.verification_used >= super::self_verify::VERIFY_REPAIR_ALLOWANCE
        {
            Some("verification_allowance")
        } else if self.run.as_ref().is_some_and(|run| run.remaining() == 0) {
            Some("run_allowance")
        } else {
            None
        };
        if let Some(refusal) = refusal {
            s.pending = None;
            self.signal(&mut s, cause, refusal);
            return Err(cause.ending());
        }
        // Re-entry may inspect the same pending continuation; it never buys a
        // second admission or loses the original failure batch.
        let previous = s.pending;
        let effective_cause = previous.map_or(cause, |old| match (old.cause, cause) {
            (CorrectionCause::FailedCheck, _) | (_, CorrectionCause::FailedCheck) => {
                CorrectionCause::FailedCheck
            }
            (CorrectionCause::ImportRepair, _) | (_, CorrectionCause::ImportRepair) => {
                CorrectionCause::ImportRepair
            }
            (CorrectionCause::ToolFailure, _) | (_, CorrectionCause::ToolFailure) => {
                CorrectionCause::ToolFailure
            }
            _ => CorrectionCause::Verification,
        });
        s.pending = Some(PendingCorrection {
            cause: effective_cause,
            failures: s.failures,
            grit: cause.spends_grit() || previous.is_some_and(|old| old.grit),
            verification: cause.spends_verification()
                || previous.is_some_and(|old| old.verification),
        });
        Ok(())
    }

    /// Called at every actual primary/auxiliary attempt, after local validation.
    /// The shared atomic run reservation is authoritative; remaining() above is
    /// only an early refusal hint. No global unit or correction is refunded.
    pub fn reserve_model(&self, cancel: Option<&AtomicBool>) -> anyhow::Result<()> {
        anyhow::ensure!(!super::is_cancelled(cancel), "model admission cancelled");
        let mut s = self.progress();
        if s.round_pending {
            let limit = self.policy.rounds.rounds.saturating_add(if s.grace_open {
                self.policy.grace_rounds
            } else {
                0
            });
            anyhow::ensure!(
                s.rounds_used < limit,
                "external turn round allowance exhausted"
            );
        }
        if let Some(run) = &self.run {
            run.try_reserve()?;
        }
        if s.round_pending {
            s.rounds_used = s.rounds_used.saturating_add(1);
            s.round_pending = false;
        }
        if let Some(pending) = s.pending.take() {
            if pending.grit {
                s.grit_used += 1;
            }
            if pending.verification {
                s.verification_used += 1;
            }
            s.acknowledged = s.acknowledged.max(pending.failures);
            self.signal(&mut s, pending.cause, "admitted");
        }
        Ok(())
    }

    pub(crate) fn drain_signals(&self) -> Vec<BehaviorSignal> {
        std::mem::take(&mut self.progress().signals)
    }

    /// Immutable projection for effective-config identity. Runtime counters are
    /// deliberately separate: executing another round cannot change policy ID.
    pub fn policy_receipt(&self) -> serde_json::Value {
        serde_json::json!({
            "tenacity": self.policy.tenacity,
            "grit_retries": self.policy.budgets.grit_retries,
            "tool_round_limit": self.policy.rounds,
            "workflow_grace_rounds": self.policy.grace_rounds,
        })
    }

    /// Execution accounting only; never input to effective-config identity.
    pub fn execution_receipt(&self) -> serde_json::Value {
        let s = self.progress();
        serde_json::json!({
            "tenacity": self.policy.tenacity,
            "grit_retries": self.policy.budgets.grit_retries,
            "grit_used": s.grit_used,
            "verification_used": s.verification_used,
            "tool_round_limit": self.policy.rounds,
            "workflow_grace_rounds": self.policy.grace_rounds,
            "rounds_used": s.rounds_used,
        })
    }
}

/// Current-thread binding, with the same thread affinity as psyche capture.
#[must_use]
pub struct TurnPolicyBinding {
    _tenacity: crate::tenacity::ScopedEffectiveTenacity,
    _verification: super::self_verify::ScopedVerificationSettings,
}
