use super::*;

fn ctx() -> newt_core::TurnPromptContext {
    newt_core::TurnPromptContext::ephemeral_operator(
        "conv",
        b"extract the module and open a PR".to_vec(),
        b"extract the module and open a PR".to_vec(),
    )
}

#[test]
fn bare_continue_after_round_cap_links_to_the_interrupted_objective() {
    let parent = ctx();
    let got = upgrade_origin_for_interrupted_objective(
        ModelInputOrigin::Operator,
        "continue",
        Some(&parent),
    );
    match got {
        ModelInputOrigin::OperatorContinuation { parent: linked } => assert_eq!(
            linked.submitted_prompt().id(),
            parent.submitted_prompt().id(),
            "the nudge must re-enter the interrupted objective's lineage"
        ),
        other => panic!("bare continue must link, got {other:?}"),
    }
}

#[test]
fn substantive_input_stays_fresh_even_with_an_interrupted_objective() {
    let parent = ctx();
    let got = upgrade_origin_for_interrupted_objective(
        ModelInputOrigin::Operator,
        "now refactor newt-tui/src/lib.rs instead and open a PR",
        Some(&parent),
    );
    assert!(
        matches!(got, ModelInputOrigin::Operator),
        "a new ask must never be silently chained to a stale objective"
    );
}

#[test]
fn no_interrupted_objective_means_no_upgrade() {
    let got =
        upgrade_origin_for_interrupted_objective(ModelInputOrigin::Operator, "continue", None);
    assert!(matches!(got, ModelInputOrigin::Operator));
}

#[test]
fn pending_clarification_continuations_are_left_untouched() {
    let parent = ctx();
    let pending = ModelInputOrigin::OperatorContinuation {
        parent: Box::new(ctx()),
    };
    let before_id = match &pending {
        ModelInputOrigin::OperatorContinuation { parent } => parent.submitted_prompt().id(),
        _ => unreachable!(),
    };
    let got = upgrade_origin_for_interrupted_objective(pending, "continue", Some(&parent));
    match got {
        ModelInputOrigin::OperatorContinuation { parent: kept } => assert_eq!(
            kept.submitted_prompt().id(),
            before_id,
            "a pending-clarification link outranks the round-cap link"
        ),
        other => panic!("existing continuation must be preserved, got {other:?}"),
    }
}

#[test]
fn durable_substantive_operator_prompt_consumes_the_round_cap_link() {
    let mut interrupted = Some(ctx());
    consume_interrupted_objective_for_accepted_prompt(
        &mut interrupted,
        &ModelInputOrigin::Operator,
    );
    assert!(
        interrupted.is_none(),
        "a fresh accepted objective must not leave the old cap link armed"
    );
}

#[test]
fn accepted_continuations_and_harness_retries_keep_the_round_cap_link() {
    for origin in [
        ModelInputOrigin::OperatorContinuation {
            parent: Box::new(ctx()),
        },
        ModelInputOrigin::HarnessRetry {
            parent: Box::new(ctx()),
        },
    ] {
        let mut interrupted = Some(ctx());
        consume_interrupted_objective_for_accepted_prompt(&mut interrupted, &origin);
        assert!(
            interrupted.is_some(),
            "continuations and derived input must preserve the objective link"
        );
    }
}

#[test]
fn accepted_web_objective_consumes_the_old_round_cap_link() {
    let mut interrupted = Some(ctx());
    consume_interrupted_objective_for_accepted_prompt(
        &mut interrupted,
        &ModelInputOrigin::WebInjected {
            inbox_id: "inbox".to_string(),
        },
    );
    assert!(interrupted.is_none());
}

#[test]
fn round_cap_footer_is_deterministic_and_only_decorates_capped_replies() {
    let reply = "Completed the parser; tests remain.";
    let capped = decorate_round_cap_reply(reply, Some(newt_core::TurnEndReason::RoundCap));
    assert!(capped.starts_with(reply), "{capped}");
    assert!(capped.contains("If work remains"), "{capped}");
    assert!(capped.contains("`continue`"), "{capped}");
    assert!(capped.contains("`/rounds <n>`"), "{capped}");
    assert_eq!(
        decorate_round_cap_reply(reply, None),
        reply,
        "ordinary replies must remain byte-for-byte unchanged"
    );
}

#[test]
fn terminal_notices_preserve_questions_and_incomplete_observations() {
    let question = "Which project should I update?";
    let awaiting =
        decorate_round_cap_reply(question, Some(newt_core::TurnEndReason::AwaitingOperator));
    assert_eq!(
        awaiting, question,
        "host notices must not become model material"
    );
    let mut metrics = newt_core::TurnMetrics {
        end_reason: Some(newt_core::TurnEndReason::AwaitingOperator),
        ..Default::default()
    };
    assert!(metrics.display_line().contains("awaiting operator"));
    for reason in [
        newt_core::TurnEndReason::NarrationCapExhausted,
        newt_core::TurnEndReason::NarrationFinalRound,
    ] {
        let reply = "I will inspect the test failure.";
        let incomplete = decorate_round_cap_reply(reply, Some(reason));
        assert_eq!(incomplete, reply);
        metrics.end_reason = Some(reason);
        assert!(metrics.display_line().contains("incomplete"));
    }
    assert_eq!(
        decorate_round_cap_reply(
            "The answer is 3.",
            Some(newt_core::TurnEndReason::Completed)
        ),
        "The answer is 3."
    );
}

