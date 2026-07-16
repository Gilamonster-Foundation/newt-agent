use super::RoundObservation;

/// Input-token ceiling implied by the `num_ctx` THIS request will carry
/// (issue #282, the B6 hole): `num_ctx` caps the backend's whole KV window —
/// input *plus* reply — so the input budget is 80 % of it, the same reply/
/// estimate headroom the probe's budget math already reserves against a hard
/// window (`safe_context` bootstraps at 80 % of the declared window; a cw-400
/// sets `max_ok_input` to 80 % of the parsed limit). `None` (model-default
/// window) or zero contributes nothing — the zero-is-disabled contract (F3),
/// never a compress-to-zero.
pub(super) fn num_ctx_input_ceiling(num_ctx: Option<u32>, input_ceiling_pct: u32) -> Option<usize> {
    num_ctx
        .map(|c| (c as usize) * (input_ceiling_pct as usize) / 100)
        .filter(|&c| c > 0)
}

/// Initial pre-send budget for one turn (issue #282; Phase 20 semantics per
/// `docs/design/model-self-tuning.md` §2.1): the empirically-cached figure is
/// `max(max_ok_input, safe_context)` composed, via `min`, with the
/// [`num_ctx_input_ceiling`] of the `num_ctx` the loop is about to send.
///
/// `max_ok_input` is a high-water mark of PROVEN-good input — a floor, not a
/// ceiling. Preferring it over `safe_context` (the pre-Phase-20 contract)
/// turned "largest prompt seen so far" into a cap, which is the motivating
/// failure: a stale 6,068 ratchet refused sends the backend was accepting at
/// 8,734 tokens. `max()` lets whichever of proven-good and believed-safe is
/// larger drive the budget. The cw-400 path already reins `safe_context`
/// down to its authoritative cap, so after a hard 400 `max()` still lands on
/// the authoritative number.
///
/// The `num_ctx` ceiling composition is unchanged: before #282 the budget
/// was the cached numbers alone — unset on a fresh capability cache until
/// the turn ENDS, so the first turn of a session had no effective ceiling
/// and a 41k-token request sailed into a forced 4,096 window with zero
/// compression events (the measured B6 failure: 8/10 silently wrong). The
/// ceiling is a real token budget: when it fires the trigger, `hard_budget`
/// semantics apply (consults + feeds anti-thrash).
pub(super) fn initial_send_budget(
    max_ok_input: Option<u32>,
    safe_context: Option<u32>,
    num_ctx: Option<u32>,
    input_ceiling_pct: u32,
) -> Option<usize> {
    let cached = match (max_ok_input, safe_context) {
        (Some(m), Some(s)) => Some(m.max(s) as usize),
        (m, s) => m.or(s).map(|c| c as usize),
    };
    match (cached, num_ctx_input_ceiling(num_ctx, input_ceiling_pct)) {
        (Some(budget), Some(ceiling)) => Some(budget.min(ceiling)),
        (budget, ceiling) => budget.or(ceiling),
    }
}

/// Convert a chars/4 estimate into real (backend-reported) token space using
/// the learned per-model `estimate_ratio` (Phase 20,
/// `docs/design/model-self-tuning.md` §2.3). Ceiling: estimates must err on
/// the side of counting, never undercounting — the 18.1 rule.
pub(super) fn calibrate_up(est: usize, ratio: f32) -> usize {
    (est as f32 * ratio).ceil() as usize
}

/// Convert a real-token budget into estimate (chars/4) space — the currency
/// the compression pipeline measures and reclaims in (Phase 20 §2.3).
/// Floor: a tighter target is safer than a looser one.
pub(super) fn calibrate_down(real: usize, ratio: f32) -> usize {
    (real as f32 / ratio).floor() as usize
}

/// Sanitize a per-model `estimate_ratio` for one turn (Phase 20 §2.3): only
/// finite values inside the learning clamp [0.5, 3.0] are trusted; anything
/// else (absent, NaN, a corrupted cache entry) degrades to 1.0 — the
/// identity, i.e. exactly the pre-calibration behavior.
pub(super) fn sanitize_estimate_ratio(estimate_ratio: Option<f32>) -> f32 {
    estimate_ratio
        .filter(|r| r.is_finite() && (0.5..=3.0).contains(r))
        .unwrap_or(1.0)
}

/// Report one quality-gated [`RoundObservation::Accepted`] (Phase 20 §2.2).
/// Called only from usable-output control paths (tool calls or non-empty
/// content — the quality gate); skips when the prompt was truncation-suspect
/// (≥95% of the request's `num_ctx`, where Ollama may have silently dropped
/// the head) or when the backend reported no usage for the round.
pub(super) fn emit_accepted(
    hook: &mut Option<&mut dyn FnMut(RoundObservation)>,
    round_usage: Option<crate::TokenUsage>,
    truncation_suspect: bool,
    estimated_tokens: usize,
) {
    if truncation_suspect {
        return;
    }
    if let (Some(hook), Some(u)) = (hook.as_deref_mut(), round_usage) {
        hook(RoundObservation::Accepted {
            prompt_tokens: u.input_tokens,
            estimated_tokens,
        });
    }
}

