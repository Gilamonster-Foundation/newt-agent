use super::*;

#[test]
fn compaction_budget_drops_schema_overhead_for_the_tools_disabled_summary() {
    // #1528 B3 (req 7): a tool-capable request reserves the exposed schemas'
    // real-token overhead in its compaction target; the tools-DISABLED final
    // summary must NOT — subtracting schemas that are never sent makes the target
    // needlessly tight and over-compacts. At identity calibration the difference
    // between the two targets is EXACTLY the schema overhead.
    use super::ResponsesBudgetState;
    let mut state = ResponsesBudgetState::new(Some(32_768), 80, None, None, None, None);
    let recovered = state.recovered_budget_for_window(32_768);
    state.recover_from_cw400(recovered);
    state.set_tool_schema_tokens(1_000);
    let with_schemas = state.compaction_budget(1.0, true);
    let without_schemas = state.compaction_budget(1.0, false);
    assert!(
        without_schemas > with_schemas,
        "dropping the un-sent schemas RAISES the target: {without_schemas} !> {with_schemas}"
    );
    assert_eq!(
        without_schemas - with_schemas,
        1_000,
        "the tools-disabled target reclaims exactly the un-sent schema overhead"
    );
}
/// #1534 BHV-BUDGET-007: reactive recovery compacts to a target the
/// immediately-following preflight accepts — the compaction budget tracks
/// `actionable_input_budget` (min(hard ceiling, mid-loop trim)), never the
/// looser hard ceiling. Also proves the target round-trips through calibration
/// without exceeding that budget, and preserves an authoritative zero.
#[test]
fn recovery_compaction_target_cannot_exceed_the_next_preflight() {
    use super::ResponsesBudgetState;
    let est = crate::tokens::TokenEstimation::default();

    // Case A: the hard ceiling is the smallest — actionable == hard == 8,000.
    let mut a = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, Some(12_000));
    a.recover_from_cw400(8_000);
    assert_eq!(a.actionable_input_budget(), Some(8_000));
    assert_eq!(
        a.compaction_budget(1.0, true),
        8_000,
        "targets the 8,000 preflight enforces"
    );

    // Case B: the mid-loop trim is the smallest — actionable == 6,000 while the
    // hard ceiling is 16,000. The compactor MUST target 6,000, not 16,000, or
    // preflight would reject what it accepted (pre-fix targeted the ceiling).
    let mut b = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, Some(6_000));
    b.recover_from_cw400(16_000);
    assert_eq!(b.actionable_input_budget(), Some(6_000));
    assert_eq!(
        b.compaction_budget(1.0, true),
        6_000,
        "targets the 6,000 preflight enforces"
    );
    assert_ne!(
        b.compaction_budget(1.0, true),
        16_000,
        "must not target the looser hard ceiling"
    );

    // Case C: the target round-trips through calibration without exceeding the
    // budget the preflight enforces — a request compacted to the target passes.
    let mut c = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
    c.set_tool_schema_tokens(1_000);
    c.recover_from_cw400(20_000);
    let actionable_c = c.actionable_input_budget().unwrap();
    for ratio in [1.3f32, 2.0, 3.0] {
        let target_raw = c.compaction_budget(ratio, true); // chars/4 input budget
        let calibrated_input = super::calibrate_up(target_raw, ratio);
        assert!(
            calibrated_input + 1_000 <= actionable_c,
            "ratio {ratio}: calibrated compacted input ({calibrated_input}) + tools (1000) \
             must fit actionable ({actionable_c})",
        );
    }

    // Case D: an authoritative zero actionable budget stays zero (fail-closed)
    // — a zero compaction target and a 0-remaining self-read, NOT an unknown
    // ceiling.
    let d = ResponsesBudgetState::new(
        Some(16_000),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        None,
    );
    assert_eq!(d.actionable_input_budget(), Some(0));
    assert_eq!(
        d.compaction_budget(1.5, true),
        0,
        "zero budget → zero compaction target"
    );
    let report =
        crate::agentic::responses_context_remaining_report(None, &[], None, &d, 1.5, est, 15);
    assert!(
        report.contains("0 tokens remaining") && !report.contains("No input-token ceiling"),
        "an authoritative zero renders 0 remaining, not unknown: {report}",
    );
}