#[test]
fn capped_progress_is_persistable_without_duplicate_notices_and_resumes_its_objective() {
    let parent = ctx();
    let core_handoff = "Progress captured.\n\nPaused at the tool-round limit (40 rounds).";
    let persisted =
        decorate_round_cap_reply(core_handoff, Some(newt_core::TurnEndReason::RoundCap));
    assert_eq!(
        persisted.matches("tool-round limit").count(),
        1,
        "the TUI adds only the interactive affordance: {persisted}"
    );
    assert_eq!(persisted.matches('⏸').count(), 1, "{persisted}");
    assert!(persisted.contains("`continue`"), "{persisted}");

    let resumed = upgrade_origin_for_interrupted_objective(
        ModelInputOrigin::Operator,
        "continue",
        Some(&parent),
    );
    match resumed {
        ModelInputOrigin::OperatorContinuation { parent: linked } => assert_eq!(
            linked.submitted_prompt().id(),
            parent.submitted_prompt().id(),
            "the persisted capped turn must resume the interrupted objective"
        ),
        other => panic!("capped progress must resume as a continuation, got {other:?}"),
    }
}

/// #2334: the failed-turn footer names a phrase that re-enters the objective the
/// Err branch keeps. Trap: a footer naming a phrase the router does not treat as
/// a bare continuation (e.g. `retry`) would pass a text-only check while the
/// operator's reply silently started a fresh objective.
#[test]
fn the_failed_turn_footer_phrase_resumes_the_failed_objective() {
    let failed = ctx();
    assert!(failed_turn_footer().contains("`continue`"));
    assert!(newt_core::classifiers::is_bare_continuation("continue"));
    match upgrade_origin_for_interrupted_objective(
        ModelInputOrigin::Operator,
        "continue",
        Some(&failed),
    ) {
        ModelInputOrigin::OperatorContinuation { parent } => {
            assert_eq!(
                parent.submitted_prompt().id(),
                failed.submitted_prompt().id()
            );
        }
        other => panic!("the footer phrase must resume the failed objective, got {other:?}"),
    }
}

fn objective(text: &str) -> newt_core::TurnPromptContext {
    newt_core::TurnPromptContext::ephemeral_operator(
        "conv",
        text.as_bytes().to_vec(),
        text.as_bytes().to_vec(),
    )
}

/// Accept a fresh operator objective the way chat does: comprehend, record.
fn accept(recorded: &mut RecordedDispositions, text: &str) -> newt_core::TurnPromptContext {
    let context = objective(text);
    let intake = intake_for_accepted_prompt(
        &ModelInputOrigin::Operator,
        text,
        None,
        recorded,
        &newt_core::agentic::DispositionLexicon::default(),
    );
    record_turn_disposition(recorded, &context, &intake);
    context
}

/// Resume `parent` with `nudge` the way chat does: link, mint the continuation
/// receipt, comprehend, record. Returns the continuation turn and its intake.
fn resume(
    recorded: &mut RecordedDispositions,
    parent: &newt_core::TurnPromptContext,
    nudge: &str,
    pending: Option<&PendingClarification>,
) -> (
    newt_core::TurnPromptContext,
    newt_core::agentic::PromptIntake,
) {
    let origin = match pending {
        Some(pending) => ModelInputOrigin::OperatorContinuation {
            parent: pending.parent.clone(),
        },
        None => upgrade_origin_for_interrupted_objective(
            ModelInputOrigin::Operator,
            nudge,
            Some(parent),
        ),
    };
    assert!(
        matches!(origin, ModelInputOrigin::OperatorContinuation { .. }),
        "{nudge:?} must link to the pending objective"
    );
    let context =
        newt_core::TurnPromptContext::ephemeral_operator_continuation("conv", nudge, nudge, parent)
            .expect("same-conversation continuation");
    let intake = intake_for_accepted_prompt(
        &origin,
        nudge,
        pending,
        recorded,
        &newt_core::agentic::DispositionLexicon::default(),
    );
    record_turn_disposition(recorded, &context, &intake);
    (context, intake)
}

/// #2332 / #2283: "try now?" after a failed or capped turn resumes that
/// objective; the same words with nothing pending stay an ordinary prompt.
#[test]
fn a_question_shaped_retry_resumes_only_a_pending_objective() {
    let parent = ctx();
    for nudge in ["try now?", "try again?", "retry"] {
        match upgrade_origin_for_interrupted_objective(
            ModelInputOrigin::Operator,
            nudge,
            Some(&parent),
        ) {
            ModelInputOrigin::OperatorContinuation { parent: linked } => assert_eq!(
                linked.submitted_prompt().id(),
                parent.submitted_prompt().id()
            ),
            other => panic!("{nudge:?} must resume the pending objective, got {other:?}"),
        }
        assert!(matches!(
            upgrade_origin_for_interrupted_objective(ModelInputOrigin::Operator, nudge, None),
            ModelInputOrigin::Operator
        ));
    }
}

