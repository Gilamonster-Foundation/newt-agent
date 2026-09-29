use super::*;

/// A prompt that trips exactly one decision needle.
fn one_decision() -> PromptIntake {
    let intake = PromptIntake::analyze(
        "Mark-then-rotate should be ordered so the drain sees the conversation \
         the action belonged to (drain before rotation, or stamp actions with \
         the conversation id).",
    );
    assert_eq!(
        intake.manifest.pending_decision_count(),
        1,
        "fixture must produce exactly one pending decision"
    );
    intake
}

/// Item 3: a single-item batch says "this decision", not "these decisions
/// … every item". The plural-over-one phrasing is what made a COMPLETE
/// batch read as a truncated one.
#[test]
fn a_single_item_batch_does_not_speak_in_the_plural() {
    let rendered = one_decision().clarification_batch();
    assert!(
        rendered.contains("I need this decision locked"),
        "singular phrasing for one item, got: {rendered}"
    );
    assert!(
        !rendered.contains("every item"),
        "'every item' over a one-item list reads as truncation: {rendered}"
    );
}

/// Item 2: an explicit ordinal outranks the `?` heuristic. Answering
/// `1: drain before rotation — sound right?` used to be refused outright
/// because a question mark appeared ANYWHERE in the reply.
#[test]
fn an_ordinal_answer_survives_a_question_mark() {
    let resolved =
        one_decision().resolve_with_operator_answer("1: drain before rotation — sound right?");
    assert_eq!(
        resolved.manifest.pending_decision_count(),
        0,
        "an explicit ordinal is an answer even with a '?' attached"
    );
    assert!(resolved.last_rejection().is_none());
}

/// #2517 follow-up: `.` and `)` are numbering punctuation just as much as
/// `:` — a human replying `1. Widen the local surface` (the actual shape the
/// bug report showed the gate rejecting) must lock the batch, not be told no
/// ordinal was found.
#[test]
fn a_dot_or_paren_numbered_reply_locks_like_a_colon() {
    for reply in ["1. Widen the local surface", "1) Widen the local surface"] {
        let resolved = one_decision().resolve_with_operator_answer(reply);
        assert_eq!(
            resolved.manifest.pending_decision_count(),
            0,
            "`{reply}` is as explicit an ordinal as `1: …`"
        );
        assert!(resolved.last_rejection().is_none());
    }
}

/// A bare `1.` or `1)` with nothing after it still locks — the ordinal
/// itself is the explicit signal, not the trailing text.
#[test]
fn a_bare_numbered_ordinal_with_no_trailing_text_still_locks() {
    for reply in ["1.", "1)"] {
        let resolved = one_decision().resolve_with_operator_answer(reply);
        assert_eq!(resolved.manifest.pending_decision_count(), 0, "`{reply}`");
        assert!(resolved.last_rejection().is_none());
    }
}

