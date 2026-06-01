//! Python bindings for `newt-inference`.
//!
//! Compiled only when the `pyo3` cargo feature is on. Exposes:
//! - `Message`, `ChatRequest`, `ChatReply` — data types.
//! - `ModelInfo` — `list_models()` row for vLLM.
//! - `LocalOllamaBackend` — async `complete` + `discover`.
//! - `LocalVllmBackend` — async `complete` + `list_models`.
//! - `BackendRegistry` — sync registry of backends.
//!
//! Async methods return Python awaitables via
//! `pyo3_async_runtimes::tokio::future_into_py`. Driving them from
//! CPython requires an asyncio event loop — `asyncio.run(...)` or
//! `pytest-asyncio` are the normal entry points.

use std::sync::Arc;
use std::time::Duration;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend, Message};
use crate::local::{LocalOllamaBackend, LocalVllmBackend, ModelInfo};
use crate::registry::BackendRegistry;
use newt_core::pyo3_module::{PyNewtError, PyTier};
use newt_core::router::Tier;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyModule, PyType};
use pyo3_async_runtimes::tokio::future_into_py;
use tokio::sync::Mutex;

// ---- Message ----

/// One chat message (role + content).
#[pyclass(
    name = "Message",
    module = "newt_agent._newt_agent.inference",
    frozen,
    from_py_object
)]
#[derive(Clone)]
pub struct PyMessage {
    pub inner: Message,
}

#[pymethods]
impl PyMessage {
    #[new]
    fn new(role: String, content: String) -> Self {
        Self {
            inner: Message { role, content },
        }
    }

    #[classmethod]
    fn system(_cls: &Bound<'_, PyType>, content: String) -> Self {
        Self {
            inner: Message::system(content),
        }
    }

    #[classmethod]
    fn user(_cls: &Bound<'_, PyType>, content: String) -> Self {
        Self {
            inner: Message::user(content),
        }
    }

    #[classmethod]
    fn assistant(_cls: &Bound<'_, PyType>, content: String) -> Self {
        Self {
            inner: Message::assistant(content),
        }
    }

    #[getter]
    fn role(&self) -> &str {
        &self.inner.role
    }

    #[getter]
    fn content(&self) -> &str {
        &self.inner.content
    }

    fn __repr__(&self) -> String {
        format!(
            "Message(role='{}', content='{}')",
            self.inner.role,
            self.inner.content.escape_default(),
        )
    }
}

// ---- ChatRequest ----

/// A chat completion request: messages + optional `max_tokens`.
#[pyclass(name = "ChatRequest", module = "newt_agent._newt_agent.inference")]
pub struct PyChatRequest {
    pub inner: ChatRequest,
}

#[pymethods]
impl PyChatRequest {
    #[new]
    fn new() -> Self {
        Self {
            inner: ChatRequest::new(),
        }
    }

