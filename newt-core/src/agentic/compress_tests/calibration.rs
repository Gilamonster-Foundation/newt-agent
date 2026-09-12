use super::test_support::{run, tool_heavy, EST};
use super::*;
use crate::agentic::trim::PromptTracker;
use crate::agentic::{calibrate_down, calibrate_up};

#[test]
fn reported_one_and_a_half_ratio_shrinks_the_next_budget() {
    let mut state = CompressState::new();
    let prior_budget = calibrate_down(9_000, state.calibration.ratio(None));
    state.calibration.observe(Some(6_000), 4_000);
    let next_budget = calibrate_down(9_000, state.calibration.ratio(None));
    assert_eq!(prior_budget, 9_000);
    assert_eq!(
        next_budget, 6_000,
        "server usage must tighten the next request"
    );
    assert_eq!(
        calibrate_up(next_budget, state.calibration.ratio(None)),
        9_000
    );
}

#[test]
fn missing_usage_leaves_the_prior_and_learned_ratio_untouched() {
    let mut state = CompressState::new();
    state.calibration.observe(None, 4_000);
    assert_eq!(state.calibration.ratio(None), 1.0);
    assert_eq!(state.calibration.ratio(Some(1.25)), 1.25);
    state.calibration.observe(Some(6_000), 4_000);
    state.calibration.observe(None, 7_000);
    assert_eq!(state.calibration.ratio(None), 1.5);
}

#[test]
fn measured_ratios_above_the_old_cache_clamp_remain_authoritative() {
    let mut state = CompressState::new();
    state.calibration.observe(Some(30_000), 4_000);
    assert_eq!(state.calibration.ratio(None), 7.5);
    assert_eq!(calibrate_down(30_000, state.calibration.ratio(None)), 4_000);
    state.calibration.observe(Some(100), 4_000);
    state.calibration.observe(Some(0), 4_000);
    state.calibration.observe(Some(30_000), 0);
    assert_eq!(
        state.calibration.ratio(None),
        7.5,
        "partial or unusable counts cannot loosen the gate"
    );
}

#[test]
fn overflow_uses_stronger_evidence_or_tightens_the_failed_ratio() {
    let mut state = CompressState::new();
    assert_eq!(state.calibration.overflow(1.0), 1.5);
    assert_eq!(state.calibration.overflow(1.5), 2.25);
    state.calibration.observe(Some(4_000), 1_000);
    assert_eq!(state.calibration.overflow(1.0), 4.0);
    assert_eq!(state.calibration.ratio(None), 4.0);
}

#[test]
fn calibration_uses_measured_usage_before_guessing_an_overflow_ratio() {
    let mut state = CompressState::new();
    state.calibration.observe(Some(6_000), 4_000);
    assert_eq!(
        state.calibration.overflow(1.5),
        1.5,
        "an observed token ratio must not be replaced by a guessed multiplier"
    );
    assert_eq!(state.calibration.ratio(None), 1.5);
}

#[test]
fn repeated_usage_free_overflows_cannot_exceed_the_heuristic_ceiling() {
    let mut state = CompressState::new();
    // Six turns, each with an initial rejection and two failed shrinks.
    for _ in 0..18 {
        state.calibration.overflow(state.calibration.ratio(None));
    }
    let ratio = state.calibration.ratio(None);
    assert!(
        ratio <= 3.0,
        "usage-free overflow guesses must stay at or below 3.0, got {ratio}"
    );
    assert_eq!(
        ratio, 3.0,
        "repeated rejection must still tighten the prior"
    );
    assert_eq!(calibrate_down(9_000, ratio), 3_000);
}

#[test]
fn overflow_keeps_authoritative_measurements_above_the_guess_ceiling() {
    let mut state = CompressState::new();
    state.calibration.observe(Some(30_000), 4_000);
    for _ in 0..18 {
        assert_eq!(state.calibration.overflow(7.5), 7.5);
    }
    assert_eq!(calibrate_down(30_000, state.calibration.ratio(None)), 4_000);
}

#[test]
fn discarded_cache_hit_usage_retains_the_observed_sample_policy_on_overflow() {
    for inferred in [false, true] {
        let mut state = CompressState::new();
        if inferred {
            state.calibration.overflow(1.0);
            state.calibration.overflow(1.5);
        }
        let retained = state.calibration.ratio(None);
        // A reported suffix count is an observation, but cannot loosen the
        // retained prior. It still selects usage over an invented multiplier.
        for tokens in [1, 100, 500] {
            state.calibration.observe(Some(tokens), 4_000);
        }
        for _ in 0..18 {
            assert_eq!(state.calibration.overflow(retained), retained);
        }
    }
}

#[tokio::test]
async fn calibration_survives_compaction_anchor_invalidation_and_turn_reuse() {
    let messages = tool_heavy("preserve the operator task", 12, 2_000);
    let mut state = CompressState::new();
    let estimate = estimate_tokens(&messages, EST);
    state
        .calibration
        .observe(Some((estimate * 3) as u32), estimate * 2);
    let mut tracker = PromptTracker::new();
    tracker.record((estimate * 3 / 2) as u32, messages.len());
    let out = run(&messages, estimate / 2, None, None, &mut state).await;
    assert!(out.fired);
    tracker.invalidate();
    let next_turn = state.clone();
    assert_eq!(next_turn.calibration.ratio(None), 1.5);
    assert_eq!(
        tracker.current(&out.messages, None, next_turn.calibration.ratio(None), EST),
        calibrate_up(estimate_tokens(&out.messages, EST), 1.5)
    );
    state.reset();
    assert_eq!(
        state.calibration.ratio(None),
        1.0,
        "a new conversation starts with the cold prior"
    );
}
