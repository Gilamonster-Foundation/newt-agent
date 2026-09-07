use super::*;

#[test]
fn action_prompt_is_atomic_and_metadata_is_content_free() {
    let secret = "ship the private parser change to /top-secret";
    let intake = PromptIntake::analyze(secret);

    assert_eq!(intake.disposition(), PromptDisposition::Act);
    assert_eq!(intake.atomic_asks().len(), 1);
    intake.validate().unwrap();
    let artifact_metadata = intake.artifact_metadata();
    assert_eq!(
        artifact_metadata["schema"],
        "prompt_comprehension_manifest_v3"
    );
    let metadata = artifact_metadata.to_string();
    assert!(!metadata.contains("private parser"));
    assert!(metadata.contains("atomic_ask_digests"));
    let card = intake.model_card();
    assert!(card.starts_with(PROMPT_COMPREHENSION_MODEL_CARD_PREFIX));
    assert!(!card.contains("private parser"));
}

#[test]
fn empty_headless_input_is_a_bounded_ask_not_act() {
    let empty = PromptIntake::analyze("   \n");
    assert_eq!(empty.disposition(), PromptDisposition::Ask);
    assert_eq!(empty.manifest().pending_decision_count(), 1);
    assert!(empty.clarification_batch().contains("non-empty task"));
    assert_eq!(
        empty
            .resolve_with_operator_answer("Explain receipts.")
            .disposition(),
        PromptDisposition::Explain
    );
}

#[test]
fn unresolved_choice_becomes_a_bounded_ask_then_explicit_answer_acts() {
    let intake = PromptIntake::analyze(
        "Implement either SQLite or Postgres; create the migration and open a PR.",
    );

    assert_eq!(intake.disposition(), PromptDisposition::Ask);
    assert_eq!(intake.atomic_asks().len(), 2);
    assert!(intake.clarification_batch().contains("SQLite"));
    assert_eq!(
        intake
            .resolve_with_operator_answer("continue")
            .disposition(),
        PromptDisposition::Ask,
        "an acknowledgement cannot choose a concrete implementation"
    );
    let resolved = intake.resolve_with_operator_answer("1: SQLite");
    assert_eq!(resolved.disposition(), PromptDisposition::Act);
    assert_eq!(resolved.manifest().pending_decision_count(), 0);
    assert_eq!(
        resolved.artifact_metadata()["decision_source_counts"]["operator"],
        1
    );
    resolved.validate().unwrap();
}

#[test]
fn multiple_decisions_require_explicit_ordinal_mapping() {
    let intake = PromptIntake::analyze(
        "Choose either SQLite or Postgres. Select either staging or production.",
    );
    assert_eq!(intake.manifest().pending_decision_count(), 2);
    assert_eq!(
        intake
            .resolve_with_operator_answer("SQLite\nproduction")
            .disposition(),
        PromptDisposition::Ask
    );
    assert_eq!(
        intake
            .resolve_with_operator_answer("1: SQLite\n2: production")
            .disposition(),
        PromptDisposition::Act
    );
}

#[test]
fn intake_overflow_remains_ask_and_cannot_be_answered_in_place() {
    let prompt = (0..=MAX_CONCRETE_DECISIONS)
        .map(|i| format!("Choose either option-{i}-a or option-{i}-b."))
        .collect::<Vec<_>>()
        .join("\n");
    let intake = PromptIntake::analyze(&prompt);

    assert_eq!(intake.disposition(), PromptDisposition::Ask);
    assert!(
        intake
            .manifest()
            .decisions()
            .iter()
            .any(super::DecisionLock::is_overflow),
        "a truncated decision set must retain an explicit overflow lock"
    );
    assert_eq!(
        intake
            .resolve_with_operator_answer("1: option-0-a")
            .disposition(),
        PromptDisposition::Ask,
        "the overflow lock cannot be converted into Act by a partial answer"
    );
}

#[test]
fn atomic_ask_overflow_remains_ask() {
    let prompt = (0..=MAX_ATOMIC_ASKS)
        .map(|i| format!("Implement bounded item {i}."))
        .collect::<Vec<_>>()
        .join("\n");
    let intake = PromptIntake::analyze(&prompt);

    assert_eq!(intake.atomic_asks().len(), MAX_ATOMIC_ASKS);
    assert_eq!(intake.disposition(), PromptDisposition::Ask);
    assert!(intake.manifest().decisions().iter().any(|decision| {
        decision.is_overflow() && decision.status() == super::DecisionStatus::Pending
    }));
}

#[test]
fn ordinal_answers_are_relative_to_the_pending_batch() {
    assert_eq!(
        super::explicit_answer_indices("1: continue", &[4]),
        Some(vec![4]),
        "the first displayed clarification must resolve the first pending decision, not raw decision zero"
    );
    assert_eq!(
        super::explicit_answer_indices("1: one\n2: two", &[2, 6]),
        Some(vec![2, 6])
    );
}