/// A decimal number must never be misread as a `.`-numbered ordinal — "3.14"
/// alone is not the operator saying "lock item 3".
#[test]
fn a_decimal_number_is_not_mistaken_for_a_dot_numbered_ordinal() {
    let intake = PromptIntake::analyze(
        "Should we use SQLite or Postgres for the cache?\n\
         Pick either the polling or the streaming transport.\n\
         Choose the retention window.",
    );
    let expected = intake.manifest.pending_decision_count();
    assert!(expected >= 3, "fixture needs at least three decisions");
    let resolved = intake.resolve_with_operator_answer("1: sqlite\n2: polling\n3.14");
    assert_eq!(
        resolved.manifest.pending_decision_count(),
        expected,
        "a bare decimal must not silently lock the third decision"
    );
    match resolved.last_rejection().expect("must record why") {
        ClarificationRejection::Incomplete { answered, .. } => assert_eq!(*answered, 2),
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

/// …but a reply carrying NO ordinals and reading as a question is still
/// refused — and now says so.
#[test]
fn a_bare_question_is_still_refused_but_explains_itself() {
    let resolved = one_decision().resolve_with_operator_answer("which one do you prefer?");
    assert_eq!(resolved.manifest.pending_decision_count(), 1);
    let rejection = resolved
        .last_rejection()
        .expect("a refusal must record why");
    assert_eq!(*rejection, ClarificationRejection::ReadsAsQuestion);
    let explained = rejection.explain();
    assert!(explained.contains("read as a question"), "{explained}");
    assert!(
        explained.contains("/new"),
        "every refusal names the escape hatch: {explained}"
    );
}

/// Item 1: prose with no ordinal at all is the common case, and the reason
/// must distinguish it from the question case.
#[test]
fn prose_without_an_ordinal_reports_the_missing_ordinal() {
    let resolved = one_decision().resolve_with_operator_answer("drain before rotation");
    let rejection = resolved.last_rejection().expect("must record why");
    assert_eq!(*rejection, ClarificationRejection::NoOrdinals);
    assert!(rejection.explain().contains("`N: value`"));
}

/// Item 1: a partial answer locks nothing, and now says how many landed
/// rather than leaving the operator to guess.
#[test]
fn a_partial_answer_reports_how_many_were_missing() {
    let intake = PromptIntake::analyze(
        "Should we use SQLite or Postgres for the cache?\n\
         Pick either the polling or the streaming transport.",
    );
    let expected = intake.manifest.pending_decision_count();
    assert!(expected >= 2, "fixture needs at least two decisions");
    let resolved = intake.resolve_with_operator_answer("1: sqlite");
    assert_eq!(
        resolved.manifest.pending_decision_count(),
        expected,
        "locking is all-or-nothing"
    );
    match resolved.last_rejection().expect("must record why") {
        ClarificationRejection::Incomplete {
            answered,
            expected: e,
        } => {
            assert_eq!(*answered, 1);
            assert_eq!(*e, expected);
        }
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

/// An out-of-range ordinal is reported as such — and only matters when the
/// batch is not otherwise fully answered.
///
/// This pins a small behavior change rather than leaving it incidental. The
/// old parser was inconsistent here: `0:` hard-rejected the whole reply (a
/// `?` on `checked_sub(1)` returned `None` for the entire function), while a
/// stray HIGH ordinal was silently skipped and tolerated as long as every
/// pending item was covered. Same operator mistake, two different outcomes.
/// Both now behave the same way, and the reason names the valid range.
#[test]
fn an_out_of_range_ordinal_is_named_and_only_blocks_an_incomplete_reply() {
    // Incomplete + out of range → the operator is told the valid range.
    let resolved = one_decision().resolve_with_operator_answer("7: chosen");
    assert_eq!(resolved.manifest.pending_decision_count(), 1);
    match resolved.last_rejection().expect("must record why") {
        ClarificationRejection::OutOfRange { ordinal, expected } => {
            assert_eq!(*ordinal, 7);
            assert_eq!(*expected, 1);
        }
        other => panic!("expected OutOfRange, got {other:?}"),
    }

    // A stray ordinal alongside a COMPLETE answer still locks — the batch
    // got what it needed. `0:` and `7:` agree now; they did not before.
    for stray in ["0: junk", "7: junk"] {
        let resolved = one_decision().resolve_with_operator_answer(&format!("1: chosen\n{stray}"));
        assert_eq!(
            resolved.manifest.pending_decision_count(),
            0,
            "a complete answer locks despite the stray `{stray}`"
        );
        assert!(resolved.last_rejection().is_none());
    }
}

/// Item 4, the landmine: every ordinal the batch DISPLAYS must be one the
/// resolver ACCEPTS.
///
/// The old code enumerated before filtering, so displayed numbers were
/// absolute indices into `decisions` while the resolver indexed the
/// pending-only slice. It could not diverge while locking stayed
/// all-or-nothing, so a parser-only test could not see it. This drives
/// render → parse → resolve as a round trip, which is what would catch a
/// future policy resolver locking one decision on its own.
#[test]
fn every_displayed_ordinal_is_one_the_resolver_accepts() {
    let mut intake = PromptIntake::analyze(
        "Should we use SQLite or Postgres for the cache?\n\
         Pick either the polling or the streaming transport.\n\
         Choose the retention window.",
    );
    let total = intake.manifest.decisions.len();
    assert!(total >= 3, "fixture needs at least three decisions");

    // Simulate exactly what today's code cannot: something locks the FIRST
    // decision without operator input, so pending no longer starts at 0.
    intake.manifest.decisions[0].status = DecisionStatus::Locked;
    intake.manifest.decisions[0].source = Some(DecisionSource::Operator);
    let pending_now = intake.manifest.pending_decision_count();
    assert!(pending_now >= 2);

    // Read the ordinals the operator would actually see.
    let rendered = intake.clarification_batch();
    let displayed: Vec<usize> = rendered
        .lines()
        .filter_map(|line| line.trim().split_once('.'))
        .filter_map(|(n, _)| n.trim().parse::<usize>().ok())
        .collect();
    assert_eq!(
        displayed.len(),
        pending_now,
        "the batch renders one line per pending decision: {rendered}"
    );

    // Answer using precisely those ordinals. If render and resolve used
    // different mappings, this would fail to lock the batch.
    let answer = displayed
        .iter()
        .map(|n| format!("{n}: chosen"))
        .collect::<Vec<_>>()
        .join("\n");
    let resolved = intake.resolve_with_operator_answer(&answer);
    assert_eq!(
        resolved.manifest.pending_decision_count(),
        0,
        "answering every DISPLAYED ordinal must lock the batch; \
         rendered:\n{rendered}\nanswer:\n{answer}"
    );
}

/// #2517: `/discuss` is the escape hatch — it must not be treated as a
/// malformed answer, must not lock or reject anything, and must hand the
/// harness the text to run through a side call.
#[test]
fn discuss_leaves_the_batch_untouched_and_surfaces_the_text() {
    let mut resolved = one_decision()
        .resolve_with_operator_answer("/discuss why does this need a decision at all?");
    assert_eq!(
        resolved.manifest.pending_decision_count(),
        1,
        "a discussion request locks nothing"
    );
    assert!(
        resolved.last_rejection().is_none(),
        "a discussion request is not a rejection"
    );
    assert_eq!(
        resolved.take_pending_discussion().as_deref(),
        Some("why does this need a decision at all?")
    );
    // Taken once, gone — it must not replay on the next unrelated turn.
    assert!(resolved.take_pending_discussion().is_none());
}

/// Bare `/discuss` (no text after it) still counts — the operator may just
/// want the batch re-explained rather than have a specific question typed.
#[test]
fn bare_discuss_still_counts_as_a_discussion_request() {
    let mut resolved = one_decision().resolve_with_operator_answer("/discuss");
    assert_eq!(resolved.manifest.pending_decision_count(), 1);
    assert_eq!(resolved.take_pending_discussion().as_deref(), Some(""));
}

/// `/chat` is accepted as a synonym, but a command that merely starts with
/// the same letters (`/discussion-of-x`, `/chatty`) is not — it must fall
/// through to the ordinary ordinal parser rather than being swallowed.
#[test]
fn only_the_exact_command_word_triggers_discussion() {
    let resolved = one_decision().resolve_with_operator_answer("/chat what about the fallback?");
    assert_eq!(resolved.manifest.pending_decision_count(), 1);
    assert!(resolved.last_rejection().is_none());

    let resolved = one_decision().resolve_with_operator_answer("/discussion-of-tradeoffs");
    assert_eq!(
        resolved.last_rejection(),
        Some(&ClarificationRejection::NoOrdinals),
        "a word that merely starts with /discuss is not the command"
    );
}

/// Every rejection and the un-rejected batch both name `/discuss` — the
/// point of the hatch is that an operator finds it without already knowing
/// it exists.
#[test]
fn discuss_is_named_everywhere_the_operator_would_look() {
    let rendered = one_decision().clarification_batch();
    assert!(rendered.contains("/discuss"), "{rendered}");

    let rejection = one_decision()
        .resolve_with_operator_answer("drain before rotation")
        .last_rejection()
        .expect("must record why")
        .explain();
    assert!(rejection.contains("/discuss"), "{rejection}");
    assert!(rejection.contains("/new"), "{rejection}");
}

/// #2517 "confirm-then-lock": a classifier's proposal is OFFERED, not
/// applied — it must not lock anything by itself.
#[test]
fn a_proposal_does_not_lock_until_confirmed() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");
    assert_eq!(
        proposed.manifest.pending_decision_count(),
        1,
        "a proposal alone locks nothing"
    );
    let notice = proposed
        .proposed_answer_notice()
        .expect("a proposal must produce a notice");
    assert!(notice.contains("drain before rotation"), "{notice}");
    assert!(notice.contains("yes"), "{notice}");
}

/// An exact affirmation confirms the SPECIFIC proposed decision, not a fresh
/// inference from the affirmation's own text.
#[test]
fn an_affirmation_locks_exactly_the_proposed_decision() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");
    for affirmation in ["yes", "Yes", " y ", "confirmed", "sounds right"] {
        let resolved = proposed.resolve_with_operator_answer(affirmation);
        assert_eq!(
            resolved.manifest.pending_decision_count(),
            0,
            "`{affirmation}` must confirm the pending proposal"
        );
        assert!(resolved.last_rejection().is_none());
        assert!(resolved.proposed_answer_notice().is_none());
    }
}

/// #2515 items 5/6 round 2 (RED FIRST — failed before the fix: the second
/// `yes` used to leave `pending_decision_count() == 1` and
/// `last_rejection().is_some()`, matching the live TUI defect measured in
/// `REVIEW-items5-6.md` — a proposal was consumed unconditionally by the
/// FIRST reply regardless of outcome, so a refusal silently dropped it and a
/// later `yes` had nothing left to confirm, even though the harness's own
/// refusal text still claimed it was "still waiting".
///
/// A non-affirmation, non-ordinal reply (`NoOrdinals`/`ReadsAsQuestion`) is a
/// REFUSAL, not a discard: the proposal must survive it, and a later `yes`
/// must still lock the SAME decision the proposal named.
#[test]
fn a_refusal_does_not_discard_a_live_proposal() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");
    let refused = proposed.resolve_with_operator_answer("actually let's talk about this more");
    assert_eq!(refused.manifest.pending_decision_count(), 1);
    assert!(refused.last_rejection().is_some(), "must be refused");
    assert!(
        refused.proposed_answer_notice().is_some(),
        "the proposal must still be on the table after a refusal"
    );

    // Sequence: propose -> refusal -> yes locks the SAME decision.
    let locked = refused.resolve_with_operator_answer("yes");
    assert_eq!(
        locked.manifest.pending_decision_count(),
        0,
        "`yes` must confirm the proposal that survived the refusal"
    );
    assert!(locked.last_rejection().is_none());
}

/// Sequence: propose -> a SECOND, different refusal -> yes still locks. The
/// proposal must survive more than one intervening refusal, not just one.
#[test]
fn a_proposal_survives_more_than_one_refusal() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");
    let refused_once = proposed.resolve_with_operator_answer("hm not sure");
    let refused_twice = refused_once.resolve_with_operator_answer("still thinking");
    assert!(refused_twice.proposed_answer_notice().is_some());
    let locked = refused_twice.resolve_with_operator_answer("yes");
    assert_eq!(locked.manifest.pending_decision_count(), 0);
}

