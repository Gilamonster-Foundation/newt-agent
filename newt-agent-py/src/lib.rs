//! Umbrella Python extension module for newt-agent.
//!
//! One cdylib, seven submodules (core, tools, coder, eval, inference,
//! acp_worker, mcp). Each underlying crate exposes a
//! `pyo3_module::register` function that adds its types to the parent
//! module — this crate just stitches them together.

use pyo3::prelude::*;

#[pymodule]
fn _newt_agent(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    newt_core::pyo3_module::register(py, m)?;
    newt_tools::pyo3_module::register(py, m)?;
    newt_inference::pyo3_module::register(py, m)?;
    newt_coder::pyo3_module::register(py, m)?;
    newt_eval::pyo3_module::register(py, m)?;
    newt_acp_worker::pyo3_module::register(py, m)?;
    newt_mcp_server::pyo3_module::register(py, m)?;
    Ok(())
}
