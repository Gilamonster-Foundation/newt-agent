//! Display-width primitives for the Markdown renderer.
//!
//! The implementation moved **up** to the public [`crate::tty::width`] in the
//! widget-suite promotion (`docs/decisions/tty_widget_suite.md` §3.0): it was
//! the workspace's only correct width model and it was locked at `pub(super)`
//! in here, which is the same privacy-causes-duplication pattern
//! `tty/frames.rs` records for the spinner frame sets. This module stays as the
//! downward re-export, so every `use super::width::…` in the renderer is
//! unchanged.

pub(super) use crate::tty::width::{ch_width, str_width};