/// Sequence: propose -> `/new` (a fresh `analyze`) -> yes locks nothing. A
/// fresh intake starts with no proposal at all — `/new` abandons the batch
/// entirely, and a bare `yes` against it is an ordinary unresolved answer.
#[test]
fn abandoning_the_batch_leaves_nothing_for_a_later_yes_to_confirm() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");
    assert!(proposed.proposed_answer_notice().is_some());
    let fresh = PromptIntake::analyze("a whole new task");
    let resolved = fresh.resolve_with_operator_answer("yes");
    assert!(
        resolved.last_rejection().is_some(),
        "a bare `yes` against a fresh intake with no proposal must not lock anything"
    );
}

/// An explicit ordinal answer overrides a live proposal outright — the
/// operator's own explicit statement always outranks the classifier's guess.
#[test]
fn an_explicit_ordinal_overrides_a_live_proposal() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");
    let resolved =
        proposed.resolve_with_operator_answer("1: stamp actions with the conversation id");
    assert_eq!(resolved.manifest.pending_decision_count(), 0);
    assert!(resolved.proposed_answer_notice().is_none());
}

/// An out-of-range ordinal is refused as a proposal rather than attaching to
/// the wrong (or no) decision.
#[test]
fn proposing_an_out_of_range_ordinal_is_a_no_op() {
    let proposed = one_decision().propose_answer(7, "not a real option");
    assert!(proposed.proposed_answer_notice().is_none());
}

