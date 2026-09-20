//! An operator-configured, whole-run inference call budget (#2313).
//!
//! Scoped to a call count, not tokens: a call-count reservation is exact by
//! construction (one reservation per attempt, no reconciliation gap), where a
//! token budget would need to reconcile an estimate against the usage the
//! server later reports. That reconciliation is real future work, tracked as
//! out of scope for this first increment — see the landing PR body.
//!
//! Checked once, at [`super::attempt_capture::send`], the same single point
//! that already keys every attempt from the exact wire bytes. `None`
//! (every existing caller today) is bit-for-bit unchanged behavior.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// A budget of remaining inference calls for one run, shared across every
/// wire's dispatch through [`super::attempt_capture::AttemptScope`].
#[derive(Debug, Clone)]
pub struct RunAllowance {
    remaining: Arc<AtomicU32>,
}

/// The run allowance was already spent when a new dispatch was attempted.
/// The request was never sent — this is refused before any wire bytes go
/// out, not after a failed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the run allowance is exhausted: no calls remain")]
pub struct RunAllowanceExhausted;

impl RunAllowance {
    /// A budget of `calls` remaining inference calls.
    pub fn new(calls: u32) -> Self {
        Self {
            remaining: Arc::new(AtomicU32::new(calls)),
        }
    }

    /// How many calls remain right now. For tests and reporting; never the
    /// basis for a second, racing reservation decision.
    pub fn remaining(&self) -> u32 {
        self.remaining.load(Ordering::Relaxed)
    }

    /// Reserve one call, or refuse if none remain. Atomic: two callers
    /// racing this on a budget of one see exactly one success.
    ///
    /// Public (#2313 b3/b4): a helper-model call (summarizer) or a spawned
    /// crew member is a real inference call against the same run, but it
    /// dispatches through its own HTTP client outside
    /// [`super::attempt_capture::send`]'s wire-byte-keyed ledger recording.
    /// Exposing the bare reservation primitive lets those callers draw down
    /// the same shared budget without needing the ledger machinery a primary
    /// attempt's audit trail requires.
    pub fn try_reserve(&self) -> Result<(), RunAllowanceExhausted> {
        self.remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .map(|_| ())
            .map_err(|_| RunAllowanceExhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_allowance_reserves_up_to_its_count_then_refuses() {
        let allowance = RunAllowance::new(2);
        assert!(allowance.try_reserve().is_ok());
        assert_eq!(allowance.remaining(), 1);
        assert!(allowance.try_reserve().is_ok());
        assert_eq!(allowance.remaining(), 0);
        assert_eq!(allowance.try_reserve(), Err(RunAllowanceExhausted));
        assert_eq!(
            allowance.remaining(),
            0,
            "a refused reservation spends nothing"
        );
    }

    #[test]
    fn a_zero_allowance_refuses_the_first_call() {
        let allowance = RunAllowance::new(0);
        assert_eq!(allowance.try_reserve(), Err(RunAllowanceExhausted));
    }

    #[test]
    fn two_racing_reservations_on_a_budget_of_one_give_exactly_one_success() {
        // Deterministic stand-in for concurrency: fetch_update is the atomic
        // primitive that would make this true under real concurrent access
        // too, since it retries on a concurrent modification rather than
        // reading remaining() and deciding non-atomically.
        let allowance = RunAllowance::new(1);
        let first = allowance.try_reserve();
        let second = allowance.try_reserve();
        assert_eq!([first, second].iter().filter(|r| r.is_ok()).count(), 1);
    }
}
