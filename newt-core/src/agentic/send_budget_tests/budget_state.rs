use super::*;

/// THE numerical regression: with a 32K window at 80% and Contemplating
/// (16K output reserve), the percentage ceiling is 26,214 but the ENFORCED
/// hard ceiling is the tighter 16,768 — and EVERY Responses budget surface
/// reports that same 16,768, closing the old `get_context_remaining`
/// divergence by construction.
#[test]
fn responses_budget_state_reports_one_enforced_ceiling() {
    use super::ResponsesBudgetState;
    // The two legs of the ceiling: percentage-only (no reserve) vs
    // window-after-reserve. The authoritative ceiling is the tighter one.
    assert_eq!(num_ctx_input_ceiling(Some(32_768), 80, None), Some(26_214));
    assert_eq!(32_768 - 16_000, 16_768);

    let mut state = ResponsesBudgetState::new(
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        None,
    );
    state.set_tool_schema_tokens(0);

    // Every surface reports the enforced 16,768, not the un-reserved 26,214:
    // 1. dispatch preflight AND get_context_remaining both read
    // `actionable_input_budget`, 2. tool exposure reads the soft budget,
    // 3. the hard ceiling seeds cw-400 recovery. Here they coincide.
    assert_eq!(
        state.actionable_input_budget(),
        Some(16_768),
        "dispatch preflight + get_context_remaining ceiling"
    );
    assert_eq!(
        state.exposure_budget(),
        Some(16_768),
        "tool exposure clip budget"
    );
    assert_eq!(
        state.learned_hard_ceiling(),
        Some(16_768),
        "hard ceiling (seeds cw-400 recovery)"
    );
    assert_eq!(
        state.recovered_budget_for_window(32_768),
        16_768,
        "cw-400 recovery seed"
    );
    // 5. compaction target: recover into the collapsed ceiling, then the
    // chars/4 target (identity calibration, no schema overhead) is 16,768.
    let recovered = state.recovered_budget_for_window(32_768);
    state.recover_from_cw400(recovered);
    assert_eq!(
        state.compaction_budget(1.0, true),
        16_768,
        "compaction target"
    );
}
/// Monotone tighten-only (invariant #3): a recovered 400 can only lower the
/// learned ceiling; a LATER larger recovery never raises it, and a huge
/// recovered window is still clamped to the retained ceiling. The soft send
/// budget collapses to the new hard ceiling (fail-closed) and preflight
/// follows.
#[test]
fn learned_hard_ceiling_only_ever_tightens() {
    use super::ResponsesBudgetState;
    let mut state = ResponsesBudgetState::new(
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        None,
    );
    assert_eq!(state.learned_hard_ceiling(), Some(16_768));
    // A tighter recovery wins (a smaller learned ceiling always wins).
    state.recover_from_cw400(8_000);
    assert_eq!(state.learned_hard_ceiling(), Some(8_000));
    assert_eq!(
        state.soft_send_budget(),
        Some(8_000),
        "soft collapses to hard"
    );
    assert_eq!(state.actionable_input_budget(), Some(8_000));
    // A LATER larger recovery cannot raise it.
    state.recover_from_cw400(20_000);
    assert_eq!(
        state.learned_hard_ceiling(),
        Some(8_000),
        "a later value cannot raise the learned ceiling"
    );
    // Even a huge recovered window is clamped to the retained ceiling.
    assert_eq!(state.recovered_budget_for_window(1_000_000), 8_000);
}
/// Opt-in reserve: no cognition dial reserves nothing, so the ceiling is the
/// plain percentage bound (26,214) and every reader agrees.
#[test]
fn no_cognition_reserve_reports_the_percentage_ceiling() {
    use super::ResponsesBudgetState;
    let state = ResponsesBudgetState::new(Some(32_768), 80, None, None, None, None);
    assert_eq!(state.learned_hard_ceiling(), Some(26_214));
    assert_eq!(state.actionable_input_budget(), Some(26_214));
    assert_eq!(state.exposure_budget(), Some(26_214));
}
/// Tightening the ceiling can only DECREASE reported remaining, and more
/// instructions + tool-schema overhead (a larger `used`) further lowers it —
/// the two directions that keep the reported budget honest.
#[test]
fn tightening_and_overhead_only_shrink_reported_remaining() {
    use super::ResponsesBudgetState;
    use crate::agentic::budget::render_context_budget;
    let mut state = ResponsesBudgetState::new(
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        None,
    );
    let render = |s: &ResponsesBudgetState, used: usize| {
        render_context_budget(
            used,
            s.learned_hard_ceiling(),
            s.num_ctx(),
            s.input_ceiling_pct(),
            15,
        )
    };
    // 16,768 ceiling, 4,000 used → 12,768 remaining.
    assert!(render(&state, 4_000).contains("12768 tokens remaining"));
    // More instructions + schema overhead (a higher `used`) lowers remaining.
    assert!(render(&state, 8_000).contains("8768 tokens remaining"));
    // Tightening the ceiling to 8,000 cannot report MORE remaining.
    state.recover_from_cw400(8_000);
    assert!(render(&state, 4_000).contains("4000 tokens remaining"));
}
/// Invariant #1 at the state level: `None` num_ctx (cloud Responses) yields
/// NO ceiling — the state stays ceiling-less and every reader reports no
/// bound, leaving hosted OpenAI unchanged.
#[test]
fn cloud_responses_none_num_ctx_stays_ceiling_less() {
    use super::ResponsesBudgetState;
    let state =
        ResponsesBudgetState::new(None, 80, Some(Cognition::Contemplating), None, None, None);
    assert_eq!(state.learned_hard_ceiling(), None, "no window → no ceiling");
    assert_eq!(
        state.actionable_input_budget(),
        None,
        "nothing to refuse against"
    );
    assert_eq!(state.exposure_budget(), None, "no live budget → don't clip");
}
/// Invariant #2 at the state level: a window with no input room resolves to
/// an authoritative `Some(0)` ceiling that is NEVER erased and shadows cached
/// evidence — fail-closed, not fail-open.
#[test]
fn zero_input_room_window_stays_authoritative_zero() {
    use super::ResponsesBudgetState;
    // 80% of 16_000 = 12_800; window − 16_000 Contemplating reserve = 0 →
    // min = Some(0). A cached max_ok_input=2_000 must be shadowed, not win.
    let state = ResponsesBudgetState::new(
        Some(16_000),
        80,
        Some(Cognition::Contemplating),
        Some(2_000),
        None,
        None,
    );
    assert_eq!(
        state.learned_hard_ceiling(),
        Some(0),
        "no input room is an authoritative zero"
    );
    assert_eq!(
        state.soft_send_budget(),
        Some(0),
        "authoritative zero shadows cached evidence"
    );
    assert_eq!(state.actionable_input_budget(), Some(0));
}
/// #1534 monotonicity (CG-6 and the used-side): the actionable budget and the
/// reported remaining only ever move in the safe direction — tighter inputs
/// never buy more room, and a recovered hard ceiling never loosens.
#[test]
fn responses_budget_moves_only_in_the_safe_direction() {
    use super::ResponsesBudgetState;
    let est = crate::tokens::TokenEstimation::default();
    let input = [serde_json::json!({"role": "user", "content": "hi"})];

    // (a) Lowering an authoritative limit never RAISES the actionable budget.
    let loose = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
    let tight = ResponsesBudgetState::new(Some(32_768), 80, None, None, None, None);
    assert!(
        tight.actionable_input_budget() <= loose.actionable_input_budget(),
        "a smaller window cannot raise the actionable budget",
    );
    let capped = ResponsesBudgetState::new(Some(65_536), 80, None, None, Some(8_000), Some(6_000));
    assert!(
        capped.actionable_input_budget() <= loose.actionable_input_budget(),
        "a tighter safe_context + mid-loop trim cannot raise it",
    );

    // (b)/(c) Adding instructions or tool schemas never RAISES remaining.
    let s = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
    let ceiling = s.actionable_input_budget().expect("window binds");
    let rem = |instr: Option<&str>, tools: Option<&[serde_json::Value]>| {
        ceiling.saturating_sub(crate::agentic::estimate_responses_request_tokens(
            instr, &input, tools, est,
        ))
    };
    let big = "POLICY. ".repeat(200);
    assert!(
        rem(Some(&big), None) <= rem(None, None),
        "instructions never increase remaining",
    );
    let wire_tools = [serde_json::json!({
        "type": "function", "name": "run_command",
        "description": "run a shell command",
        "parameters": {"type": "object",
            "properties": {"command": {"type": "string"}}, "required": ["command"]}
    })];
    assert!(
        rem(None, Some(&wire_tools)) <= rem(None, None),
        "tool schemas never increase remaining",
    );

    // (d) A cw-400 only TIGHTENS the hard ceiling; a later larger recovered
    //     window cannot raise it back.
    let mut s = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
    let before = s.actionable_input_budget();
    s.recover_from_cw400(10_000);
    let after = s.actionable_input_budget();
    assert!(
        after <= before && after == Some(10_000),
        "a cw-400 tightens the actionable budget to the recovered window",
    );
    s.recover_from_cw400(50_000);
    assert_eq!(
        s.actionable_input_budget(),
        Some(10_000),
        "a later larger recovered window cannot raise the hard ceiling",
    );
}