    /// Append a system message and return `self` for chaining.
    fn system(mut slf: PyRefMut<'_, Self>, content: String) -> PyRefMut<'_, Self> {
        slf.inner.messages.push(Message::system(content));
        slf
    }

    /// Append a user message and return `self` for chaining.
    fn user(mut slf: PyRefMut<'_, Self>, content: String) -> PyRefMut<'_, Self> {
        slf.inner.messages.push(Message::user(content));
        slf
    }

    /// Append an assistant message and return `self` for chaining.
    fn assistant(mut slf: PyRefMut<'_, Self>, content: String) -> PyRefMut<'_, Self> {
        slf.inner.messages.push(Message::assistant(content));
        slf
    }

    /// Set `max_tokens` and return `self` for chaining.
    fn with_max_tokens(mut slf: PyRefMut<'_, Self>, n: u32) -> PyRefMut<'_, Self> {
        slf.inner.max_tokens = Some(n);
        slf
    }

    #[getter]
    fn messages(&self) -> Vec<PyMessage> {
        self.inner
            .messages
            .iter()
            .cloned()
            .map(|inner| PyMessage { inner })
            .collect()
    }

    #[getter]
    fn max_tokens(&self) -> Option<u32> {
        self.inner.max_tokens
    }
}

impl Default for PyChatRequest {
    fn default() -> Self {
        Self::new()
    }
}

// ---- ChatReply ----

/// A chat completion reply (content + model id).
#[pyclass(
    name = "ChatReply",
    module = "newt_agent._newt_agent.inference",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyChatReply {
    pub inner: ChatReply,
}

#[pymethods]
impl PyChatReply {
    #[new]
    fn new(content: String, model_id: String) -> Self {
        Self {
            inner: ChatReply { content, model_id, usage: None },
        }
    }

    #[getter]
    fn content(&self) -> &str {
        &self.inner.content
    }

    #[getter]
    fn model_id(&self) -> &str {
        &self.inner.model_id
    }

    /// "backend=<name> model_id=<id>" — audit-trail line.
    fn audit_string(&self, backend_name: &str) -> String {
        self.inner.audit_string(backend_name)
    }

    fn __repr__(&self) -> String {
        format!(
            "ChatReply(model_id='{}', content='{}')",
            self.inner.model_id,
            self.inner.content.escape_default(),
        )
    }
}

// ---- ModelInfo ----

/// One row from `LocalVllmBackend.list_models`.
#[pyclass(
    name = "ModelInfo",
    module = "newt_agent._newt_agent.inference",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyModelInfo {
    pub inner: ModelInfo,
}

#[pymethods]
impl PyModelInfo {
    #[getter]
    fn id(&self) -> &str {
        &self.inner.id
    }

    fn __repr__(&self) -> String {
        format!("ModelInfo(id='{}')", self.inner.id)
    }
}

// ---- LocalOllamaBackend ----

/// Ollama-compatible HTTP backend.
#[pyclass(
    name = "LocalOllamaBackend",
    module = "newt_agent._newt_agent.inference"
)]
pub struct PyLocalOllamaBackend {
    pub inner: Arc<LocalOllamaBackend>,
}

#[pymethods]
impl PyLocalOllamaBackend {
    #[new]
    fn new(endpoint: String, model: String) -> Self {
        Self {
            inner: Arc::new(LocalOllamaBackend::new(endpoint, model)),
        }
    }

    /// Endpoint URL the backend is bound to.
    fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    /// Built-in fallback endpoint list (in-cluster proxy + home.lab
    /// names + localhost). Use this as the candidate list for
    /// `discover_with_candidates`.
    #[classmethod]
    fn default_endpoints(_cls: &Bound<'_, PyType>) -> Vec<String> {
        LocalOllamaBackend::default_endpoints()
    }

    /// Probe endpoints (env first, then defaults) and return a
    /// backend bound to the first reachable one. Awaitable.
    #[classmethod]
    fn discover<'py>(
        _cls: &Bound<'py, PyType>,
        py: Python<'py>,
        model: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            let backend = LocalOllamaBackend::discover(&model)
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("discover: {e}")))?;
            Ok(Self {
                inner: Arc::new(backend),
            })
        })
    }

    /// Strict probe: every candidate (including any env override)
    /// must respond to `/api/tags` for selection. Awaitable.
    #[classmethod]
    fn discover_strict<'py>(
        _cls: &Bound<'py, PyType>,
        py: Python<'py>,
        model: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move {
            let backend = LocalOllamaBackend::discover_strict(&model)
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("discover_strict: {e}")))?;
            Ok(Self {
                inner: Arc::new(backend),
            })
        })
    }

    /// Issue one chat completion. Returns an awaitable yielding a
    /// `ChatReply`. Retries 5xx/connection errors transparently.
    fn complete<'py>(
        &self,
        py: Python<'py>,
        request: PyRef<'_, PyChatRequest>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let backend = self.inner.clone();
        let req = request.inner.clone();
        future_into_py(py, async move {
            let reply = backend
                .complete(req)
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("complete: {e}")))?;
            Ok(PyChatReply { inner: reply })
        })
    }

    fn supports_tier(&self, tier: PyTier) -> bool {
        let t: Tier = py_tier_to_inner(tier);
        self.inner.supports_tier(t)
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
}

// ---- LocalVllmBackend ----

/// vLLM (OpenAI-compatible) HTTP backend.
#[pyclass(name = "LocalVllmBackend", module = "newt_agent._newt_agent.inference")]
pub struct PyLocalVllmBackend {
    pub inner: Arc<LocalVllmBackend>,
}

#[pymethods]
impl PyLocalVllmBackend {
    #[new]
    fn new(endpoint: String, model: String) -> Self {
        Self {
            inner: Arc::new(LocalVllmBackend::new(endpoint, model)),
        }
    }

    fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    fn complete<'py>(
        &self,
        py: Python<'py>,
        request: PyRef<'_, PyChatRequest>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let backend = self.inner.clone();
        let req = request.inner.clone();
        future_into_py(py, async move {
            let reply = backend
                .complete(req)
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("complete: {e}")))?;
            Ok(PyChatReply { inner: reply })
        })
    }

    /// `GET /v1/models`. Returns an awaitable yielding `list[ModelInfo]`.
    fn list_models<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let backend = self.inner.clone();
        future_into_py(py, async move {
            let models = backend
                .list_models()
                .await
                .map_err(|e| PyRuntimeError::new_err(format!("list_models: {e}")))?;
            Ok(models
                .into_iter()
                .map(|inner| PyModelInfo { inner })
                .collect::<Vec<_>>())
        })
    }

    fn supports_tier(&self, tier: PyTier) -> bool {
        let t: Tier = py_tier_to_inner(tier);
        self.inner.supports_tier(t)
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
}

// ---- BackendRegistry ----

/// Registry of inference backends; pick by tier.
#[pyclass(name = "BackendRegistry", module = "newt_agent._newt_agent.inference")]
pub struct PyBackendRegistry {
    /// Wrap in a tokio Mutex so Python can mutate (`register`) without
    /// the GIL providing the only synchronization.
    pub inner: Arc<Mutex<BackendRegistry>>,
}

#[pymethods]
impl PyBackendRegistry {
    #[new]
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BackendRegistry::new())),
        }
    }

    /// Register a `LocalOllamaBackend`.
    fn register_ollama<'py>(
        &self,
        py: Python<'py>,
        backend: PyRef<'_, PyLocalOllamaBackend>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let registry = self.inner.clone();
        let backend = backend.inner.clone();
        future_into_py(py, async move {
            let mut guard = registry.lock().await;
            // Backend trait objects require static + Send + Sync; both
            // satisfied by LocalOllamaBackend.
            let dyn_backend: Arc<dyn InferenceBackend> = backend;
            guard.register(dyn_backend);
            Ok(())
        })
    }

    /// Register a `LocalVllmBackend`.
    fn register_vllm<'py>(
        &self,
        py: Python<'py>,
        backend: PyRef<'_, PyLocalVllmBackend>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let registry = self.inner.clone();
        let backend = backend.inner.clone();
        future_into_py(py, async move {
            let mut guard = registry.lock().await;
            let dyn_backend: Arc<dyn InferenceBackend> = backend;
            guard.register(dyn_backend);
            Ok(())
        })
    }

    /// Backend names in registration order. Awaitable.
    fn names<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let registry = self.inner.clone();
        future_into_py(py, async move {
            let guard = registry.lock().await;
            Ok(guard
                .names()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>())
        })
    }

    /// Backend count. Awaitable.
    fn len<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let registry = self.inner.clone();
        future_into_py(py, async move {
            let guard = registry.lock().await;
            Ok(guard.len())
        })
    }
}

impl Default for PyBackendRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert the Python-side `Tier` enum back into the Rust enum.
fn py_tier_to_inner(t: PyTier) -> Tier {
    match t {
        PyTier::Fast => Tier::Fast,
        PyTier::Standard => Tier::Standard,
        PyTier::Complex => Tier::Complex,
        PyTier::Review => Tier::Review,
    }
}

/// `Duration` builder exported for tests that want to tweak timeouts.
#[pyfunction]
fn duration_millis(ms: u64) -> u64 {
    Duration::from_millis(ms).as_millis() as u64
}

/// Register the `inference` submodule on the parent `_newt_agent` module.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "inference")?;
    m.add_class::<PyMessage>()?;
    m.add_class::<PyChatRequest>()?;
    m.add_class::<PyChatReply>()?;
    m.add_class::<PyModelInfo>()?;
    m.add_class::<PyLocalOllamaBackend>()?;
    m.add_class::<PyLocalVllmBackend>()?;
    m.add_class::<PyBackendRegistry>()?;
    m.add_function(wrap_pyfunction!(duration_millis, &m)?)?;
    // Re-export the core NewtError so callers don't have to import
    // from two submodules to catch backend errors.
    m.add("NewtError", py.get_type::<PyNewtError>())?;
    parent.add_submodule(&m)?;
    Ok(())
}
