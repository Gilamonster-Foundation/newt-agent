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
/// there is one, and ALWAYS carries the initiative exit guidance — that
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

// ---------------------------------------------------------------------------
// Composed approval-to-dispatch: a scripted gate drives `run_plan_approval`
// against real `PlanModeState` / `PlanDraftState`, and the queued turn is
// consumed through `PendingPlanTurn::into_input` exactly as the loop head
// does. Acceptance question: does exactly the human-authorized work proceed
// once, with the correct objective, draft, lineage, and authority?
// ---------------------------------------------------------------------------

use newt_core::agentic::{
    HumanQuestionOutcome, PermissionDecision, PermissionGate, PermissionRequest,
    PlanDraftSink as _, PlanModeControl as _,
};
use newt_core::Caveats;

/// Answers each question with the next scripted outcome; records the
/// questions asked. `ask`/`refresh_caveats` must never be reached by the
/// approval path — approval is a question, not a permission grant.
struct ScriptedGate {
    outcomes: Vec<HumanQuestionOutcome>,
    asked: Vec<String>,
}

impl ScriptedGate {
    fn new(outcomes: impl IntoIterator<Item = HumanQuestionOutcome>) -> Self {
        let mut outcomes: Vec<_> = outcomes.into_iter().collect();
        outcomes.reverse();
        Self {
            outcomes,
            asked: Vec::new(),
        }
    }
}

impl PermissionGate for ScriptedGate {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        panic!("plan approval must never request a permission grant");
    }
    fn refresh_caveats(&mut self, _: &Caveats) -> PermissionDecision {
        panic!("plan approval must never re-mint caveats");
    }
    fn ask_question(&mut self, question: &str) -> HumanQuestionOutcome {
        self.asked.push(question.to_string());
        self.outcomes
            .pop()
            .expect("a scripted outcome per question")
    }
}

fn answer(text: &str) -> HumanQuestionOutcome {
    HumanQuestionOutcome::Answer(text.to_string())
}

fn objective(text: &str) -> newt_core::TurnPromptContext {
    newt_core::TurnPromptContext::ephemeral_operator("conv-1", text, text)
}

/// A model-entered plan turn under `objective` that saved and presented
/// `markdown`: the state the turn-end hook sees right before asking.
fn presented_plan(
    states: &ConversationModeStates,
    objective: &newt_core::TurnPromptContext,
    markdown: &str,
) -> newt_core::agentic::PresentedPlan {
    states.plan.set_plan_mode(true).unwrap();
    let noop = |_: &std::path::Path, _: &str| Ok(());
    states
        .plan_draft
        .bind("conv-1", plan_objective(Some(objective)), &noop)
        .save_draft(markdown.to_string())
        .unwrap();
    states
        .plan_draft
        .take_for_presentation(plan_objective(Some(objective)).unwrap())
        .expect("presented")
}

fn approve(
    gate: &mut ScriptedGate,
    states: &ConversationModeStates,
    entry: PlanEntry,
    parent: Option<&newt_core::TurnPromptContext>,
) -> PlanApprovalEffects {
    run_plan_approval(
        Some(gate),
        entry,
        &states.plan,
        &states.plan_draft,
        parent,
        "GUIDANCE",
    )
}

/// "y" → exactly one harness-origin continuation, carrying the presented
/// draft, parented to the objective it was drafted under; the clamp lifts.
#[test]
fn approval_queues_exactly_one_harness_turn_with_the_presented_plan() {
    let states = ConversationModeStates::default();
    let a = objective("refactor the parser");
    let shown = presented_plan(&states, &a, "# plan A");
    let mut gate = ScriptedGate::new([answer("y")]);

    let effects = approve(&mut gate, &states, PlanEntry::ModelDuringAct, Some(&a));

    assert_eq!(gate.asked, vec!["Approve this plan? [y/N/discuss] "]);
    assert!(!states.plan.is_plan_mode(), "clamp lifted");
    assert!(!effects.switch_to_dev);
    let (input, origin) = effects.queued.expect("one continuation").into_input();
    let ReadOutcome::Line(text) = input else {
        panic!("a line of input")
    };
    assert!(text.starts_with(newt_core::agentic::PLAN_APPROVAL_PREFIX));
    assert!(text.contains(&shown.draft.markdown), "{text}");
    assert!(text.contains("GUIDANCE"));
    match origin {
        ModelInputOrigin::HarnessPlanApproval { parent } => {
            assert_eq!(
                parent.active().root_prompt_id(),
                a.active().root_prompt_id()
            );
        }
        other => panic!("harness origin expected, got {other:?}"),
    }
    assert!(
        states
            .plan_draft
            .take_approved(a.active().root_prompt_id())
            .is_none(),
        "the snapshot is consumed by the one approval"
    );
}

