use super::*;

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
    assert_eq!(sanitize_estimate_ratio(Some(3.0)), 3.0, "clamp inclusive");
}
/// #1968: dispatch calibration may only tighten the authoritative
/// send-budget gate, never loosen it — a stored EMA below 1.0 (a
/// cache-hit-poisoned sample that escaped exclusion) must not scale
/// preflight estimates DOWN below the model's real token cost.
#[test]
fn sanitize_estimate_ratio_floors_dispatch_at_1_0() {
    use super::sanitize_estimate_ratio;
    // Below 1.0 but inside the learning clamp: floored, not passed
    // through — this is the exact shape of the #1968 incident's final
    // stored ratio (0.9994337).
    assert_eq!(
        sanitize_estimate_ratio(Some(0.5)),
        1.0,
        "the clamp's own lower bound must not loosen the gate"
    );
    assert_eq!(sanitize_estimate_ratio(Some(0.9994337)), 1.0);
    // At or above 1.0: unaffected — the floor only ever raises, never
    // lowers, a value already safe to dispatch with.
    assert_eq!(sanitize_estimate_ratio(Some(1.0)), 1.0);
    assert_eq!(sanitize_estimate_ratio(Some(1.3)), 1.3);
}
/// #1967: the ONE truncation-suspect predicate — a suspect round's
/// prompt is window evidence of nothing (Ollama may have silently
/// head-truncated it), so nothing may treat it as proof of a safe
/// ceiling. Replays the incident's exact numbers: `num_ctx` 209,715
/// (the session's `safe_context`, absent an explicit `[backends]
/// num_ctx`), threshold 199,229 (95% of it, integer floor), and the
/// poisoned round's real 205,189 input tokens.
#[test]
fn is_truncation_suspect_replays_the_1967_incident_numbers() {
    use super::is_truncation_suspect;
    assert!(
        is_truncation_suspect(205_189, Some(209_715)),
        "205,189 is 97.8% of 209,715 — inside the suspect zone"
    );
    // The threshold itself: exactly 95% (integer floor) is suspect;
    // one token under is not.
    assert!(is_truncation_suspect(199_229, Some(209_715)));
    assert!(!is_truncation_suspect(199_228, Some(209_715)));
    // No known `num_ctx` — nothing to compare against, never suspect.
    assert!(!is_truncation_suspect(205_189, None));
    // A genuinely small prompt, nowhere near the window.
    assert!(!is_truncation_suspect(4_136, Some(209_715)));
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
