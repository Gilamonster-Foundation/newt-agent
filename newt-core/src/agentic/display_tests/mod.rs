//! Test families for [`super`] (`agentic::display`), split out of
//! `display.rs` by what each test asserts. Sibling files; no behaviour change.

pub(crate) use super::super::observability::ErrorClass;

// A glob RE-EXPORT, not a plain glob: the moved bodies write `super::X` from
// when `super` was `display`, and a private glob binding is not nameable by
// path from a child module.
pub(crate) use super::*;

#[cfg(test)]
#[path = "cadence.rs"]
mod cadence;
#[cfg(test)]
#[path = "compression_notice.rs"]
mod compression_notice;
#[cfg(test)]
#[path = "printers.rs"]
mod printers;
#[cfg(test)]
#[path = "reasoning_fold.rs"]
mod reasoning_fold;
#[cfg(test)]
#[path = "retry_indicator.rs"]
mod retry_indicator;
#[cfg(test)]
#[path = "spill_renderer.rs"]
mod spill_renderer;
#[cfg(test)]
#[path = "spill_view.rs"]
mod spill_view;
#[cfg(test)]
#[path = "summary_line.rs"]
mod summary_line;
#[cfg(test)]
#[path = "token_gauge.rs"]
mod token_gauge;
#[cfg(test)]
#[path = "wrapping.rs"]
mod wrapping;