/// Addendum item 5: a refusal issued while a classifier's proposal is
/// outstanding must say so — the proposal is silently consumed by
/// `resolve_with_operator_answer` on the very next reply, so a reply that
/// fails to parse (`ReadsAsQuestion`/`NoOrdinals`) used to drop it with
/// nothing said, leaving the operator unable to tell it was still on the
/// table.
#[test]
fn reads_as_question_refusal_mentions_an_outstanding_proposal() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");
    assert!(proposed.proposed_answer_notice().is_some());

    let resolved = proposed.resolve_with_operator_answer("yes?");
    let explanation = resolved
        .last_rejection_explanation()
        .expect("a reply that fails to parse must record why");
    assert!(explanation.contains("still waiting"), "{explanation}");
    assert!(
        explanation.contains("drain before rotation"),
        "{explanation}"
    );
}

/// The `NoOrdinals` shape of the same bug: a reply with no question mark and
/// no ordinal still silently drops the proposal today.
#[test]
fn no_ordinals_refusal_mentions_an_outstanding_proposal() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");

    let resolved = proposed.resolve_with_operator_answer("sounds fine");
    let explanation = resolved
        .last_rejection_explanation()
        .expect("a reply with no ordinal must record why");
    assert!(explanation.contains("still waiting"), "{explanation}");
}

