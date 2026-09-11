//! A foreign composition root using the public leaf registration boundary.

use pyo3::prelude::*;

#[pymodule]
fn _smart_harness_consumer(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    agent_harness_py::pyo3_module::register(py, m)
}
