use super::*;

#[test]
fn responses_honors_the_configured_window_as_a_local_safety_limit() {
    // #1526 (invariant #4): a CONFIGURED context window is a local safety
    // limit for Responses even though the wire sends no `num_ctx`. #1528: the
    // seam PROJECTS from `ResponsesBudgetState`, so it reserves the cognition
    // output allowance the loop reserves — the budget is the RESERVED ceiling
    // (16,768), NOT the un-reserved percentage bound (26,214) the seam used to
    // over-report, and NOT `None` (the old, now-reversed contract).
    let ceiling = super::initial_context_input_budget(
        BackendKind::Openai,
        OpenAiApi::Responses,
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        ChatCompletionsCapability {
            cognition: Some(true),
            ..Default::default()
        },
        ReasoningReplayScope::CurrentUserTurn,
        None,
        None,
    );
    // 32_768 − 16_000 Contemplating output reserve = 16_768, tighter than the
    // 80% percentage bound (26,214) — the SAME value the Responses loop enforces.
    assert_eq!(
        ceiling,
        Some(16_768),
        "the Responses seam projects the RESERVED ceiling the loop enforces (#1528)"
    );
    // The cloud default (no configured window) still yields no local ceiling —
    // the change is opt-in via configuration and does not affect hosted OpenAI.
    assert_eq!(
        super::initial_context_input_budget(
            BackendKind::Openai,
            OpenAiApi::Responses,
            None,
            80,
            Some(Cognition::Contemplating),
            ChatCompletionsCapability {
                cognition: Some(true),
                ..Default::default()
            },
            ReasoningReplayScope::CurrentUserTurn,
            None,
            None,
        ),
        None,
        "an UNset num_ctx (cloud Responses) still has no local ceiling",
    );
}
/// P1.1 (#1534): the public reporting seam PROJECTS from the shared
/// `ResponsesBudgetState`, so for EVERY cognition level and representative
/// configured/learned combo the seam equals the state's authoritative input
/// budget (its soft send budget). Pre-fix the Responses branch re-derived the
/// ceiling with NO output reserve and diverged (26,214 vs 16,768).
#[test]
fn seam_projects_the_responses_budget_state_for_every_cognition() {
    use super::ResponsesBudgetState;
    let capability = ChatCompletionsCapability {
        cognition: Some(true),
        ..Default::default()
    };
    let cognitions = [
        None,
        Some(Cognition::Glancing),
        Some(Cognition::Pondering),
        Some(Cognition::Deliberating),
        Some(Cognition::Contemplating),
    ];
    // (num_ctx, max_ok_input, safe_context): configured-window-only, cached
    // caps present, a learned/cached cap tighter than the window, and the
    // cloud default (no window).
    let combos = [
        (Some(32_768u32), None, None),
        (Some(65_536), Some(40_000), None),
        (Some(32_768), None, Some(8_000)),
        (Some(32_768), Some(6_068), Some(26_214)),
        (None, Some(12_000), Some(20_000)),
        (None, None, None),
    ];
    for cognition in cognitions {
        for (num_ctx, max_ok_input, safe_context) in combos {
            let seam = super::initial_context_input_budget(
                BackendKind::Openai,
                OpenAiApi::Responses,
                num_ctx,
                80,
                cognition,
                capability,
                ReasoningReplayScope::CurrentUserTurn,
                max_ok_input,
                safe_context,
            );
            let state =
                ResponsesBudgetState::new(num_ctx, 80, cognition, max_ok_input, safe_context, None);
            assert_eq!(
                seam,
                state
                    .soft_send_budget()
                    .map(|budget| u32::try_from(budget).unwrap()),
                "the seam must project the state's authoritative input budget \
                 (num_ctx={num_ctx:?}, max_ok={max_ok_input:?}, safe={safe_context:?}, \
                 cognition={cognition:?})",
            );
        }
    }
    // Lock the exact reserve divergence the fix closes: Contemplating at 32K
    // reports the RESERVED 16,768, never the un-reserved 26,214.
    let contemplating = super::initial_context_input_budget(
        BackendKind::Openai,
        OpenAiApi::Responses,
        Some(32_768),
        80,
        Some(Cognition::Contemplating),
        capability,
        ReasoningReplayScope::CurrentUserTurn,
        None,
        None,
    );
    assert_eq!(contemplating, Some(16_768));
    assert_ne!(contemplating, Some(26_214), "no un-reserved over-report");
}
