/// #1534 BHV-BUDGET-001/002/003: the Responses self-read and the dispatch
/// preflight share ONE calibrated estimate in ONE real-token currency. For
/// every calibration ratio the reported remaining equals
/// `actionable − calibrate_up(raw, ratio)`, and the preflight refuses against
/// that same real value — never the raw chars/4 estimate.
#[test]
fn responses_self_read_and_dispatch_agree_across_calibration() {
    use super::ResponsesBudgetState;
    let est = crate::tokens::TokenEstimation::default();
    let instr = "SYSTEM POLICY. ".repeat(64);
    let input = [serde_json::json!({"role": "user", "content": "please do the task"})];
    let tools = [serde_json::json!({
        "type": "function", "name": "run_command",
        "description": "run a shell command in the workspace",
        "parameters": {"type": "object",
            "properties": {"command": {"type": "string"}}, "required": ["command"]}
    })];
    // A generous window so the request fits with positive remaining.
    let state = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
    let actionable = state.actionable_input_budget().expect("window binds");
    let raw =
        crate::agentic::estimate_responses_request_tokens(Some(&instr), &input, Some(&tools), est);
    for ratio in [0.5f32, 1.0, 1.3, 2.0, 3.0] {
        let real = super::calibrate_up(raw, ratio);
        // The shared calibrated estimator IS calibrate_up(raw).
        assert_eq!(
            crate::agentic::estimate_responses_request_real_tokens(
                Some(&instr),
                &input,
                Some(&tools),
                est,
                ratio,
            ),
            real,
            "ratio {ratio}: calibrated estimator == calibrate_up(raw)",
        );
        // Self-read: remaining == actionable − real.
        let report = crate::agentic::responses_context_remaining_report(
            Some(&instr),
            &input,
            Some(&tools),
            &state,
            ratio,
            est,
            15,
        );
        let expected = actionable.saturating_sub(real);
        assert!(
            report.contains(&format!("{expected} tokens remaining")),
            "ratio {ratio}: remaining must be actionable({actionable}) − real({real}) \
             = {expected}: {report}",
        );
        // Dispatch refuses against the SAME real value: Ok exactly at `real`,
        // Err one token below it.
        assert!(
            crate::agentic::preflight_responses_request(
                Some(&instr),
                &input,
                Some(&tools),
                Some(real),
                ratio,
                est,
                "m",
            )
            .is_ok(),
            "ratio {ratio}: preflight accepts a budget equal to the real estimate",
        );
        assert!(
            crate::agentic::preflight_responses_request(
                Some(&instr),
                &input,
                Some(&tools),
                Some(real.saturating_sub(1)),
                ratio,
                est,
                "m",
            )
            .is_err(),
            "ratio {ratio}: preflight refuses one token below the real estimate",
        );
    }
}
/// #1534 BHV-BUDGET-002 fail-on-old: calibration decides the low-budget
/// warning. A request whose CALIBRATED size nearly fills the budget is LOW;
/// the pre-fix self-read subtracted the smaller RAW estimate and looked
/// healthy. This FAILS on 8f0111c (which rendered the raw estimate). Mirrors
/// the 10,000 / 6,000 / ×1.5 / 9,000 / 1,000 shape.
#[test]
fn responses_self_read_low_budget_tracks_calibration() {
    use super::ResponsesBudgetState;
    let est = crate::tokens::TokenEstimation::default();
    let input = [serde_json::json!({"role": "user", "content": "x ".repeat(4_000)})];
    let raw = crate::agentic::estimate_responses_request_tokens(None, &input, None, est);
    let ratio = 1.5f32;
    let real = super::calibrate_up(raw, ratio);
    // Budget just above the CALIBRATED size: real leaves < 15% (LOW); the raw
    // estimate leaves ~36% (not LOW). Guarantees the flip for any raw.
    let actionable = (real + real / 20) as u32;
    let state = ResponsesBudgetState::new(None, 80, None, None, Some(actionable), None);
    assert_eq!(state.actionable_input_budget(), Some(actionable as usize));
    let calibrated = crate::agentic::responses_context_remaining_report(
        None, &input, None, &state, ratio, est, 15,
    );
    // The pre-fix behaviour: subtract the RAW estimate from the same ceiling.
    let raw_report = crate::agentic::budget::render_context_budget(
        raw,
        state.actionable_input_budget(),
        state.num_ctx(),
        state.input_ceiling_pct(),
        15,
    );
    assert!(
        calibrated.contains("Budget is LOW"),
        "the calibrated self-read must warn LOW ({real} real of {actionable}): {calibrated}",
    );
    assert!(
        !raw_report.contains("Budget is LOW"),
        "the pre-fix raw self-read did NOT warn — the bug ({raw} raw of {actionable}): {raw_report}",
    );
}
