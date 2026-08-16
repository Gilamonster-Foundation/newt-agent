//! Newt-Agent tool surface.
//!
//! vi-minimal v0 set: `read`, `edit`, `search`, `apply_patch`.
//! Thin wrappers — when `thoon-fileops` publishes, this crate delegates
//! to it rather than reimplementing.

pub mod jupyter;
pub mod ls;
pub mod patch;
pub mod read;
pub mod search;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use jupyter::{execute_notebook, JupyterExecuteParams, JupyterExecuteResult};
pub use ls::{list_dir, DirEntry, EntryKind};
#[cfg(feature = "applier-diffy")]
pub use patch::DiffyApplier;
pub use patch::{
    applier_from_env, apply_patch, apply_whole_files, edit, FuzzyApplier, PatchApplier,
};
pub use read::read;
pub use search::{search, Hit};
