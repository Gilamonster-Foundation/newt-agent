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