/// With no proposal outstanding, the refusal is unchanged — no dangling
/// mention of a proposal that was never made.
#[test]
fn refusal_is_unchanged_with_no_proposal_outstanding() {
    let resolved = one_decision().resolve_with_operator_answer("drain before rotation");
    let explanation = resolved.last_rejection_explanation().unwrap();
    assert!(!explanation.contains("still waiting"), "{explanation}");
    assert_eq!(
        explanation,
        resolved.last_rejection().unwrap().explain(),
        "no proposal outstanding means the explanation is exactly the rejection's own text"
    );
}

/// A proposal that WAS confirmed leaves nothing pending, so the explanation
/// after it is `None`, same as any other locked, non-rejected reply.
#[test]
fn confirmed_proposal_locks_and_leaves_no_rejection() {
    let proposed = one_decision().propose_answer(1, "drain before rotation");
    let resolved = proposed.resolve_with_operator_answer("yes");
    assert!(resolved.last_rejection_explanation().is_none());
}

/// Addendum item 6: locking a decision prints one line naming what locked,
/// keyed to the ordinal the operator answered against.
#[test]
fn locking_a_decision_reports_which_one_locked() {
    let batch = one_decision();
    let resolved = batch.resolve_with_operator_answer("1: drain before rotation");
    let lines = resolved.newly_locked_lines(&batch);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("locked 1: "), "{}", lines[0]);
}

/// A refused reply locks nothing, so nothing is reported as newly locked.
#[test]
fn a_refused_reply_reports_no_newly_locked_lines() {
    let batch = one_decision();
    let resolved = batch.resolve_with_operator_answer("not sure");
    assert!(resolved.newly_locked_lines(&batch).is_empty());
}

/// #2515 items 5/6 round 2 (RED FIRST — failed before the fix: the line
/// echoed the QUESTION even when an explicit `N: value` reply supplied a
/// different ANSWER). One arm: an explicit `N: value` reply names the value,
/// not the question, when the two differ.
#[test]
fn locking_an_explicit_answer_names_the_value_not_the_question() {
    let batch = one_decision();
    let resolved = batch.resolve_with_operator_answer("1: keep the index");
    let lines = resolved.newly_locked_lines(&batch);
    assert_eq!(lines, vec!["locked 1: keep the index".to_string()]);
}

/// A bare `1:` with no value text has nothing to name — falls back to the
/// question, same as before this change.
#[test]
fn locking_a_bare_ordinal_falls_back_to_the_question() {
    let batch = one_decision();
    let resolved = batch.resolve_with_operator_answer("1:");
    let lines = resolved.newly_locked_lines(&batch);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("drain before rotation, or stamp actions"),
        "a bare ordinal must fall back to the question text: {}",
        lines[0]
    );
}

/// The other arm: a confirmed proposal names the proposal's summary, not the
/// question.
#[test]
fn locking_a_confirmed_proposal_names_the_proposal_not_the_question() {
    let batch = one_decision();
    let proposed = batch.propose_answer(1, "keep the index");
    let resolved = proposed.resolve_with_operator_answer("yes");
    let lines = resolved.newly_locked_lines(&batch);
    assert_eq!(lines, vec!["locked 1: keep the index".to_string()]);
}
