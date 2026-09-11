//! The single registration seam for foreign Python hosts.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

pub(crate) fn invalid(error: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(error.to_string())
}

pub(crate) fn json(value: &impl serde::Serialize) -> PyResult<String> {
    serde_json::to_string(value).map_err(invalid)
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(text: &str) -> PyResult<T> {
    serde_json::from_str(text).map_err(invalid)
}

/// Add the frame and harness modules to any consumer's extension.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    crate::frame::register(py, parent)?;
    crate::harness::register(py, parent)
}
