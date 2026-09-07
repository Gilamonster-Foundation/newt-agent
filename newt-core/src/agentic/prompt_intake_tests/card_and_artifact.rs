use super::*;

/// **The stated fact survives.** Both halves of the evidenced failure: the
/// model is told the fact in its own turn, and the durable artifact can say
/// WHAT was stated rather than only that something 92 bytes long was.
#[test]
fn the_stated_fact_survives_to_the_card_and_the_durable_artifact() {
    let intake = PromptIntake::analyze(GIT_REMOTE_FYI);

    let card = intake.model_card();
    assert!(
        card.contains(GIT_REMOTE_FYI),
        "the model must be told the fact it was given: {card}"
    );
    assert!(
        card.contains("carry no request to act"),
        "…and told it is not authorization: {card}"
    );

    let metadata = intake.artifact_metadata();
    assert_eq!(metadata["informational_ask_count"], 1);
    assert_eq!(
        metadata["informational_asks"][0].as_str(),
        Some(GIT_REMOTE_FYI),
        "a digest cannot be read back; the durable record must be able to \
         say what was dropped"
    );
    assert_eq!(metadata["atomic_ask_kinds"][0], "informational");
}

/// The content-free rule is UNCHANGED for everything an operator
/// instructed or decided — only stated facts are carried, and the
/// pre-existing secret-bearing action prompt proves it, because that
/// prompt is an instruction and stays digest-only.
#[test]
fn an_instruction_carries_no_text_into_the_artifact_or_the_card() {
    let intake = PromptIntake::analyze("ship the private parser change to /top-secret");
    let metadata = intake.artifact_metadata().to_string();
    assert!(!metadata.contains("private parser"), "{metadata}");
    assert_eq!(intake.artifact_metadata()["informational_ask_count"], 0);
    assert!(!intake.model_card().contains("private parser"));
    assert!(!intake.model_card().contains("noted_facts"));
}

/// #2051: the card names WHOSE decision the disposition is, and marks
/// itself as plumbing.
///
/// The evidenced 9b session answered `hello?` and then told the operator
/// *"this is an 'explain' turn, so I won't be making any changes"*. The
/// action line alone reads as a rule imposed from outside and worth
/// announcing; these two clauses are what say otherwise.
#[test]
fn the_card_states_the_disposition_is_the_harness_own_inference() {
    let intake = PromptIntake::analyze("hello?");
    assert_eq!(intake.disposition_source(), DispositionSource::Inferred);
    let card = intake.model_card();
    assert!(card.contains("disposition: explain"), "{card}");
    assert!(
        card.contains("disposition_source: the harness inferred this"),
        "the card must say the harness inferred this: {card}"
    );
    assert!(
        card.contains("disposition_privacy:"),
        "the card must say it is not for the operator: {card}"
    );
    // The suppression is of the mechanism, not of honesty about limits.
    assert!(card.contains("say plainly what you cannot do"), "{card}");
}

/// Review of #2057: `/mode plan` and `/mode diagnose` reach
/// `enforce_read_only` on the operator's own standing instruction, so a
/// card that still says "the operator did not choose it" is false there.
/// Every narrowed disposition gets both clauses AND the policy provenance;
/// a new variant cannot ship a card that reads as an unattributed cage.
#[test]
fn a_policy_narrowed_card_credits_the_session_mode_not_the_prompt() {
    for disposition in [
        PromptDisposition::Explain,
        PromptDisposition::Research,
        PromptDisposition::Plan,
    ] {
        let mut intake = PromptIntake::analyze("fix the parser");
        assert_eq!(intake.disposition(), PromptDisposition::Act);
        intake.enforce_read_only(disposition);
        assert_eq!(
            intake.disposition_source(),
            DispositionSource::SessionPolicy
        );
        let card = intake.model_card();
        assert!(
            card.contains("disposition_source: a session mode the operator set"),
            "{disposition:?}: {card}"
        );
        assert!(
            !card.contains("did not choose it"),
            "{disposition:?}: the card must not deny the operator's own mode choice: {card}"
        );
        assert!(
            card.contains("disposition_privacy:"),
            "{disposition:?}: {card}"
        );
    }
}

/// An `Ask` intake is terminal: `enforce_read_only` changes nothing, so it
/// must not relabel the provenance either.
#[test]
fn narrowing_an_ask_intake_keeps_its_inferred_provenance() {
    let mut intake = PromptIntake::analyze("pick either parser and fix it");
    assert_eq!(intake.disposition(), PromptDisposition::Ask);
    intake.enforce_read_only(PromptDisposition::Plan);
    assert_eq!(intake.disposition(), PromptDisposition::Ask);
    assert_eq!(intake.disposition_source(), DispositionSource::Inferred);
}

#[test]
fn read_only_attenuation_keeps_model_card_and_artifact_in_sync() {
    let mut action = PromptIntake::analyze("Implement the requested parser change.");
    assert_eq!(action.disposition(), PromptDisposition::Act);

    action.enforce_read_only(PromptDisposition::Plan);

    assert_eq!(action.disposition(), PromptDisposition::Plan);
    assert!(
        action.model_card().contains("disposition: plan"),
        "{}",
        action.model_card()
    );
    assert_eq!(
        action.artifact_metadata()["schema"],
        "prompt_comprehension_manifest_v3"
    );
    assert_eq!(action.artifact_metadata()["disposition"], "plan");

    let mut research = PromptIntake::analyze("Investigate the parser behavior.");
    research.enforce_read_only(PromptDisposition::Research);
    assert_eq!(
        research.disposition(),
        PromptDisposition::Research,
        "the mode-selected read-only disposition must remain consistent"
    );
}
