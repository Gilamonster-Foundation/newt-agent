use super::*;

#[test]
fn ambiguous_destructive_pronoun_requires_clarification() {
    let ambiguous = PromptIntake::analyze("Delete it.");
    assert_eq!(ambiguous.disposition(), PromptDisposition::Ask);

    let grounded = PromptIntake::analyze("Delete scratch/obsolete.txt.");
    assert_eq!(grounded.disposition(), PromptDisposition::Act);
}

#[test]
fn negated_enumeration_is_not_an_ambiguous_decision() {
    // #1707 field regression: a live session hung at the clarification
    // gate on a scoped task prompt containing this exact scope-discipline
    // bullet. `needs_operator_decision`'s `" or " + trigger-word` branch
    // (meant for "should we use X or Y") cannot tell that from "Do NOT
    // implement A, B, or C" — a prohibition over a list, not a choice.
    let prompt = "Do NOT implement execution lifecycle, Brush streaming, \
         RemoteFence, OpenShell, or Wyvern integration here.";
    let intake = PromptIntake::analyze(prompt);
    assert_eq!(
        intake.disposition(),
        PromptDisposition::Act,
        "a negated enumeration must not block execution on a bogus decision lock: {:#?}",
        intake.manifest().decisions()
    );
    assert_eq!(intake.manifest().pending_decision_count(), 0);

    // Sibling phrasings from the same field prompt must stay unaffected.
    let also_negated = PromptIntake::analyze(
        "Do not create a second proxy supervisor in execution, \
         tool-shell, Wyvern, or transport code.",
    );
    assert_eq!(also_negated.disposition(), PromptDisposition::Act);

    let never_form = PromptIntake::analyze("Never implement caching or memoization in this layer.");
    assert_eq!(never_form.disposition(), PromptDisposition::Act);

    // A genuine ambiguous choice — no negation — must still be caught.
    let genuine = PromptIntake::analyze("Should we use SQLite or Postgres for the cache?");
    assert_eq!(genuine.disposition(), PromptDisposition::Ask);
}

/// #1708: `is_directive_prohibition` must recognize a negation wrapped
/// in a politeness filler, a second-person subject, or a "Label: "
/// prefix — not just a bare clause-initial cue.
#[test]
fn wrapped_prohibitions_still_produce_zero_pending_decisions() {
    for prompt in [
        "Please do not use A or B.",
        "You must not implement A or B.",
        "Constraint: do not implement A or B.",
    ] {
        let intake = PromptIntake::analyze(prompt);
        assert_eq!(
            intake.manifest().pending_decision_count(),
            0,
            "{prompt:?} must not be read as an operator decision: {:#?}",
            intake.manifest().decisions()
        );
        assert_eq!(intake.disposition(), PromptDisposition::Act, "{prompt:?}");
    }
}

/// #1708: a negated auxiliary that is actually a QUESTION must remain a
/// blocking decision — the `?` guard in `is_directive_prohibition`
/// exists precisely so "shouldn't" is not read as the same mood as
/// "should not" (a genuine field risk: `NEGATION_CUES` includes
/// `"shouldn't "`, and without the guard this exact prompt would have
/// been wrongly suppressed).
#[test]
fn interrogative_negation_remains_a_blocking_decision() {
    let hostile = PromptIntake::analyze(
        "Shouldn't we use SQLite or Postgres for the cache? Implement the cache.",
    );
    assert_eq!(
        hostile.disposition(),
        PromptDisposition::Ask,
        "a genuine unresolved question must still block, not silently resolve: {:#?}",
        hostile.manifest().decisions()
    );
    assert!(hostile.manifest().pending_decision_count() >= 1);
}

/// #1708: `"does not "` / `"doesn't "` are indicative, not imperative,
/// and must NOT be treated as an automatically-resolved prohibition —
/// `is_directive_prohibition` leaves them alone entirely, so this
/// clause's classification is whatever the pre-existing `" or "` +
/// trigger-word heuristic already gave it (unchanged by #1707/#1708).
#[test]
fn indicative_negation_is_not_treated_as_a_directive_prohibition() {
    let indicative =
        PromptIntake::analyze("This module does not implement caching or memoization.");
    assert_eq!(
        indicative.disposition(),
        PromptDisposition::Ask,
        "a descriptive 'does not' statement is not a prohibition this classifier may \
         silently resolve — it is left to the pre-existing heuristic: {:#?}",
        indicative.manifest().decisions()
    );
}

/// #1708: extend prohibition reasoning to the `choose`/`select`/`pick`
/// needles — "Do not choose A or B" is a constraint, not a decision the
/// operator must lock, exactly like the `" or "` + trigger-word case.
#[test]
fn negated_choose_select_pick_are_constraints_not_decisions() {
    for prompt in ["Do not choose A or B.", "Never select A or B."] {
        let intake = PromptIntake::analyze(prompt);
        assert_eq!(
            intake.manifest().pending_decision_count(),
            0,
            "{prompt:?} is a constraint, not an operator decision: {:#?}",
            intake.manifest().decisions()
        );
        assert_eq!(intake.disposition(), PromptDisposition::Act, "{prompt:?}");
    }
}

/// #1708: the positive controls for the choose/select/pick and " or "
/// heuristics must still fire with no negation present.
#[test]
fn unnegated_choice_language_still_blocks() {
    let should_use = PromptIntake::analyze("Should we use SQLite or Postgres?");
    assert_eq!(should_use.disposition(), PromptDisposition::Ask);

    let choose = PromptIntake::analyze("Choose SQLite or Postgres.");
    assert_eq!(choose.disposition(), PromptDisposition::Ask);
}
