//! Immutable admitted parents compose without trusting a foreign resolver.

use std::collections::BTreeMap;

use agent_frame::{Event, RawEvent};
use content_addressable::{
    canonical::from_canonical_dagcbor_checked, ContentAddressable, ContentId,
};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::pyo3_module::{decode, invalid, json};

/// Structural admission does not grant host authority or verify payload bytes.
#[pyclass(name = "Event", frozen)]
pub(crate) struct PyEvent {
    inner: Event,
}

fn parents(events: Option<Vec<PyRef<'_, PyEvent>>>) -> PyResult<BTreeMap<ContentId, Event>> {
    events
        .unwrap_or_default()
        .into_iter()
        .map(|event| Ok((event.inner.id().map_err(invalid)?, event.inner.clone())))
        .collect()
}

#[pymethods]
impl PyEvent {
    #[new]
    #[pyo3(signature = (body_json, parent_events=None))]
    fn new(body_json: &str, parent_events: Option<Vec<PyRef<'_, Self>>>) -> PyResult<Self> {
        let parents = parents(parent_events)?;
        Ok(Self {
            inner: Event::new(decode(body_json)?, parents.keys().copied(), |id| {
                parents.get(id).cloned()
            })
            .map_err(invalid)?,
        })
    }

    #[staticmethod]
    #[pyo3(signature = (raw_json, parent_events=None))]
    fn from_json(raw_json: &str, parent_events: Option<Vec<PyRef<'_, Self>>>) -> PyResult<Self> {
        let parents = parents(parent_events)?;
        Ok(Self {
            inner: Event::admit(decode::<RawEvent>(raw_json)?, |id| parents.get(id).cloned())
                .map_err(invalid)?,
        })
    }

    #[staticmethod]
    #[pyo3(signature = (canonical, parent_events=None))]
    fn from_canonical(
        canonical: &[u8],
        parent_events: Option<Vec<PyRef<'_, Self>>>,
    ) -> PyResult<Self> {
        let parents = parents(parent_events)?;
        let raw: RawEvent = from_canonical_dagcbor_checked(canonical).map_err(invalid)?;
        Ok(Self {
            inner: Event::admit(raw, |id| parents.get(id).cloned()).map_err(invalid)?,
        })
    }

    fn id(&self) -> PyResult<String> {
        Ok(self.inner.id().map_err(invalid)?.to_string())
    }

    fn to_json(&self) -> PyResult<String> {
        json(&self.inner)
    }

    fn body(&self) -> PyResult<String> {
        json(self.inner.body())
    }

    fn depth(&self) -> u32 {
        self.inner.depth()
    }

    fn parents(&self) -> Vec<String> {
        self.inner
            .parents()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn canonical<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        Ok(PyBytes::new(
            py,
            &self.inner.canonical_form().map_err(invalid)?,
        ))
    }
}
