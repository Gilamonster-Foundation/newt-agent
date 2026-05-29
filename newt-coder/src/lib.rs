//! newt-coder — coder plugin closing failure mode T0b.
//!
//! Background: see the authoritative knowledge card
//! `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`.
//!
//! The default `newt worker` (newt-flat) sends the operator's task
//! prompt verbatim with no file contents, then asks the model to emit
//! a unified diff. Local Ollama coder models reliably invent
//! plausible-but-wrong context lines under that contract — apply_patch
//! rejects the patch on context mismatch and the workspace diff stays
//! empty (failure mode T0b).
//!
//! newt-coder is the opinionated path: it scans the workspace for
//! files the task mentions, injects them verbatim into the user
//! message, and asks the model to emit each updated file in full
//! (no fences, no prose, no diffs). The reply is parsed by
//! `normalize_emission` and the changes are written to the workspace;
//! the caller then captures a real diff via `git diff`.
//!
//! Activation:
//! - env: `NEWT_CODER=1`, or
//! - per-session ACP param: `{ "coder": true }` on `new_session`.
//!
//! Both are checked by `newt-acp-worker`. The legacy newt-flat path is
//! the default and remains unchanged.

pub mod coder;
pub mod emission;
pub mod error;
pub mod prompt;
pub mod workspace_scan;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use coder::{Coder, CoderRun};
pub use emission::{normalize_emission, Emission};
pub use error::{CoderError, Result};
pub use prompt::{build_prompt, CoderPrompt, DEFAULT_CONTEXT_CAP_CHARS, WHOLE_FILE_SYSTEM_PROMPT};
pub use workspace_scan::scan_workspace_for_files;
