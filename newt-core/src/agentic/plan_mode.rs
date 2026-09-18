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
