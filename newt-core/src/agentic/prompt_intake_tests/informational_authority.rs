use super::*;

/// A statement of fact calls for an explanation rather than inventing an
/// action request. Classification itself neither grants nor revokes authority.
#[test]
fn an_informational_prompt_does_not_invent_an_action_request() {
    let intake = PromptIntake::analyze(GIT_REMOTE_FYI);
    assert_eq!(
        intake.disposition(),
        PromptDisposition::Explain,
        "a stated fact does not request a new action"
    );
    assert!(
        intake.atomic_asks().iter().all(AtomicAsk::is_informational),
        "the clause states rather than asks: {:?}",
        intake.atomic_asks()
    );
}

/// Ordinary observations may require commands. Preserve session authority and
/// budget; an informational clause still cannot resolve a pending decision.
#[test]
fn an_informational_turn_preserves_session_authority_and_budget() {
    let intake = PromptIntake::analyze(GIT_REMOTE_FYI);
    assert_eq!(intake.disposition(), PromptDisposition::Explain);
    assert_eq!(
        intake.disposition().tool_round_limit(8),
        8,
        "inferred response style does not narrow the configured budget"
    );
    assert_eq!(
        PromptDisposition::Ask.tool_round_limit(8),
        0,
        "…and Ask is excluded because it is terminal, not merely strict"
    );
    assert!(
        intake.model_card().contains("session grants"),
        "the model is told which authority governs its tools: {}",
        intake.model_card()
    );
}
