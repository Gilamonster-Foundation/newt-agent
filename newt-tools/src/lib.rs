//! Newt-Agent tool surface.
//!
//! vi-minimal v0 set: `read`, `edit`, `search`, `apply_patch`.
//! Thin wrappers — when `thoon-fileops` publishes, this crate delegates
//! to it rather than reimplementing.

pub mod patch;
pub mod read;
pub mod search;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use patch::{apply_patch, apply_whole_files, edit};
pub use read::read;
pub use search::{search, Hit};