// Unit tests for the #282 budget wiring: the `num_ctx` a request will carry
// must participate in the pre-send budget — composing with the cached
// capability numbers via `min`, vanishing when absent, and never turning a
// zero/absent window into a compress-to-zero (F3).
#[cfg(test)]
mod send_budget_tests {
    use super::super::compress::{compression_trigger, CompressionTriggerLimits};
    use super::{initial_send_budget, num_ctx_input_ceiling};
    use crate::CompactionTriggerPolicy;

    /// THE B6 first-turn hole: a fresh capability cache (no `max_ok_input`,
    /// no `safe_context`) used to mean NO budget at all even though the
    /// request itself carried `options.num_ctx = 4096`. The ceiling must now
    /// arm the trigger on turn 1 — as a HARD budget (anti-thrash semantics).
    #[test]
    fn first_turn_fresh_cache_trigger_sees_the_num_ctx_ceiling() {
        let budget = initial_send_budget(None, None, Some(4096), 80);
        assert_eq!(budget, Some(3276), "80% of 4096 — reply headroom reserved");
        // The measured B6 shape: ~41k estimated tokens, 3 messages, no
        // count/token thresholds in reach — pre-fix this returned None and
        // the request sailed into the 4k window with zero events.
        let trigger = compression_trigger(
            3,
            41_355,
            39_900,
            CompressionTriggerLimits {
                count_threshold: 40,
                token_threshold: None,
                send_budget: budget,
                tool_tokens: 1_432,
                policy: CompactionTriggerPolicy::HeadroomAware,
                has_authoritative_headroom: true,
            },
        )
        .expect("the ceiling must fire the trigger on the first turn");
        assert!(trigger.hard_budget, "a real token budget, not a soft halve");
        assert_eq!(
            trigger.budget,
            3_276 - 1_432,
            "budget lands in message space: ceiling minus tool-schema tokens"
        );
        assert_eq!(trigger.max_messages, None, "no count firing here");
    }

    /// Absent `num_ctx` → exactly the cached-numbers budget (no ceiling).
    /// CONTRACT CHANGED in Phase 20 (docs/design/model-self-tuning.md §2.1):
    /// the cached figure is now `max(max_ok_input, safe_context)` — the
    /// high-water mark is a floor of proven-good, not a ceiling, so it must
    /// never pull the budget BELOW the believed-safe window.
    #[test]
    fn absent_num_ctx_leaves_the_budget_unchanged() {
        assert_eq!(initial_send_budget(None, None, None, 80), None);
        assert_eq!(
            initial_send_budget(Some(2_000), None, None, 80),
            Some(2_000)
        );
        assert_eq!(
            initial_send_budget(None, Some(5_000), None, 80),
            Some(5_000)
        );
        assert_eq!(
            initial_send_budget(Some(2_000), Some(5_000), None, 80),
            Some(5_000),
            "an HWM below safe_context is a floor, not a cap — safe_context wins"
        );
        // And with no budget at all, the trigger stays silent regardless of size.
        assert_eq!(
            compression_trigger(
                3,
                41_355,
                39_900,
                CompressionTriggerLimits {
                    count_threshold: 40,
                    token_threshold: None,
                    send_budget: None,
                    tool_tokens: 1_432,
                    policy: CompactionTriggerPolicy::HeadroomAware,
                    has_authoritative_headroom: false,
                },
            ),
            None
        );
    }

    /// Phase 20 §2.1 — the max(proven, believed) contract, all three shapes:
    /// HWM below the claim, HWM above the claim (proven beyond it), and the
    /// post-cw-400 shape where `safe_context` was reined to the authoritative
    /// cap so `max()` still lands on the authoritative number.
    #[test]
    fn cached_budget_is_max_of_proven_and_believed() {
        // The motivating failure: max_ok_input ratcheted to 6,068 (largest
        // prompt SEEN) while safe_context believed 80% of a 32k window safe.
        // Pre-fix the 6,068 won and refused sends the backend accepted.
        assert_eq!(
            initial_send_budget(Some(6_068), Some(26_214), None, 80),
            Some(26_214),
            "HWM below safe_context → safe_context"
        );
        // Proven beyond the claim: an accepted 8,734-token prompt outranks a
        // conservative claim-derived window.
        assert_eq!(
            initial_send_budget(Some(8_734), Some(6_553), None, 80),
            Some(8_734),
            "HWM above safe_context (proven beyond the claim) → HWM"
        );
        // cw-400-reined shape (#223): the 400 set max_ok_input to 80% of the
        // endpoint's reported hard limit (authoritative, may be HIGH) and
        // reined safe_context down to equal-or-lower — max() must land on
        // the authoritative cap, not regress to the VRAM-capped figure.
        assert_eq!(
            initial_send_budget(Some(800_000), Some(64_000), None, 80),
            Some(800_000),
            "post-cw-400: max_ok_input is the authoritative cap"
        );
        assert_eq!(
            initial_send_budget(Some(800_000), Some(800_000), None, 80),
            Some(800_000)
        );
    }

