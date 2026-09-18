use super::*;
use newt_core::agentic::{PlanEntry, PromptDisposition};

/// #2424: an Act-disposition turn means the model called `enter_plan_mode`
/// mid-turn — the clamp is a LOCAL restriction layered on a turn already
/// validated for Act, so approval only needs to lift it, never re-validate
/// authority. This must hold regardless of the human-selected operating
/// mode, since the mode only decided what disposition INTAKE would have
/// picked absent a model override — the model's own mid-turn entry wins.
#[test]
fn act_disposition_is_always_model_during_act() {
    for mode in [
        OperatingMode::Chat,
        OperatingMode::Dev,
        OperatingMode::Admin,
        OperatingMode::Plan,
        OperatingMode::Diagnose,
        OperatingMode::Auto,
        OperatingMode::FullAuto,
    ] {
        assert_eq!(
            plan_entry_for_turn(PromptDisposition::Act, mode),
            PlanEntry::ModelDuringAct,
            "an Act-disposition turn is always ModelDuringAct, regardless of {mode:?}"
        );
    }
}

/// A Plan-disposition turn while the operator has `/mode plan` selected is
/// the OperatorSelected case — the operator explicitly asked for this, as
/// opposed to intake inferring it on its own.
#[test]
fn plan_disposition_under_mode_plan_is_operator_selected() {
    assert_eq!(
        plan_entry_for_turn(PromptDisposition::Plan, OperatingMode::Plan),
        PlanEntry::OperatorSelected
    );
}

/// The originally observed bug case: a Plan-disposition turn the operator
/// never asked for (their selected mode is something else entirely) means
/// intake inferred Plan on its own.
#[test]
fn plan_disposition_under_any_other_mode_is_intake_inferred() {
    for mode in [
        OperatingMode::Chat,
        OperatingMode::Dev,
        OperatingMode::Admin,
        OperatingMode::Diagnose,
        OperatingMode::Auto,
        OperatingMode::FullAuto,
    ] {
        assert_eq!(
            plan_entry_for_turn(PromptDisposition::Plan, mode),
            PlanEntry::IntakeInferred,
            "Plan disposition under {mode:?} (not /mode plan) is intake-inferred"
        );
    }
}

/// Every non-Act, non-Plan disposition (Ask/Explain/Research) is not
/// expected to reach the approval hook at all in practice — the local
/// mid-turn promotion only fires from Act, and intake only narrows to Plan
/// itself, never to these. But the classifier is total, so an unexpected
/// disposition must still fail SAFE: it is treated as needing the full
/// seeded-turn approval path (IntakeInferred), never as a same-turn resume
/// (which would be the wrong direction to fail toward, since it would let a
/// misclassified turn keep going instead of stopping for a fresh look).
#[test]
fn unexpected_dispositions_fail_safe_to_intake_inferred() {
    for disposition in [
        PromptDisposition::Ask,
        PromptDisposition::Explain,
        PromptDisposition::Research,
    ] {
        assert_eq!(
            plan_entry_for_turn(disposition, OperatingMode::Chat),
            PlanEntry::IntakeInferred,
            "{disposition:?} must fail safe, never ModelDuringAct"
        );
    }
}

/// #2424 regression: the guard must fire on the turn's own disposition, not
/// only the model-entered flag — an intake-inferred Plan turn (the original
/// bug report) never sets that flag, so keying on it alone silently skipped
/// the exact case the design exists for.
#[test]
fn a_plan_turn_that_presented_a_draft_is_asked_even_without_the_model_flag() {
    assert!(plan_approval_due(
        PromptDisposition::Plan,
        /* model_entered_plan */ false,
        /* exit_requested */ false,
        /* presented_draft */ true,
    ));
}

/// A Plan turn that drafted nothing (it answered a question, say) has no
/// plan to approve and is not asked.
#[test]
fn a_plan_turn_with_no_draft_is_not_asked() {
    assert!(!plan_approval_due(
        PromptDisposition::Plan,
        false,
        false,
        /* presented_draft */ false,
    ));
}

/// The model-entered case: an Act turn where the model called
/// `enter_plan_mode` and drafted is asked; an ordinary Act turn that never
/// touched Plan mode is never interrupted.
#[test]
fn model_entered_plan_with_a_draft_is_asked_and_a_plain_act_turn_is_not() {
    assert!(plan_approval_due(
        PromptDisposition::Act,
        /* model_entered_plan */ true,
        false,
        /* presented_draft */ true,
    ));
    assert!(!plan_approval_due(
        PromptDisposition::Act,
        false,
        false,
        false
    ));
}

/// An explicit `exit_plan_mode` request always reaches the operator — the
/// clamp lift itself is what needs a human, draft or no draft.
#[test]
fn an_exit_request_is_always_asked() {
    assert!(plan_approval_due(
        PromptDisposition::Act,
        false,
        /* exit_requested */ true,
        /* presented_draft */ false,
    ));
    assert!(plan_approval_due(
        PromptDisposition::Plan,
        false,
        true,
        false
    ));
}

/// A non-plan disposition is never a plan turn, whatever else is set —
/// `render_report` only stores a draft under Plan, so this state is not
/// reachable, but the predicate must still say no rather than assume.
#[test]
fn a_non_plan_disposition_is_not_asked_without_an_exit_request() {
    for disposition in [
        PromptDisposition::Ask,
        PromptDisposition::Explain,
        PromptDisposition::Research,
    ] {
        assert!(
            !plan_approval_due(disposition, false, false, true),
            "{disposition:?} is not a plan turn"
        );
    }
}

/// #2424: the seeded implementation turn carries the approved draft when
/// there is one, and ALWAYS carries the tenacity exit guidance — that
/// guidance used to be `exit_plan_mode`'s own ack; this is the turn it now
/// belongs to.
#[test]
fn approval_seed_text_carries_the_plan_when_present_and_the_guidance_always() {
    let with_plan = plan_approval_seed_text(Some("# Refactor\n1. step"), "GUIDANCE");
    // The coordinate: every seeded turn starts with the registered prefix,
    // so the compaction boundary classifier recognizes it as harness-owned.
    assert!(
        with_plan.starts_with(newt_core::agentic::PLAN_APPROVAL_PREFIX),
        "{with_plan}"
    );
    assert!(with_plan.contains("Implement it now"), "{with_plan}");
    assert!(with_plan.contains("# Refactor\n1. step"), "{with_plan}");
    assert!(with_plan.contains("GUIDANCE"), "{with_plan}");

    let without = plan_approval_seed_text(None, "GUIDANCE");
    assert!(without.contains("Continue the task"), "{without}");
    assert!(!without.contains("Implement it now"), "{without}");
    assert!(without.contains("GUIDANCE"), "{without}");
}

/// The seeded turn resumes as Act by the operator's decision — even though
/// its text quotes a plan, which a fresh intake analysis could read as Plan
/// and re-clamp the very turn that was just approved.
#[test]
fn approval_intake_resumes_as_act_even_when_the_text_quotes_a_plan() {
    let lexicon = newt_core::agentic::DispositionLexicon::default();
    let text = plan_approval_seed_text(
        Some("# Plan\n1. Plan the refactor of the parser\n2. Outline the steps"),
        "guidance",
    );
    assert_eq!(
        plan_approval_intake(&text, &lexicon).disposition(),
        PromptDisposition::Act
    );
}
