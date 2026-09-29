use super::*;

fn one_decision_intake() -> newt_core::agentic::PromptIntake {
    let intake = newt_core::agentic::PromptIntake::analyze(
        "Mark-then-rotate should be ordered so the drain sees the conversation \
         the action belonged to (drain before rotation, or stamp actions with \
         the conversation id).",
    );
    assert_eq!(
        intake.manifest().decisions().len(),
        1,
        "fixture must produce exactly one decision"
    );
    intake
}

/// A well-formed `PROPOSAL: n` / `SUMMARY: …` response is read as a
/// proposal for that ordinal.
#[test]
fn a_well_formed_response_becomes_a_proposal() {
    let intake = one_decision_intake();
    let proposal = classify_clarification_reply(&intake, "okay, so do it", |_prompt| {
        Ok("PROPOSAL: 1\nSUMMARY: drain before rotation".to_string())
    })
    .expect("a well-formed response must classify");
    assert_eq!(proposal.ordinal, 1);
    assert_eq!(proposal.summary, "drain before rotation");
}

/// `PROPOSAL: none` — the classifier declining — yields no proposal. This is
/// the expected outcome for a genuinely ambiguous reply, not a failure.
#[test]
fn an_explicit_none_yields_no_proposal() {
    let intake = one_decision_intake();
    let proposal = classify_clarification_reply(&intake, "not sure yet", |_prompt| {
        Ok("PROPOSAL: none\nSUMMARY:".to_string())
    });
    assert!(proposal.is_none());
}

/// Anything that is not the exact two-line contract — free prose, a missing
/// `PROPOSAL:` line, explanatory padding — is "no proposal". Fail-closed:
/// this is a classifier the harness cannot force to answer correctly, so an
/// unparseable answer must never be treated as a confident one.
#[test]
fn a_malformed_response_is_treated_as_no_proposal() {
    let intake = one_decision_intake();
    for malformed in [
        "I think they mean option 1, but I'm not fully sure.",
        "SUMMARY: drain before rotation",
        "",
        "PROPOSAL: soon",
    ] {
        let response = malformed.to_string();
        let proposal =
            classify_clarification_reply(&intake, "okay, so do it", |_prompt| Ok(response));
        assert!(proposal.is_none(), "must not classify: {malformed:?}");
    }
}

/// A failed side call (backend down, timeout, …) is "no proposal" — the same
/// fail-closed default as every other classifier in this harness.
#[test]
fn a_failed_side_call_is_treated_as_no_proposal() {
    let intake = one_decision_intake();
    let proposal = classify_clarification_reply(&intake, "okay, so do it", |_prompt| {
        Err(anyhow::anyhow!("backend unreachable"))
    });
    assert!(proposal.is_none());
}

/// An empty `SUMMARY:` still yields a proposal — the ordinal is what
/// matters for locking; a filler label is supplied rather than treating a
/// blank summary as a malformed response.
#[test]
fn an_empty_summary_still_proposes_with_a_filler_label() {
    let intake = one_decision_intake();
    let proposal = classify_clarification_reply(&intake, "okay, so do it", |_prompt| {
        Ok("PROPOSAL: 1\nSUMMARY:".to_string())
    })
    .expect("an ordinal alone is still a proposal");
    assert_eq!(proposal.ordinal, 1);
    assert!(!proposal.summary.is_empty());
}

/// The prompt sent to the classifier carries the pending batch and the
/// operator's own reply text, not a generic instruction — this is the whole
/// input the classifier has to work with.
#[test]
fn the_classifier_prompt_carries_the_batch_and_the_reply() {
    let intake = one_decision_intake();
    let mut seen_prompt = String::new();
    let _ = classify_clarification_reply(&intake, "okay, so do it", |prompt| {
        seen_prompt = prompt;
        Ok("PROPOSAL: none".to_string())
    });
    assert!(seen_prompt.contains("okay, so do it"), "{seen_prompt}");
    assert!(
        seen_prompt.contains("drain before rotation"),
        "{seen_prompt}"
    );
}
