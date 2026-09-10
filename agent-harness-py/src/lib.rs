//! Python boundary for the reusable frame kernel and deterministic host.
//!
//! Consumers register these modules in their own extension. Neither the
//! kernel nor the host needs Python; only the composition root enables
//! PyO3's `extension-module` feature.

mod event;
mod frame;
mod harness;
pub mod pyo3_module;
