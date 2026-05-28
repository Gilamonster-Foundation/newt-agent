//! Newt-Agent tool surface.
//!
//! vi-minimal v0 set: `read`, `edit`, `search`, `apply_patch`.
//! Thin wrappers — when `thoon-fileops` publishes, this crate delegates
//! to it rather than reimplementing.

pub mod patch;
pub mod read;
pub mod search;

pub use patch::{apply_patch, edit};
pub use read::read;
pub use search::{search, Hit};
