//! Python bindings for `newt-core`.
//!
//! Compiled only when the `pyo3` cargo feature is on; the rest of the
//! crate has no Python dependencies. The umbrella `newt-agent-py`
//! crate is the one consumer that turns this on.
//!
//! Exposes the small core surface — `NewtError`, `Router`,
//! `Classification`, `Tier`, `Config`, `SessionId`, `ModelId` — as
//! Python classes wrapped around the existing Rust values. Behavior is
//! unchanged; these are thin owning wrappers.

use crate::{
    config::{BackendConfig, Config, ProviderConfig},
    error::NewtError,
    model_id::ModelId,
    router::{Classification, Router, Tier},
    session::SessionId,
};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyModule, PyType};
use std::path::PathBuf;
use std::str::FromStr;

create_exception!(_newt_agent, PyNewtError, PyException);

/// Convert a `NewtError` into the Python-side `NewtError` exception.
fn newt_err_to_py(e: NewtError) -> PyErr {
    PyNewtError::new_err(e.to_string())
}

// ---- Tier ----

/// Tier label routed by [`PyRouter`]. Wraps the Rust `Tier` enum.
#[pyclass(
    name = "Tier",
    module = "newt_agent._newt_agent.core",
    eq,
    eq_int,
    frozen,
    from_py_object
)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PyTier {
    Fast,
    Standard,
    Complex,
    Review,
}

impl PyTier {
    fn from_inner(tier: Tier) -> Self {
        match tier {
            Tier::Fast => Self::Fast,
            Tier::Standard => Self::Standard,
            Tier::Complex => Self::Complex,
            Tier::Review => Self::Review,
        }
    }

    fn to_inner(self) -> Tier {
        match self {
            Self::Fast => Tier::Fast,
            Self::Standard => Tier::Standard,
            Self::Complex => Tier::Complex,
            Self::Review => Tier::Review,
        }
    }
}

#[pymethods]
impl PyTier {
    fn __repr__(&self) -> &'static str {
        match self {
            Self::Fast => "Tier.Fast",
            Self::Standard => "Tier.Standard",
            Self::Complex => "Tier.Complex",
            Self::Review => "Tier.Review",
        }
    }

    fn __str__(&self) -> &'static str {
        match self {
            Self::Fast => "FAST",
            Self::Standard => "STANDARD",
            Self::Complex => "COMPLEX",
            Self::Review => "REVIEW",
        }
    }

    /// Parse a canonical UPPER-CASE label back into a tier. Mirrors the
    /// `serde(rename_all = "UPPERCASE")` shape that `case.toml` uses.
    #[classmethod]
    fn parse(_cls: &Bound<'_, PyType>, s: &str) -> PyResult<Self> {
        match s.to_ascii_uppercase().as_str() {
            "FAST" => Ok(Self::Fast),
            "STANDARD" => Ok(Self::Standard),
            "COMPLEX" => Ok(Self::Complex),
            "REVIEW" => Ok(Self::Review),
            other => Err(PyNewtError::new_err(format!(
                "invalid tier: {other} (expected FAST|STANDARD|COMPLEX|REVIEW)"
            ))),
        }
    }
}

// ---- Classification ----

/// One classification result from [`PyRouter::classify_detailed`].
#[pyclass(
    name = "Classification",
    module = "newt_agent._newt_agent.core",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyClassification {
    pub inner: Classification,
}

#[pymethods]
impl PyClassification {
    #[getter]
    fn tier(&self) -> PyTier {
        PyTier::from_inner(self.inner.tier)
    }

    #[getter]
    fn confidence(&self) -> f64 {
        self.inner.confidence
    }

    #[getter]
    fn reasons(&self) -> Vec<String> {
        self.inner.reasons.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "Classification(tier={:?}, confidence={:.2}, reasons={:?})",
            self.inner.tier, self.inner.confidence, self.inner.reasons
        )
    }
}

// ---- Router ----

/// Tier router. Construct with `Router()` or `Router.with_override(tier)`.
#[pyclass(name = "Router", module = "newt_agent._newt_agent.core")]
pub struct PyRouter {
    inner: Router,
}

#[pymethods]
impl PyRouter {
    #[new]
    fn new() -> Self {
        Self {
            inner: Router::new(),
        }
    }

    /// Build a router that always returns `tier` (confidence 1.0).
    #[classmethod]
    fn with_override(_cls: &Bound<'_, PyType>, tier: PyTier) -> Self {
        Self {
            inner: Router::with_override(tier.to_inner()),
        }
    }

    /// Return just the chosen tier for `prompt`.
    fn classify(&self, prompt: &str) -> PyTier {
        PyTier::from_inner(self.inner.classify(prompt))
    }

    /// Return the full classification (tier + confidence + reasons).
    fn classify_detailed(&self, prompt: &str) -> PyClassification {
        PyClassification {
            inner: self.inner.classify_detailed(prompt),
        }
    }
}

// ---- ModelId ----

/// Strongly-typed model identifier for audit-trail purposes.
#[pyclass(
    name = "ModelId",
    module = "newt_agent._newt_agent.core",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyModelId {
    pub inner: ModelId,
}

#[pymethods]
impl PyModelId {
    #[new]
    fn new(id: String) -> Self {
        Self {
            inner: ModelId::new(id),
        }
    }

