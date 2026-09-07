use super::*;

/// **The fix.** A statement of fact no longer buys execution authority.
///
/// Before this, the prompt matched no action, research or explain needle,
/// did not end in `?`, and fell through to `Act` — full authority and the
/// full round budget, granted on the absence of any evidence of intent.
#[test]
fn an_informational_prompt_does_not_grant_act_authority() {
    let intake = PromptIntake::analyze(GIT_REMOTE_FYI);
    assert_eq!(
        intake.disposition(),
        PromptDisposition::Explain,
        "a stated fact authorizes nothing"
    );
    assert!(
        intake.atomic_asks().iter().all(AtomicAsk::is_informational),
        "the clause states rather than asks: {:?}",
        intake.atomic_asks()
    );
}

/// **Why `Explain` and not `Research`** — the decision, made visible so it
/// can be overruled in one enum value.
///
/// The defect is AUTHORITY, not budget: an informational turn was granted
/// the power to mutate, and mutate is what it did. `Explain` removes that
/// and keeps the ordinary round budget, because a stated fact often
/// deserves a read before answering — "the remote is X" may reasonably be
/// checked against the remote actually configured. `Research` would also
/// cap rounds at 3, which is a COST heuristic wearing an authorization
/// rule's clothes, and would tell the model to go gather evidence when what
/// it was given was a fact.
///
/// `Ask` is excluded for a different reason: it is terminal with a ZERO
/// round limit, so routing every unclassified statement there would end the
/// turn without a reply and nag on each one.
#[test]
fn an_informational_turn_keeps_its_budget_and_loses_its_authority() {
    let intake = PromptIntake::analyze(GIT_REMOTE_FYI);
    assert_eq!(intake.disposition(), PromptDisposition::Explain);
    assert_eq!(
        intake.disposition().tool_round_limit(8),
        8,
        "the budget is unchanged — this fix is about authority"
    );
    assert_eq!(
        PromptDisposition::Ask.tool_round_limit(8),
        0,
        "…and Ask is excluded because it is terminal, not merely strict"
    );
    assert!(
        intake.model_card().contains("answer without mutation"),
        "the model is told it may not mutate: {}",
        intake.model_card()
    );
}
