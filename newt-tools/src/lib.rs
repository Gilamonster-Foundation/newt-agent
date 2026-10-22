//! Unified-diff and whole-file landing pads for model emissions:
//! `apply_patch` and `apply_whole_files`, used by `newt-coder`,
//! `newt-acp-worker` and the Python module.
//!
//! The file tools a model calls by name (`read_file`, `edit_file`,
//! `list_dir`, `grep`, …) live in `newt-core/src/agentic/tools/` and are
//! served from there to the TUI, headless and the MCP server alike (#2956).

pub mod patch;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

#[cfg(feature = "applier-diffy")]
pub use patch::DiffyApplier;
pub use patch::{applier_from_env, apply_patch, apply_whole_files, FuzzyApplier, PatchApplier};