/// Reject, empty answer, cancel, exit, closed input, no operator, no gate:
/// none of these queue anything, and the clamp stays. No gate outcome other
/// than an explicit "yes" widens what the model may do.
#[test]
fn every_non_approval_outcome_queues_nothing_and_keeps_the_clamp() {
    let outcomes = [
        answer("n"),
        answer(""),
        HumanQuestionOutcome::Cancelled,
        HumanQuestionOutcome::ExitRequested,
        HumanQuestionOutcome::InputClosed,
        HumanQuestionOutcome::Unavailable,
    ];
    for outcome in outcomes {
        let states = ConversationModeStates::default();
        let a = objective("A");
        presented_plan(&states, &a, "# plan A");
        let mut gate = ScriptedGate::new([outcome.clone()]);
        let effects = approve(&mut gate, &states, PlanEntry::ModelDuringAct, Some(&a));
        assert!(effects.queued.is_none(), "{outcome:?} queued a turn");
        assert!(states.plan.is_plan_mode(), "{outcome:?} lifted the clamp");
        assert!(!effects.switch_to_dev);
        assert!(
            states
                .plan_draft
                .take_approved(a.active().root_prompt_id())
                .is_some(),
            "{outcome:?} must leave the snapshot for a later approval"
        );
    }
    // No gate at all.
    let states = ConversationModeStates::default();
    let a = objective("A");
    presented_plan(&states, &a, "# plan A");
    let effects = run_plan_approval(
        None,
        PlanEntry::ModelDuringAct,
        &states.plan,
        &states.plan_draft,
        Some(&a),
        "GUIDANCE",
    );
    assert!(effects.queued.is_none());
    assert!(states.plan.is_plan_mode());
}

/// "discuss" text → one OPERATOR-origin continuation of the same lineage,
/// the clamp stays, and the presented snapshot survives so the eventual
/// "y" (with no new draft) seeds exactly what the operator saw.
#[test]
fn discussion_preserves_lineage_clamp_and_snapshot_then_approval_seeds_it() {
    let states = ConversationModeStates::default();
    let a = objective("A");
    let shown = presented_plan(&states, &a, "# plan A");
    let mut gate = ScriptedGate::new([answer("why step 2?"), answer("yes")]);

    let effects = approve(&mut gate, &states, PlanEntry::ModelDuringAct, Some(&a));
    assert!(states.plan.is_plan_mode(), "discussion keeps the clamp");
    let (input, origin) = effects.queued.expect("the discussion turn").into_input();
    assert!(matches!(input, ReadOutcome::Line(ref t) if t == "why step 2?"));
    let ModelInputOrigin::OperatorContinuation { parent } = origin else {
        panic!("operator-authored text keeps an operator origin")
    };
    assert_eq!(
        parent.active().root_prompt_id(),
        a.active().root_prompt_id()
    );

    // The discussion turn ran as a continuation (same root) and saved no
    // new draft; the operator now says yes.
    let discuss =
        newt_core::TurnPromptContext::ephemeral_operator_continuation("conv-1", "d", "d", &a)
            .unwrap();
    assert!(states
        .plan_draft
        .take_for_presentation(discuss.active().root_prompt_id())
        .is_none());
    let effects = approve(
        &mut gate,
        &states,
        PlanEntry::ModelDuringAct,
        Some(&discuss),
    );
    let (input, _) = effects
        .queued
        .expect("the implementation turn")
        .into_input();
    let ReadOutcome::Line(text) = input else {
        panic!()
    };
    assert!(text.contains(&shown.draft.markdown), "{text}");
    assert!(!states.plan.is_plan_mode());
}

/// The steer's binding case end to end: plan A presented; unrelated
/// objective B ends with `exit_plan_mode` and no B draft; the operator
/// approves. The seeded turn carries NO plan (A's must not become B's
/// input) and is parented to B, and A's snapshot remains A's.
#[test]
fn approval_under_objective_b_never_seeds_objective_a_plan() {
    let states = ConversationModeStates::default();
    let a = objective("A");
    presented_plan(&states, &a, "# plan A");

    let b = objective("B");
    states.plan.set_plan_mode(true).unwrap();
    states.plan.request_exit().unwrap();
    assert!(states
        .plan_draft
        .take_for_presentation(b.active().root_prompt_id())
        .is_none());
    let mut gate = ScriptedGate::new([answer("y")]);
    let effects = approve(&mut gate, &states, PlanEntry::ModelDuringAct, Some(&b));

    let (input, origin) = effects.queued.expect("B's continuation").into_input();
    let ReadOutcome::Line(text) = input else {
        panic!()
    };
    assert!(!text.contains("# plan A"), "A leaked into B: {text}");
    assert!(text.contains("Continue the task"), "{text}");
    let ModelInputOrigin::HarnessPlanApproval { parent } = origin else {
        panic!()
    };
    assert_eq!(
        parent.active().root_prompt_id(),
        b.active().root_prompt_id()
    );
    assert!(
        states
            .plan_draft
            .take_approved(a.active().root_prompt_id())
            .is_some(),
        "A's snapshot is untouched by B's approval"
    );
}

