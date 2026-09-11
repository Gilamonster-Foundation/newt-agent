//! Text and byte conversions; admission and verification stay in agent-frame.

use agent_frame::{Op, RawUnit, RootEvent, Span, Unit};
use content_addressable::canonical::{from_canonical_dagcbor_checked, to_canonical_dagcbor};
use content_addressable::{ContentAddressable, ContentId};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::pyo3_module::{decode, invalid, json};

fn admit(text: &str) -> PyResult<Unit> {
    Unit::try_from(decode::<RawUnit>(text)?).map_err(invalid)
}

/// Address an operator_prompt, user_action, or harness_event by its bytes.
#[pyfunction]
fn root(kind: &str, material: &[u8], seq: u64) -> PyResult<String> {
    let kind = serde_json::from_value(serde_json::Value::String(kind.into())).map_err(invalid)?;
    json(&RootEvent::new(kind, material, seq))
}

/// Derive a structured root CID from a JSON root record.
#[pyfunction]
fn root_id(root: &str) -> PyResult<String> {
    Ok(decode::<RootEvent>(root)?
        .id()
        .map_err(invalid)?
        .to_string())
}

/// Seal an elision over the half-open byte range [start, end).
#[pyfunction]
fn elide(source: &[u8], start: u64, end: u64, root_id: &str) -> PyResult<String> {
    let unit = Unit::seal(
        Op::Elide,
        source,
        Span::new(start, end),
        root_id.parse().map_err(invalid)?,
    )
    .map_err(invalid)?;
    json(&unit)
}

/// Admit a JSON unit and return its derivation CID.
#[pyfunction]
fn unit_id(unit: &str) -> PyResult<String> {
    Ok(admit(unit)?.id().map_err(invalid)?.to_string())
}

/// Verify the admitted unit against source bytes and return the selected bytes.
#[pyfunction]
fn verify_unit<'py>(py: Python<'py>, unit: &str, source: &[u8]) -> PyResult<Bound<'py, PyBytes>> {
    let verified = agent_frame::verify_unit(&admit(unit)?, source).map_err(invalid)?;
    Ok(PyBytes::new(
        py,
        verified.span.slice(source).expect("kernel verified bounds"),
    ))
}

/// Encode the admitted unit's wire form, including lifecycle, for transfer.
#[pyfunction]
fn unit_canonical<'py>(py: Python<'py>, unit: &str) -> PyResult<Bound<'py, PyBytes>> {
    let canonical = to_canonical_dagcbor(&admit(unit)?).map_err(invalid)?;
    Ok(PyBytes::new(py, &canonical))
}

/// The derivation bytes that define the unit CID, independent of lifecycle.
#[pyfunction]
fn unit_identity_canonical<'py>(py: Python<'py>, unit: &str) -> PyResult<Bound<'py, PyBytes>> {
    Ok(PyBytes::new(
        py,
        &admit(unit)?.canonical_form().map_err(invalid)?,
    ))
}

/// Validate canonical structured bytes and derive their CID through the shared codec.
#[pyfunction]
fn canonical_id(canonical: &[u8]) -> PyResult<String> {
    Ok(ContentId::from_canonical_bytes_checked(canonical)
        .map_err(invalid)?
        .to_string())
}

/// Decode canonical bytes, then cross the same unit admission boundary.
#[pyfunction]
fn unit_from_canonical(canonical: &[u8]) -> PyResult<String> {
    let raw: RawUnit = from_canonical_dagcbor_checked(canonical).map_err(invalid)?;
    json(&Unit::try_from(raw).map_err(invalid)?)
}

pub(crate) fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let module = PyModule::new(py, "frame")?;
    module.add_class::<crate::event::PyEvent>()?;
    module.add_function(wrap_pyfunction!(root, &module)?)?;
    module.add_function(wrap_pyfunction!(root_id, &module)?)?;
    module.add_function(wrap_pyfunction!(elide, &module)?)?;
    module.add_function(wrap_pyfunction!(unit_id, &module)?)?;
    module.add_function(wrap_pyfunction!(verify_unit, &module)?)?;
    module.add_function(wrap_pyfunction!(unit_canonical, &module)?)?;
    module.add_function(wrap_pyfunction!(unit_identity_canonical, &module)?)?;
    module.add_function(wrap_pyfunction!(canonical_id, &module)?)?;
    module.add_function(wrap_pyfunction!(unit_from_canonical, &module)?)?;
    parent.add_submodule(&module)
}
