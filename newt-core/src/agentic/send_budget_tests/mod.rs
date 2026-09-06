// Re-exported, not a plain glob: a family's in-body `use super::X;` was
// written when `super` was `send_budget`, and a private glob binding is not
// nameable by path from a child module.
use super::super::compress::{compression_trigger, CompressionTriggerLimits};
pub(crate) use super::*;
use super::{initial_send_budget, num_ctx_input_ceiling, recovered_input_budget};
use crate::agentic::generation_policy::GenerationPolicy;
use crate::model_card::{ChatCompletionsCapability, ReasoningReplayScope};
use crate::role_profile::Cognition;
use crate::{BackendKind, CompactionTriggerPolicy, OpenAiApi};

// --- #1528 `ResponsesBudgetState`: one enforced budget, every reader ---

// --- #1534 "finish the single source of truth" — the two equalities ---

// Families beside this file. Both attributes are required: rustc needs only
// the `#[path]`, but the ratchets' shared scanner resolves a child ONLY when
// a `#[cfg(test)]` immediately precedes the `mod` (#2149).
#[cfg(test)]
#[path = "budget_state.rs"]
mod budget_state;
#[cfg(test)]
#[path = "calibrated_report.rs"]
mod calibrated_report;
#[cfg(test)]
#[path = "calibration.rs"]
mod calibration;
#[cfg(test)]
#[path = "compaction_target.rs"]
mod compaction_target;
#[cfg(test)]
#[path = "dispatch_agreement.rs"]
mod dispatch_agreement;
#[cfg(test)]
#[path = "seam.rs"]
mod seam;
#[cfg(test)]
#[path = "window_ceiling.rs"]
mod window_ceiling;