    fn as_str(&self) -> &str {
        self.inner.as_str()
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("ModelId('{}')", self.inner.as_str())
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    fn __hash__(&self) -> u64 {
        let bytes = self.inner.as_str().as_bytes();
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }
}

// ---- SessionId ----

/// Opaque session identifier (UUID v4).
#[pyclass(
    name = "SessionId",
    module = "newt_agent._newt_agent.core",
    frozen,
    skip_from_py_object
)]
#[derive(Clone, Copy)]
pub struct PySessionId {
    pub inner: SessionId,
}

#[pymethods]
impl PySessionId {
    /// Generate a fresh session id (UUID v4).
    #[new]
    fn new() -> Self {
        Self {
            inner: SessionId::new(),
        }
    }

    /// Parse a session id from its hyphenated string representation.
    #[classmethod]
    fn parse(_cls: &Bound<'_, PyType>, s: &str) -> PyResult<Self> {
        let id = SessionId::from_str(s)
            .map_err(|e| PyNewtError::new_err(format!("invalid session id: {e}")))?;
        Ok(Self { inner: id })
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!("SessionId('{}')", self.inner)
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    fn __hash__(&self) -> u64 {
        let bytes = self.inner.as_uuid().as_bytes();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[..8]);
        u64::from_le_bytes(buf)
    }
}

// ---- BackendConfig ----

/// One inference backend entry.
#[pyclass(
    name = "BackendConfig",
    module = "newt_agent._newt_agent.core",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyBackendConfig {
    pub inner: BackendConfig,
}

#[pymethods]
impl PyBackendConfig {
    #[new]
    fn new(name: String, endpoint: String, model: String, tiers: Vec<PyTier>) -> Self {
        Self {
            inner: BackendConfig {
                name,
                endpoint,
                model,
                tiers: tiers.into_iter().map(PyTier::to_inner).collect(),
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    #[getter]
    fn model(&self) -> &str {
        &self.inner.model
    }

    #[getter]
    fn tiers(&self) -> Vec<PyTier> {
        self.inner
            .tiers
            .iter()
            .copied()
            .map(PyTier::from_inner)
            .collect()
    }
}

// ---- ProviderConfig ----

/// One subprocess provider-plugin entry.
#[pyclass(
    name = "ProviderConfig",
    module = "newt_agent._newt_agent.core",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyProviderConfig {
    pub inner: ProviderConfig,
}

#[pymethods]
impl PyProviderConfig {
    #[new]
    #[pyo3(signature = (name, command, tiers, env_pass = None))]
    fn new(
        name: String,
        command: String,
        tiers: Vec<PyTier>,
        env_pass: Option<Vec<String>>,
    ) -> Self {
        Self {
            inner: ProviderConfig {
                name,
                command,
                env_pass: env_pass.unwrap_or_default(),
                tiers: tiers.into_iter().map(PyTier::to_inner).collect(),
            },
        }
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn command(&self) -> &str {
        &self.inner.command
    }

    #[getter]
    fn env_pass(&self) -> Vec<String> {
        self.inner.env_pass.clone()
    }

    #[getter]
    fn tiers(&self) -> Vec<PyTier> {
        self.inner
            .tiers
            .iter()
            .copied()
            .map(PyTier::from_inner)
            .collect()
    }
}

// ---- Config ----

/// Top-level newt-agent configuration.
#[pyclass(
    name = "Config",
    module = "newt_agent._newt_agent.core",
    skip_from_py_object
)]
pub struct PyConfig {
    pub inner: Config,
}

#[pymethods]
impl PyConfig {
    /// Build a `Config` carrying the built-in defaults (one Ollama
    /// backend on localhost).
    #[new]
    fn new() -> Self {
        Self {
            inner: Config::default(),
        }
    }

    /// Load configuration from an explicit TOML file.
    #[classmethod]
    fn load(_cls: &Bound<'_, PyType>, path: PathBuf) -> PyResult<Self> {
        let inner = Config::load(&path).map_err(newt_err_to_py)?;
        Ok(Self { inner })
    }

    /// Resolve configuration via the usual search order:
    /// `$NEWT_CONFIG`, `./newt.toml`, `~/.newt/config.toml`,
    /// `/etc/newt/config.toml`. Falls back to defaults if none exist.
    #[classmethod]
    fn resolve(_cls: &Bound<'_, PyType>) -> PyResult<Self> {
        let inner = Config::resolve().map_err(newt_err_to_py)?;
        Ok(Self { inner })
    }

    #[getter]
    fn backends(&self) -> Vec<PyBackendConfig> {
        self.inner
            .backends
            .iter()
            .cloned()
            .map(|inner| PyBackendConfig { inner })
            .collect()
    }

    #[getter]
    fn providers(&self) -> Vec<PyProviderConfig> {
        self.inner
            .providers
            .iter()
            .cloned()
            .map(|inner| PyProviderConfig { inner })
            .collect()
    }

    #[getter]
    fn default_tier_order(&self) -> Vec<PyTier> {
        self.inner
            .default_tier_order
            .iter()
            .copied()
            .map(PyTier::from_inner)
            .collect()
    }
}

/// Register the `core` submodule on the parent `_newt_agent` module.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "core")?;
    m.add_class::<PyTier>()?;
    m.add_class::<PyClassification>()?;
    m.add_class::<PyRouter>()?;
    m.add_class::<PyModelId>()?;
    m.add_class::<PySessionId>()?;
    m.add_class::<PyBackendConfig>()?;
    m.add_class::<PyProviderConfig>()?;
    m.add_class::<PyConfig>()?;
    m.add("NewtError", py.get_type::<PyNewtError>())?;
    parent.add_submodule(&m)?;
    Ok(())
}
