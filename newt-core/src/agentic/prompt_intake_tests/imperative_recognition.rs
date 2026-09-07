use super::*;

/// **The twin that bounds the blast radius, measured not asserted.**
///
/// All 22 of these reach `Act` ONLY through the terminal fallback — the
/// action lexicon is 17 needles wide and matches none of them. They are
/// what makes inverting that fallback the wrong fix, so every one of them
/// must still infer `Act` after the narrowing. A regression here means the
/// narrowing has started eating ordinary instructions.
#[test]
fn every_ordinary_imperative_still_infers_act() {
    for prompt in [
        "add a test for the parser",
        "update the docs",
        "remove the dead code",
        "refactor the tty module",
        "rename the field",
        "install the hooks",
        "upgrade serde",
        "bump the version",
        "revert that",
        "rebase onto main",
        "tag the release",
        "extract the helper",
        "wire it up",
        "port it to windows",
        "migrate the store",
        "continue",
        "proceed",
        "go ahead",
        "carry on",
        "clean that up",
        "split the file",
        "land it",
    ] {
        assert_eq!(
            PromptIntake::analyze(prompt).disposition(),
            PromptDisposition::Act,
            "{prompt:?} reaches Act only through the terminal fallback — \
             the #1971 narrowing must not touch it"
        );
    }
}

/// One instruction among statements is still an instruction. The narrowing
/// requires EVERY clause to state; a mixed prompt keeps `Act`, so an FYI
/// cannot be used to launder away the authority of the sentence beside it.
#[test]
fn a_mixed_prompt_keeps_act() {
    let intake = PromptIntake::analyze(&format!("{GIT_REMOTE_FYI}. add the CI workflow"));
    assert_eq!(intake.disposition(), PromptDisposition::Act);
    let kinds: Vec<AskKind> = intake.atomic_asks().iter().map(AtomicAsk::kind).collect();
    assert_eq!(
        kinds,
        vec![AskKind::Informational, AskKind::Instruction],
        "the clauses are classified separately: {:?}",
        intake.atomic_asks()
    );
    // …and the stated half still survives, even though the turn acts.
    assert!(intake.model_card().contains(GIT_REMOTE_FYI));
}

/// The two positive shapes, and the negatives that must NOT trip them.
/// A subject lead alone and a copula alone are each insufficient — both
/// halves are required — which is what keeps "make sure it is green" and
/// "the tests need updating" instructions.
#[test]
fn only_a_positive_statement_shape_is_informational() {
    for stated in [
        "fyi the remote moved",
        "btw we are on 0.8 now",
        "note that the CI runner is self-hosted",
        "i'll want a TUI eventually",
        "the parser is broken",
        "there is a bug in the parser",
    ] {
        assert_eq!(
            PromptIntake::analyze(stated).disposition(),
            PromptDisposition::Explain,
            "{stated:?} states a fact"
        );
    }
    for instructed in [
        // A copula with no subject lead: an imperative about a state.
        "make sure it is green",
        // A subject lead with no copula.
        "the tests need updating",
        // Bare `i want` is routinely an instruction and is deliberately
        // absent from the marker list.
        "i want you to fix the parser",
    ] {
        assert_eq!(
            PromptIntake::analyze(instructed).disposition(),
            PromptDisposition::Act,
            "{instructed:?} instructs"
        );
    }
}

/// **An FYI prefix cannot launder an instruction.** The action needles are
/// still checked FIRST and still win outright, so a marker at the start of
/// a clause cannot demote a recognised imperative sitting beside it.
///
/// This is why the informational test runs last rather than first: reversed,
/// "fyi …, fix it" would classify as a statement and lose the `fix`.
#[test]
fn an_fyi_prefix_cannot_launder_a_recognised_imperative() {
    assert_eq!(
        PromptIntake::analyze("fyi the parser is broken, fix it").disposition(),
        PromptDisposition::Act,
        "`fix` is an action needle and wins outright"
    );
}

/// **A known limitation, pinned rather than hidden.**
///
/// An UNRECOGNISED imperative comma-joined onto a marker-led clause is read
/// as part of the statement, because clause splitting is by line, `;` and
/// `. ` — not by comma — and widening it to commas would split "add a, b
/// and c" into three asks.
///
/// The result is `Explain`: the agent answers instead of acting. That is
/// the conservative direction of the same trade-off this whole change
/// makes — a visible lost round the operator recovers with one more
/// sentence, rather than an invisible unauthorized mutation — and the text
/// survives verbatim in the card, so the model sees the request and can
/// offer to do it.
#[test]
fn an_unrecognised_imperative_joined_to_an_fyi_by_a_comma_is_read_as_stated() {
    let intake = PromptIntake::analyze("fyi the parser is broken, tidy it up");
    assert_eq!(intake.disposition(), PromptDisposition::Explain);
    assert!(
        intake.model_card().contains("tidy it up"),
        "the request is not lost, only unauthorized: {}",
        intake.model_card()
    );
}

#[test]
fn quoted_command_test_question_is_an_action_turn() {
    // Field regression (2026-08-13): the trailing `?` used to win because
    // `test` was absent from the action lexicon. Explain then hid
    // `run_command` even though the operator explicitly asked Newt to
    // execute a quoted command under --yolo --full-access.
    let prompt = "you should have a \"gh\" command ... test \"gh auth status\" now to tell me if you can use it?";
    assert_eq!(
        PromptIntake::analyze(prompt).disposition(),
        PromptDisposition::Act
    );

    // Merely discussing tests remains an Explain deliverable; the narrow
    // quoted-command needle must not turn ordinary test prose into Act.
    assert_eq!(
        PromptIntake::analyze("Explain how the test harness works?").disposition(),
        PromptDisposition::Explain
    );
    assert_eq!(
        PromptIntake::analyze("What is the latest \"release\"?").disposition(),
        PromptDisposition::Explain
    );
    assert_eq!(
        PromptIntake::analyze("Explain the greatest 'risk'?").disposition(),
        PromptDisposition::Explain
    );
    assert_eq!(
        PromptIntake::analyze("test \"gh auth status\" now").disposition(),
        PromptDisposition::Act
    );
}