/// A second `exit_plan_mode` + "y" after an approval cannot re-seed the
/// same plan: the snapshot was consumed, so the second seed carries none.
#[test]
fn a_repeated_exit_cannot_consume_the_approval_twice() {
    let states = ConversationModeStates::default();
    let a = objective("A");
    presented_plan(&states, &a, "# plan A");
    let mut gate = ScriptedGate::new([answer("y"), answer("y")]);

    let first = approve(&mut gate, &states, PlanEntry::ModelDuringAct, Some(&a));
    let (ReadOutcome::Line(first_text), _) = first.queued.unwrap().into_input() else {
        panic!()
    };
    assert!(first_text.contains("# plan A"));

    states.plan.set_plan_mode(true).unwrap();
    states.plan.request_exit().unwrap();
    let second = approve(&mut gate, &states, PlanEntry::ModelDuringAct, Some(&a));
    let (ReadOutcome::Line(second_text), _) = second.queued.unwrap().into_input() else {
        panic!()
    };
    assert!(
        !second_text.contains("# plan A"),
        "the approved snapshot must not be replayed: {second_text}"
    );
}

/// `/mode plan` (OperatorSelected): the question names the mode switch and
/// approval reports it; the loop applies the process-global write.
#[test]
fn operator_selected_approval_asks_about_the_mode_switch_and_requests_it() {
    let states = ConversationModeStates::default();
    let a = objective("A");
    presented_plan(&states, &a, "# plan A");
    let mut gate = ScriptedGate::new([answer("y")]);
    let effects = approve(&mut gate, &states, PlanEntry::OperatorSelected, Some(&a));
    assert_eq!(
        gate.asked,
        vec!["Approve this plan and switch to /mode dev? [y/N/discuss] "]
    );
    assert!(effects.switch_to_dev);
    assert!(effects.queued.is_some());
}

/// No receipt for the turn (fails safe): approval lifts the clamp but
/// queues nothing — the operator's own next message starts implementation,
/// so no harness turn runs without a lineage to attach to.
#[test]
fn approval_without_a_receipt_queues_nothing() {
    let states = ConversationModeStates::default();
    states.plan.set_plan_mode(true).unwrap();
    let mut gate = ScriptedGate::new([answer("y")]);
    let effects = approve(&mut gate, &states, PlanEntry::ModelDuringAct, None);
    assert!(effects.queued.is_none());
    assert!(!states.plan.is_plan_mode());
    assert!(effects.notice.contains("Send a message"));
}

/// `exit_plan_mode` only REQUESTS: the flag is set, the clamp is untouched,
/// and the request is consumed by exactly one turn-end read.
#[test]
fn exit_plan_mode_only_requests_and_the_request_is_read_once() {
    let states = ConversationModeStates::default();
    states.plan.set_plan_mode(true).unwrap();
    states.plan.request_exit().unwrap();
    assert!(
        states.plan.is_plan_mode(),
        "request does not lift the clamp"
    );
    assert!(states.plan.take_exit_requested());
    assert!(!states.plan.take_exit_requested(), "consumed once");
    assert!(states.plan.is_plan_mode());
}

/// A queued continuation from before a boundary clear is stale: the queue
/// is a single slot the loop head drains on the very next iteration, and
/// `ConversationModeStates::clear()` (new/restore/persona) drops the draft
/// and snapshot, so nothing survives for a later objective to run.
#[test]
fn boundary_clear_leaves_no_snapshot_for_a_later_approval() {
    let states = ConversationModeStates::default();
    let a = objective("A");
    presented_plan(&states, &a, "# plan A");
    states.clear();
    assert!(!states.plan.is_plan_mode());
    assert!(states
        .plan_draft
        .take_approved(a.active().root_prompt_id())
        .is_none());
    let noop = |_: &std::path::Path, _: &str| Ok(());
    assert!(states
        .plan_draft
        .bind("conv-1", plan_objective(Some(&a)), &noop)
        .latest_draft()
        .is_none());
}

/// Recording presenter: the pane-facing render the loop performs and the
/// snapshot it prints are the same bytes — `ToolDisplay::document` is a
/// `writeln!`+flush, so rendering the snapshot's markdown directly is
/// contract-equivalent; this pins that the presented text is the draft.
#[test]
fn the_presented_snapshot_is_the_saved_markdown_byte_for_byte() {
    let states = ConversationModeStates::default();
    let a = objective("A");
    let shown = presented_plan(&states, &a, "# Title\n\n1. step one\n");
    assert_eq!(shown.draft.markdown, "# Title\n\n1. step one\n");
    assert_eq!(shown.draft.revision, 1);
}
