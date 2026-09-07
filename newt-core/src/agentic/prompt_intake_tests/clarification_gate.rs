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
