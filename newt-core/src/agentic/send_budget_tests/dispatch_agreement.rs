use super::*;

/// The `get_context_remaining` REGRESSION (#1528): the reported ceiling is
/// the ENFORCED 16,768, never the old `num_ctx_input_ceiling(num_ctx, pct,
/// None)` recompute (26,214). Rendered end to end, the report the model sees
/// quotes the enforced headroom, not the un-reserved percentage ceiling.
#[test]
fn get_context_remaining_reports_enforced_not_percentage_ceiling() {
    use super::ResponsesBudgetState;
    let state = ResponsesBudgetState::new(
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        None,
    );
    // The value get_context_remaining now feeds to render_context_budget is
    // the actionable input budget (here == the enforced hard ceiling).
    assert_eq!(state.actionable_input_budget(), Some(16_768));
    // The pre-fix recompute (reserve = None) over-advertised — the divergence.
    assert_eq!(
        num_ctx_input_ceiling(state.num_ctx(), 80, None),
        Some(26_214)
    );
    assert_ne!(state.actionable_input_budget(), Some(26_214));
    let report = crate::agentic::budget::render_context_budget(
        0,
        state.actionable_input_budget(),
        state.num_ctx(),
        state.input_ceiling_pct(),
        15,
    );
    assert!(report.contains("ceiling of ~16768"), "{report}");
    assert!(
        !report.contains("26214"),
        "must not advertise the un-reserved ceiling: {report}"
    );
}
/// P1.2 (#1534): `get_context_remaining` describes the NEXT dispatch — it
/// reads the SAME `actionable_input_budget` the preflight refuses against and
/// the SAME `estimate_responses_request_tokens` (instructions, the running
/// input, and the real Responses-wire tools) the preflight counts. For every
/// budget shape the rendered remaining equals
/// `actionable_input_budget − actual_responses_wire_estimate`.
#[test]
fn get_context_remaining_agrees_with_dispatch_across_budget_shapes() {
    use super::ResponsesBudgetState;
    let est = crate::tokens::TokenEstimation::default();
    let input = [serde_json::json!({"role": "user", "content": "do the thing"})];
    let big_instructions = "SYSTEM POLICY. ".repeat(200); // ~3,000 chars
    let wire_tools = [
        serde_json::json!({
            "type": "function",
            "name": "read_file",
            "description": "read a file in pages",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string"}, "offset": {"type": "integer"}
            }, "required": ["path"]}
        }),
        serde_json::json!({
            "type": "function",
            "name": "run_command",
            "description": "run a shell command in the workspace",
            "parameters": {"type": "object", "properties": {
                "command": {"type": "string"}
            }, "required": ["command"]}
        }),
    ];

    // The reconstruction the loop performs at the `get_context_remaining`
    // intercept: used = the shared wire estimator, ceiling = actionable.
    let agrees = |label: &str,
                  state: &ResponsesBudgetState,
                  instructions: Option<&str>,
                  tools: Option<&[serde_json::Value]>|
     -> usize {
        let used =
            crate::agentic::estimate_responses_request_tokens(instructions, &input, tools, est);
        let ceiling = state
            .actionable_input_budget()
            .unwrap_or_else(|| panic!("{label}: the actionable budget must bind"));
        let expected = ceiling.saturating_sub(used);
        let report = crate::agentic::budget::render_context_budget(
            used,
            state.actionable_input_budget(),
            state.num_ctx(),
            state.input_ceiling_pct(),
            15,
        );
        assert!(
            report.contains(&format!("{expected} tokens remaining")),
            "{label}: self-read remaining must equal actionable({ceiling}) − \
             wire_estimate({used}) = {expected}; got: {report}",
        );
        used
    };

    // 1. Only num_ctx constrains (no reserve, no cached caps).
    let s = ResponsesBudgetState::new(Some(32_768), 80, None, None, None, None);
    assert_eq!(s.actionable_input_budget(), Some(26_214));
    agrees("only num_ctx", &s, None, None);

    // 2. Only safe_context constrains (no window at all).
    let s = ResponsesBudgetState::new(None, 80, None, None, Some(8_000), None);
    assert_eq!(s.actionable_input_budget(), Some(8_000));
    // Pre-fix divergence: the old ceiling (learned_hard_ceiling) was None here,
    // so the self-read said "no ceiling" while dispatch refused at 8,000.
    assert_eq!(
        s.learned_hard_ceiling(),
        None,
        "no window → no hard ceiling"
    );
    agrees("only safe_context", &s, None, None);

    // 3. max_ok_input smaller than safe_context (a floor, never a cap).
    let s = ResponsesBudgetState::new(Some(32_768), 80, None, Some(12_000), Some(20_000), None);
    assert_eq!(s.actionable_input_budget(), Some(20_000));
    agrees("max_ok smaller", &s, None, None);

    // 4. mid_loop_trim_tokens smaller than the ceiling — it binds the dispatch.
    let s = ResponsesBudgetState::new(
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        Some(6_000),
    );
    assert_eq!(s.actionable_input_budget(), Some(6_000));
    // Divergence guard: the OLD ceiling over-advertised 16,768 while dispatch
    // refuses at 6,000.
    assert_eq!(s.learned_hard_ceiling(), Some(16_768));
    assert_ne!(s.learned_hard_ceiling(), s.actionable_input_budget());
    agrees("mid_loop_trim smaller", &s, None, None);

    // 5. Cognition reserve active (16,768 tighter than the 26,214 pct bound).
    let s = ResponsesBudgetState::new(
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        None,
    );
    assert_eq!(s.actionable_input_budget(), Some(16_768));
    agrees("cognition reserve", &s, None, None);

    // 6. Substantial instructions — the used side MUST count them (the old
    // Chat-shaped estimate omitted instructions entirely).
    let s = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
    let with_instr = agrees(
        "instructions substantial",
        &s,
        Some(&big_instructions),
        None,
    );
    let without_instr = crate::agentic::estimate_responses_request_tokens(None, &input, None, est);
    assert!(
        with_instr > without_instr,
        "instructions must raise the wire estimate (was omitted pre-fix)",
    );

    // 7. Responses tool schemas enabled — the used side counts the real
    //    Responses-wire tools.
    let with_tools = agrees("tools enabled", &s, None, Some(&wire_tools));
    // 8. Tools disabled — the used side drops the tool schemas.
    let without_tools = agrees("tools disabled", &s, None, None);
    assert!(
        with_tools > without_tools,
        "enabled wire tools must raise the estimate over tools-disabled",
    );

    // 9. A learned hard ceiling tighter than EVERY configured value (a cw-400
    //    recovery), so the self-read tracks the recovered constraint.
    let mut s = ResponsesBudgetState::new(
        Some(65_536),
        80,
        Some(Cognition::Deliberating),
        Some(40_000),
        Some(40_000),
        Some(30_000),
    );
    s.recover_from_cw400(9_000);
    assert_eq!(s.actionable_input_budget(), Some(9_000));
    // Tighter than the window ceiling, the cached caps, and the mid-loop trim.
    assert!(s.actionable_input_budget().unwrap() < 30_000);
    agrees(
        "learned ceiling tighter than all",
        &s,
        Some(&big_instructions),
        Some(&wire_tools),
    );

    // 10. max_ok_input is the SOLE cached cap and below the ceiling, so it
    //     binds as the smallest operative limit (safe_context absent — with
    //     safe_context present the two are max'd and safe_context wins).
    let s = ResponsesBudgetState::new(Some(32_768), 80, None, Some(12_000), None, None);
    assert_eq!(
        s.actionable_input_budget(),
        Some(12_000),
        "max_ok binds alone"
    );
    agrees("max_ok binds", &s, None, None);

    // 11. No authoritative ceiling (unknown cloud window, no caches): the
    //     self-read must STATE that no ceiling is known, never fabricate a
    //     remaining figure. `agrees` requires a bound, so assert directly.
    let s = ResponsesBudgetState::new(None, 80, Some(Cognition::Contemplating), None, None, None);
    assert_eq!(
        s.actionable_input_budget(),
        None,
        "an unknown window stays unknown — no fabricated ceiling"
    );
    let unknown = crate::agentic::budget::render_context_budget(
        crate::agentic::estimate_responses_request_tokens(None, &input, None, est),
        s.actionable_input_budget(),
        s.num_ctx(),
        s.input_ceiling_pct(),
        15,
    );
    assert!(
        unknown.contains("No input-token ceiling is configured"),
        "ceiling-less self-read must say so, not report remaining: {unknown}",
    );

    // 12. Authoritative zero (window minus the cognition reserve leaves no
    //     input room): a real fail-closed budget, never erased to None. The
    //     self-read reports 0 remaining, NOT "no ceiling".
    let s = ResponsesBudgetState::new(
        Some(16_000),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        None,
    );
    assert_eq!(
        s.actionable_input_budget(),
        Some(0),
        "window (16,000) − reserve (16,000) = 0 is an authoritative zero, not None",
    );
    let zero = crate::agentic::budget::render_context_budget(
        crate::agentic::estimate_responses_request_tokens(None, &input, None, est),
        s.actionable_input_budget(),
        s.num_ctx(),
        s.input_ceiling_pct(),
        15,
    );
    assert!(
        zero.contains("0 tokens remaining") && !zero.contains("No input-token ceiling"),
        "an authoritative zero renders 0 remaining, not ceiling-less: {zero}",
    );

    // --- fail-on-old: the SELF-READ SOURCE matters. The pre-fix intercept
    // read `learned_hard_ceiling` (which diverges from the enforced budget
    // when a soft / mid-loop limit binds, and is `None` when only
    // safe_context binds) and the CHAT-shaped estimator (which omits
    // instructions). Rendering / estimating each shape both ways proves the
    // OLD pair yields the WRONG answer — so a regression of the mod.rs
    // intercept back to the old source/estimator is caught here (the intercept
    // reads the NEW pair — `actionable_input_budget` +
    // `estimate_responses_request_tokens` — per the diff at the
    // `is_context_remaining_call` site).
    let render0 = |ceiling: Option<usize>, num_ctx: Option<u32>| {
        crate::agentic::budget::render_context_budget(0, ceiling, num_ctx, 80, 15)
    };
    // Item 2: safe_context=8,000 with no window. OLD (learned=None) → "no
    // ceiling"; NEW (actionable=8,000) → an 8,000 remaining budget.
    let s = ResponsesBudgetState::new(None, 80, None, None, Some(8_000), None);
    let old2 = render0(s.learned_hard_ceiling(), s.num_ctx());
    let new2 = render0(s.actionable_input_budget(), s.num_ctx());
    assert!(
        old2.contains("No input-token ceiling is configured"),
        "item 2 pre-fix source wrongly reports no ceiling: {old2}"
    );
    assert!(
        new2.contains("8000 tokens remaining"),
        "item 2 fixed source reports the enforced 8,000: {new2}"
    );
    assert_ne!(old2, new2, "item 2: the ceiling source changes the answer");
    // Item 3: a 6,000 mid-loop trim under a 16,768 hard ceiling. OLD
    // (learned=16,768) over-advertises; NEW (actionable=6,000) matches dispatch.
    let s = ResponsesBudgetState::new(
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        None,
        None,
        Some(6_000),
    );
    let old3 = render0(s.learned_hard_ceiling(), s.num_ctx());
    let new3 = render0(s.actionable_input_budget(), s.num_ctx());
    assert!(
        old3.contains("16768 tokens remaining"),
        "item 3 pre-fix over-advertises the hard ceiling: {old3}"
    );
    assert!(
        new3.contains("6000 tokens remaining"),
        "item 3 fixed source reports the 6,000 dispatch refuses at: {new3}"
    );
    assert_ne!(
        old3, new3,
        "item 3: the mid-loop trim is invisible to the pre-fix source"
    );
    // Item 5: the pre-fix self-read estimated the CHAT-shaped catalog and
    // OMITTED instructions. The Responses-wire estimate (instructions + flat
    // Responses tools) is the larger, honest count dispatch enforces.
    let responses_used = crate::agentic::estimate_responses_request_tokens(
        Some(&big_instructions),
        &input,
        Some(&wire_tools),
        est,
    );
    // The pre-fix Chat estimator takes the tool catalog as a single array
    // Value (Chat's nested `{function:{…}}` shape), not the flat Responses tools.
    let chat_tools = serde_json::json!([{
        "type": "function",
        "function": {"name": "run_command", "description": "run a shell command",
            "parameters": {"type": "object",
                "properties": {"command": {"type": "string"}}, "required": ["command"]}}
    }]);
    let chat_used = crate::agentic::trim::estimate_request_tokens(&input, Some(&chat_tools), est);
    assert!(
        responses_used > chat_used,
        "item 5: the Responses-wire estimate (instructions + flat tools, {responses_used}) must \
         exceed the pre-fix Chat-shaped estimate that omitted instructions ({chat_used})",
    );
}