/// #2332: a bare continuation resumes the objective, including its authority.
/// The nudge used to be classified on its own text. Since #2337 both the
/// install request and "retry" read as Act, so that pair alone cannot fail;
/// "continue?" still reads as Explain and would have narrowed the request.
#[test]
fn a_bare_continuation_takes_the_objectives_disposition_not_its_own() {
    use newt_core::agentic::{PromptDisposition, PromptIntake};
    let text = "Can you install skills for that herdr tool?";
    assert_eq!(
        PromptIntake::analyze(text).disposition(),
        PromptDisposition::Act
    );
    assert_eq!(
        PromptIntake::analyze("continue?").disposition(),
        PromptDisposition::Explain,
        "precondition: the nudge alone would narrow"
    );
    for nudge in ["retry", "continue?"] {
        let mut recorded = RecordedDispositions::new();
        let parent = accept(&mut recorded, text);
        let (_, intake) = resume(&mut recorded, &parent, nudge, None);
        assert_eq!(intake.disposition(), PromptDisposition::Act, "{nudge:?}");
    }
}

/// The no-amplify case: an Explain objective resumed with "continue" (Act on
/// its own text) stays Explain. A one-word nudge never widens a task.
#[test]
fn an_explain_objective_resumed_with_continue_stays_explain() {
    assert_eq!(
        newt_core::agentic::PromptIntake::analyze("continue").disposition(),
        newt_core::agentic::PromptDisposition::Act,
        "precondition: the nudge alone would widen"
    );
    let mut recorded = RecordedDispositions::new();
    let parent = accept(&mut recorded, "How does prompt intake work?");
    let (_, intake) = resume(&mut recorded, &parent, "continue", None);
    assert_eq!(
        intake.disposition(),
        newt_core::agentic::PromptDisposition::Explain
    );
}

/// The chain: O is capped, "continue" is capped, "continue" again. The second
/// continuation's parent is the first continuation, whose own active prompt is
/// the word "continue"; the authority must still be O's.
#[test]
fn a_chained_continue_keeps_the_objectives_disposition() {
    let mut recorded = RecordedDispositions::new();
    let objective = accept(&mut recorded, "How does prompt intake work?");
    let (first, _) = resume(&mut recorded, &objective, "continue", None);
    let (_, second) = resume(&mut recorded, &first, "continue", None);
    assert_eq!(
        second.disposition(),
        newt_core::agentic::PromptDisposition::Explain
    );
}

/// A clarified objective capped after its answer resumes with the answered
/// authority. Re-reading the answer ("1: sqlite") gives Act; re-reading the
/// question gives Ask and re-asks a locked decision. Neither may happen.
#[test]
fn a_clarified_objective_resumed_after_a_cap_does_not_ask_again() {
    let mut recorded = RecordedDispositions::new();
    let question = "Should we use SQLite or Postgres for the cache?";
    let objective = accept(&mut recorded, question);
    let asked = newt_core::agentic::PromptIntake::analyze(question);
    assert_eq!(
        asked.disposition(),
        newt_core::agentic::PromptDisposition::Ask,
        "fixture needs a pending decision"
    );
    let pending = PendingClarification {
        parent: Box::new(objective.clone()),
        intake: asked,
    };
    let (answered, resolved) = resume(&mut recorded, &objective, "1: sqlite", Some(&pending));
    assert_ne!(
        resolved.disposition(),
        newt_core::agentic::PromptDisposition::Ask
    );
    let (_, resumed) = resume(&mut recorded, &answered, "continue", None);
    assert_eq!(resumed.disposition(), resolved.disposition());
}

/// The recorded value wins over a re-read of the objective's words: an
/// operator's `[intake]` edit mid-session must not change an accepted task.
#[test]
fn a_resumed_objective_uses_its_recorded_disposition_not_a_rereading() {
    let mut recorded = RecordedDispositions::new();
    let parent = objective("How does prompt intake work?");
    recorded.insert(
        parent.submitted_prompt().id(),
        newt_core::agentic::PromptDisposition::Research,
    );
    let (_, intake) = resume(&mut recorded, &parent, "continue", None);
    assert_eq!(
        intake.disposition(),
        newt_core::agentic::PromptDisposition::Research
    );
}

/// Twin: a pending clarification still resolves with the operator's answer
/// rather than reclassifying the parent.
#[test]
fn a_pending_clarification_still_resolves_with_the_operators_answer() {
    let question = "Should we use SQLite or Postgres for the cache?";
    let mut recorded = RecordedDispositions::new();
    let parent = objective(question);
    let pending = PendingClarification {
        parent: Box::new(parent.clone()),
        intake: newt_core::agentic::PromptIntake::analyze(question),
    };
    let (_, resolved) = resume(&mut recorded, &parent, "1: sqlite", Some(&pending));
    assert_eq!(resolved.manifest().pending_decision_count(), 0);
}