    /// Phase 20 §2.3 — the calibration converters: ratio 1.0 is the identity,
    /// estimate→real rounds UP (must-err-on-counting, the 18.1 rule),
    /// real→estimate rounds DOWN (a tighter compression target is safer).
    #[test]
    fn calibration_helpers_round_in_the_safe_direction() {
        use super::{calibrate_down, calibrate_up, sanitize_estimate_ratio};
        // Identity at 1.0 — the no-calibration baseline is exact.
        assert_eq!(calibrate_up(6_068, 1.0), 6_068);
        assert_eq!(calibrate_down(6_068, 1.0), 6_068);
        // The measured nemotron3 shape: chars/4 undercounts ~30% (×1.3).
        assert_eq!(calibrate_up(1_000, 1.3), 1_300);
        assert_eq!(calibrate_down(1_000, 1.3), 769, "floor, never round up");
        // Fractional results: up ceils, down floors.
        assert_eq!(calibrate_up(3, 1.5), 5, "4.5 ceils to 5");
        assert_eq!(calibrate_down(3, 2.0), 1, "1.5 floors to 1");
        // Sanitizer: absent / NaN / out-of-clamp all degrade to identity.
        assert_eq!(sanitize_estimate_ratio(None), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(f32::NAN)), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(0.1)), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(5.0)), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(1.29)), 1.29);
        assert_eq!(sanitize_estimate_ratio(Some(0.5)), 0.5, "clamp inclusive");
        assert_eq!(sanitize_estimate_ratio(Some(3.0)), 3.0, "clamp inclusive");
    }

    /// Phase 20 §2.3 — currency composition at the trigger boundary: a
    /// real-token send budget minus calibrated-up tool tokens, fired through
    /// the trigger, then calibrated DOWN into the pipeline's chars/4 space,
    /// must equal converting each leg separately (the e2e wiring in both
    /// loops relies on this composition).
    #[test]
    fn calibration_composes_across_the_trigger_boundary() {
        use super::{calibrate_down, calibrate_up};
        let cal = 1.3_f32;
        let send_budget = 8_734_usize; // real tokens
        let tool_tokens_est = 1_000_usize; // chars/4 estimate
        let tool_tokens_real = calibrate_up(tool_tokens_est, cal); // 1,300
        let current_real = calibrate_up(9_000, cal); // estimate → real
        let trigger = compression_trigger(
            3,
            current_real,
            9_000,
            CompressionTriggerLimits {
                count_threshold: 40,
                token_threshold: None,
                send_budget: Some(send_budget),
                tool_tokens: tool_tokens_real,
                policy: CompactionTriggerPolicy::HeadroomAware,
                has_authoritative_headroom: true,
            },
        )
        .expect("over-budget context fires the guard");
        assert!(trigger.hard_budget);
        // trigger.budget is real space (send budget minus real tool tokens);
        // the pipeline target converts it back to estimate space.
        assert_eq!(trigger.budget, send_budget - tool_tokens_real);
        let pipeline_budget = calibrate_down(trigger.budget, cal);
        assert_eq!(pipeline_budget, calibrate_down(8_734 - 1_300, cal));
        assert!(
            pipeline_budget < trigger.budget,
            "ratio > 1: the estimate-space target is tighter than the real one"
        );
    }

    /// The ceiling composes with existing budgets via `min` — whichever is
    /// tighter wins, in both directions.
    #[test]
    fn ceiling_composes_with_cached_budgets_via_min() {
        // Cached cap tighter than the ceiling: cached wins (mid-loop B5
        // behavior is untouched by #282).
        assert_eq!(
            initial_send_budget(Some(2_135), None, Some(4_096), 80),
            Some(2_135)
        );
        // Ceiling tighter than the cached cap: the B6 shape — bootstrap
        // safe_context 104,857 vs forced num_ctx 4,096.
        assert_eq!(
            initial_send_budget(None, Some(104_857), Some(4_096), 80),
            Some(3_276)
        );
        assert_eq!(
            initial_send_budget(Some(104_857), Some(104_857), Some(4_096), 80),
            Some(3_276)
        );
    }

    /// Zero/tiny `num_ctx` must never become a compress-to-zero budget — the
    /// zero-is-disabled contract (F3) holds at the source.
    #[test]
    fn zero_or_tiny_num_ctx_is_no_budget_at_all() {
        assert_eq!(num_ctx_input_ceiling(None, 80), None);
        assert_eq!(num_ctx_input_ceiling(Some(0), 80), None);
        assert_eq!(
            num_ctx_input_ceiling(Some(1), 80),
            None,
            "80% rounds to zero"
        );
        assert_eq!(initial_send_budget(None, None, Some(0), 80), None);
        // A zero ceiling must not shadow a real cached budget either.
        assert_eq!(
            initial_send_budget(Some(2_000), None, Some(0), 80),
            Some(2_000)
        );
    }
}
