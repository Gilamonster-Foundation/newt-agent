//! Session-local control for the model-entered Plan phase.
//!
//! Core owns the `enter_plan_mode` / `exit_plan_mode` tools, but the embedding
//! session owns their state. Keeping that state behind an injected collaborator
//! prevents one TUI session (or one concurrent test) from changing another.

/// Session-local state behind `enter_plan_mode` and `exit_plan_mode`.
///
/// Entering Plan can only attenuate the active turn. The dispatcher consults
/// [`Self::is_plan_mode`] before every tool call, so a successful enter takes
/// effect immediately for later calls in the same model tool round.
pub trait PlanModeControl: Send + Sync {
    /// Whether the model-entered Plan phase is currently active.
    fn is_plan_mode(&self) -> bool;

    /// Enter or leave the model-entered Plan phase.
    ///
    /// Implementations should update only state owned by the current session.
    fn set_plan_mode(&self, active: bool) -> Result<(), String>;

    /// #2424: the model called `exit_plan_mode` this round, asking for the
    /// clamp to be lifted. Does NOT lift it — recorded separately from
    /// [`Self::is_plan_mode`] so calls made between the request and the
    /// turn's end still see the clamp. Only the turn-end approval hook may
    /// actually call `set_plan_mode(false)`.
    fn request_exit(&self) -> Result<(), String>;

    /// Whether an approval request is pending, without consuming it.
    ///
    /// Provider loops use this after recording a tool batch to yield to the
    /// caller's approval hook. The clamp and request stay intact until that
    /// hook handles the operator's answer. Legacy embedders that do not expose
    /// this query retain their existing turn-end handoff behavior.
    fn exit_requested(&self) -> bool {
        false
    }

    /// Take-and-clear whether [`Self::request_exit`] was called since the
    /// last take. The turn-end hook calls this once, at most, per turn.
    fn take_exit_requested(&self) -> bool;

    /// Whether the current turn is the implementing turn an approval
    /// seeded. The executor's plan-before-act arm does not ask about a plan
    /// the operator just approved when the model records it in the ledger.
    /// Embedders that never seed such a turn keep the default.
    fn implementing_approved_plan(&self) -> bool {
        false
    }
}

/// One revision of the model's in-progress Plan-phase draft.
///
/// design: `docs/design/plan-mode-draft-present-approve.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDraft {
    /// Monotonic per-session revision number, starting at 1.
    pub revision: u32,
    /// The composed Markdown `render_report` would otherwise have displayed.
    pub markdown: String,
}

/// The exact draft the operator was shown, identified by its content — the
/// snapshot an approval binds to. Approval seeds implementation from THIS,
/// never from whatever the mutable draft slot holds later, so a draft saved
/// after presentation (or under another objective) cannot ride an earlier
/// "yes". The id is `RawContentId` over the Markdown bytes (a `PlanDraft` is
/// an opaque byte payload here, not a structured record).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedPlan {
    pub draft: PlanDraft,
    pub id: content_addressable::RawContentId,
}

impl PresentedPlan {
    pub fn new(draft: PlanDraft) -> Self {
        let id = content_addressable::RawContentId::from_content(draft.markdown.as_bytes());
        Self { draft, id }
    }
}

/// Session-local sink for the model's in-progress Plan-phase draft.
///
/// Under the Plan disposition, `render_report` replaces this draft instead of
/// printing it — one draft slot, not a growing transcript of near-identical
/// displays. Implementations own where the draft is persisted (e.g. the
/// session's `plan.md`); newt-core gains no filesystem authority from this
/// trait, and the model's own clamp is untouched because the harness does the
/// write.
pub trait PlanDraftSink: Send + Sync {
    /// Replace the draft with `markdown`. Returns the new revision number
    /// (the prior revision plus one; the first save is revision 1).
    fn save_draft(&self, markdown: String) -> Result<u32, String>;

    /// The latest saved draft this session, if any `render_report` call has
    /// landed one yet. Used to present exactly once at the end of a planning
    /// turn — never polled mid-turn to decide model-facing behavior.
    fn latest_draft(&self) -> Option<PlanDraft>;
}

/// How the model-entered Plan phase now clamping the turn was reached.
/// Decides what an approval may do (design:
/// `docs/design/plan-mode-draft-present-approve.md`, #2424) — the caveats a
/// turn started with are the ceiling an approval inside it may restore, never
/// widen past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanEntry {
    /// The model called `enter_plan_mode` inside an already-validated Act
    /// turn. The turn's own Act disposition was checked at turn start; the
    /// Plan clamp is a LOCAL, later-arriving restriction on top of it.
    /// Approval can simply lift the local clamp and let the same turn
    /// continue — it restores authority the turn already held, mints none.
    ModelDuringAct,
    /// Intake resolved this turn as Plan from the start (the observed bug
    /// case): the turn's own caveats were met with `plan_phase_clamp()` at
    /// turn start, so there is no wider Act disposition to fall back into.
    /// Approval must end this turn and seed a fresh Act turn with the
    /// approved plan — lifting the clamp mid-turn here would mint authority
    /// the turn was never validated for.
    IntakeInferred,
    /// The operator explicitly set `/mode plan`. Approval offers switching
    /// to `/mode dev` as the operator's own act, then proceeds exactly like
    /// [`Self::IntakeInferred`].
    OperatorSelected,
}

impl PlanEntry {
    /// Whether an approval may resume the SAME turn (true only when that
    /// turn's own disposition was already Act — see [`Self::ModelDuringAct`]).
    #[must_use]
    pub fn resumes_same_turn(self) -> bool {
        matches!(self, Self::ModelDuringAct)
    }
}

/// What an approval question resolved to. Never mints authority — see
/// [`PlanEntry`]'s own doc for why the two variants differ in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanVerdict {
    /// Approved. [`PlanEntry::resumes_same_turn`] on the entry that produced
    /// this verdict says whether the caller may continue the current turn or
    /// must end it and seed a fresh Act turn with the approved plan.
    Approved,
    /// Stay clamped read-only. `feedback`, when present, is the operator's
    /// own text (a rejection reason, a "discuss" follow-up, or anything
    /// else typed instead of an unambiguous yes/no) — fed back as the next
    /// operator prompt. `None` means a plain no, or no operator answer was
    /// obtained at all (unavailable/cancelled/exit/closed/failed all fail
    /// closed to this, per [`HumanQuestionOutcome`]'s own variants).
    StayClamped { feedback: Option<String> },
}

/// The three answers a plan question offers, in presentation order. `yes`
/// is the default for a blank submission (`[Y/n/discuss]`). Cancellation or
/// unavailable input is not a submission and never approves. `discuss` opens
/// a free-text follow-up whose text is fed back to the model.
pub const PLAN_APPROVAL_CHOICES: &[super::permissions::ChoiceSpec] = &[
    super::permissions::ChoiceSpec {
        id: "yes",
        label: "yes — implement this plan",
        key: "y",
        role: newt_interaction::SemanticRole::Allow,
    },
    super::permissions::ChoiceSpec {
        id: "no",
        label: "no — stay in plan mode",
        key: "n",
        role: newt_interaction::SemanticRole::Deny,
    },
    super::permissions::ChoiceSpec {
        id: "discuss",
        label: "discuss — tell the model what to change",
        key: "d",
        role: newt_interaction::SemanticRole::Value,
    },
];

/// Map a submitted answer to approval, decline or verbatim feedback.
/// Blank, `y`/`yes`, and bare continuation answers approve; `n`/`no` declines.
/// Non-answer outcomes always retain the clamp, regardless of the default.
#[must_use]
pub fn plan_verdict(
    _entry: PlanEntry,
    outcome: super::permissions::HumanQuestionOutcome,
) -> PlanVerdict {
    use super::permissions::HumanQuestionOutcome;
    let HumanQuestionOutcome::Answer(text) = outcome else {
        // Unavailable, Cancelled, ExitRequested, InputClosed, InputFailed —
        // none of them carry an answer, so none of them can approve.
        return PlanVerdict::StayClamped { feedback: None };
    };
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("y")
        || trimmed.eq_ignore_ascii_case("yes")
    {
        return PlanVerdict::Approved;
    }
    // "go", "do it", "proceed", "ship it": the operator's natural yes at a
    // plan question. The one continuation table decides what counts.
    if crate::classifiers::is_bare_continuation(trimmed) {
        return PlanVerdict::Approved;
    }
    if trimmed.eq_ignore_ascii_case("n") || trimmed.eq_ignore_ascii_case("no") {
        return PlanVerdict::StayClamped { feedback: None };
    }
    PlanVerdict::StayClamped {
        feedback: Some(text),
    }
}

#[cfg(test)]
mod tests {
    use super::super::permissions::HumanQuestionOutcome;
    use super::*;

    /// The operator-selected default approves an actually submitted blank;
    /// missing input remains a separate, non-answer outcome.
    #[test]
    fn plan_default_yes_blank_submission_approves() {
        for entry in [
            PlanEntry::IntakeInferred,
            PlanEntry::ModelDuringAct,
            PlanEntry::OperatorSelected,
        ] {
            for text in ["", "   ", "\t\n"] {
                assert_eq!(
                    plan_verdict(entry, HumanQuestionOutcome::Answer(text.into())),
                    PlanVerdict::Approved
                );
            }
        }
    }

    /// Selection UIs select the first offered answer when Enter is pressed.
    #[test]
    fn plan_default_yes_is_the_first_choice() {
        assert_eq!(
            PLAN_APPROVAL_CHOICES
                .iter()
                .map(|choice| choice.id)
                .collect::<Vec<_>>(),
            ["yes", "no", "discuss"]
        );
    }

    #[test]
    fn yes_approves_case_insensitively_and_trimmed() {
        for text in ["y", "Y", "yes", "YES", "  y  ", "Yes"] {
            assert_eq!(
                plan_verdict(
                    PlanEntry::IntakeInferred,
                    HumanQuestionOutcome::Answer(text.to_string())
                ),
                PlanVerdict::Approved,
                "{text:?} must approve"
            );
        }
    }

    #[test]
    fn a_bare_continuation_at_the_question_approves() {
        for text in ["go", "do it", "proceed", "go ahead", "ship it", "Continue."] {
            assert_eq!(
                plan_verdict(
                    PlanEntry::ModelDuringAct,
                    HumanQuestionOutcome::Answer(text.to_string())
                ),
                PlanVerdict::Approved,
                "{text:?} must approve"
            );
        }
    }

    #[test]
    fn no_declines_with_no_feedback() {
        for text in ["n", "N", "no", "NO"] {
            assert_eq!(
                plan_verdict(
                    PlanEntry::IntakeInferred,
                    HumanQuestionOutcome::Answer(text.to_string())
                ),
                PlanVerdict::StayClamped { feedback: None },
                "{text:?} must decline with no feedback"
            );
        }
    }

    #[test]
    fn anything_else_typed_stays_clamped_and_is_fed_back_verbatim() {
        for text in ["discuss the caching step", "edit", "what about tests?"] {
            assert_eq!(
                plan_verdict(
                    PlanEntry::IntakeInferred,
                    HumanQuestionOutcome::Answer(text.to_string())
                ),
                PlanVerdict::StayClamped {
                    feedback: Some(text.to_string())
                },
                "{text:?} must be kept verbatim as feedback"
            );
        }
    }

    #[test]
    fn every_non_answer_outcome_fails_closed_to_stay_clamped() {
        for outcome in [
            HumanQuestionOutcome::Unavailable,
            HumanQuestionOutcome::Cancelled,
            HumanQuestionOutcome::ExitRequested,
            HumanQuestionOutcome::InputClosed,
            HumanQuestionOutcome::InputFailed,
        ] {
            for entry in [
                PlanEntry::IntakeInferred,
                PlanEntry::ModelDuringAct,
                PlanEntry::OperatorSelected,
            ] {
                assert_eq!(
                    plan_verdict(entry, outcome.clone()),
                    PlanVerdict::StayClamped { feedback: None }
                );
            }
        }
    }

    #[test]
    fn only_model_during_act_resumes_the_same_turn() {
        assert!(PlanEntry::ModelDuringAct.resumes_same_turn());
        assert!(!PlanEntry::IntakeInferred.resumes_same_turn());
        assert!(!PlanEntry::OperatorSelected.resumes_same_turn());
    }
}
