//! Python owns orchestration; the Rust session owns policy, recording, and replay.

use agent_harness::{Session, ToolReturn};
use content_addressable::ContentId;
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use crate::pyo3_module::{decode, invalid, json};

#[pyclass(name = "Session")]
struct PySession {
    inner: Session,
}

#[pymethods]
impl PySession {
    /// Create a session with JSON configuration and optional durable directory.
    #[new]
    #[pyo3(signature = (config_json="{}", directory=None))]
    fn new(config_json: &str, directory: Option<&str>) -> PyResult<Self> {
        let config = decode(config_json)?;
        let inner = match directory {
            Some(directory) => Session::open(directory, config),
            None => Session::new(config),
        }
        .map_err(invalid)?;
        Ok(Self { inner })
    }

    /// Restore only after the recorded run and its authority have been verified.
    #[staticmethod]
    fn restore(directory: &str, head: &str, authority: &str) -> PyResult<Self> {
        Ok(Self {
            inner: Session::restore(directory, head.parse().map_err(invalid)?, authority)
                .map_err(invalid)?,
        })
    }

    #[staticmethod]
    fn restore_with_config(directory: &str, head: &str, config_json: &str) -> PyResult<Self> {
        Ok(Self {
            inner: Session::restore_with_config(
                directory,
                head.parse().map_err(invalid)?,
                &decode(config_json)?,
            )
            .map_err(invalid)?,
        })
    }

    fn run_id(&self) -> String {
        self.inner.run_id().to_string()
    }

    fn checkpoint_path(&self) -> Option<String> {
        self.inner
            .checkpoint_path()
            .map(|path| path.display().to_string())
    }

    fn config(&self) -> PyResult<String> {
        json(self.inner.config())
    }

    /// The durable head to retain for a subsequent restore.
    fn head(&self) -> String {
        self.inner.head().to_string()
    }

    /// Verify this process still owns the current run before host side effects.
    fn ensure_writer(&mut self) -> PyResult<()> {
        self.inner.ensure_writer().map_err(invalid)
    }

    /// Reset the host's per-turn navigation and elapsed-time budgets.
    fn start_turn(&mut self) {
        self.inner.start_turn();
    }

    /// Charge the host's external navigation work to the same elapsed budget.
    fn account_navigation_elapsed(&mut self, milliseconds: u64) {
        self.inner
            .account_navigation_elapsed(std::time::Duration::from_millis(milliseconds));
    }

    /// Record the exact request before dispatch, returning its JSON receipt.
    fn record_request(&mut self, body_json: &str, format: &str) -> PyResult<String> {
        let request = self
            .inner
            .record_request(decode(body_json)?, format)
            .map_err(invalid)?;
        receipt(request)
    }

    fn record_rendered_request(
        &mut self,
        body_json: &str,
        format: &str,
        messages_json: &str,
    ) -> PyResult<String> {
        receipt(
            self.inner
                .record_rendered_request(
                    decode(body_json)?,
                    format,
                    &decode::<Vec<serde_json::Value>>(messages_json)?,
                )
                .map_err(invalid)?,
        )
    }

    /// Record an observation before it can receive a verdict.
    fn record_reply(&mut self, request: &str, bytes: &[u8]) -> PyResult<String> {
        Ok(self
            .inner
            .record_reply(request.parse().map_err(invalid)?, bytes)
            .map_err(invalid)?
            .to_string())
    }

    /// Classify an observation as answer, narration, or question.
    fn record_verdict(&mut self, reply: &str, verdict: &str) -> PyResult<()> {
        let verdict =
            serde_json::from_value(serde_json::Value::String(verdict.into())).map_err(invalid)?;
        self.inner
            .record_verdict(reply.parse().map_err(invalid)?, verdict)
            .map_err(invalid)
    }

    /// Record failed adjudication without turning a pending reply into success.
    fn record_failure(&mut self, reply: &str, error: &str) -> PyResult<()> {
        self.inner
            .record_failure(reply.parse().map_err(invalid)?, error)
            .map_err(invalid)
    }

    /// Append an explicit host intervention, preserving its causal parent.
    fn record_intervention(&mut self, text: &str, parent: &str) -> PyResult<String> {
        Ok(self
            .inner
            .record_intervention(text, parent.parse().map_err(invalid)?)
            .map_err(invalid)?
            .to_string())
    }

    /// Retrieve a bounded page; access and byte limits remain host decisions.
    fn re_read(&mut self, cid: &str, offset: usize, max_bytes: usize) -> PyResult<String> {
        json(
            &self
                .inner
                .re_read(cid, offset, max_bytes)
                .map_err(invalid)?,
        )
    }

    /// Project caller-supplied role/content messages under the host budget.
    fn project(&mut self, messages_json: &str, max_bytes: usize) -> PyResult<String> {
        json(
            &self
                .inner
                .project(&decode::<Vec<serde_json::Value>>(messages_json)?, max_bytes)
                .map_err(invalid)?,
        )
    }

    fn record_messages(&mut self, messages_json: &str) -> PyResult<()> {
        self.inner
            .record_messages(&decode::<Vec<serde_json::Value>>(messages_json)?)
            .map_err(invalid)
    }

    fn restored_messages(&self) -> PyResult<String> {
        json(&self.inner.restored_messages().map_err(invalid)?)
    }

    fn retain_tool_output(&mut self, name: &str, bytes: &[u8]) -> PyResult<String> {
        Ok(self
            .inner
            .retain_tool_output(name, bytes)
            .map_err(invalid)?
            .to_string())
    }

    /// Commit the assistant envelope and queued occurrences before execution.
    fn begin_tool_batch(
        &mut self,
        reply: &str,
        calls_json: &str,
        messages_json: &str,
    ) -> PyResult<Vec<String>> {
        Ok(self
            .inner
            .begin_tool_batch(
                reply.parse().map_err(invalid)?,
                &decode::<Vec<serde_json::Value>>(calls_json)?,
                &decode::<Vec<serde_json::Value>>(messages_json)?,
            )
            .map_err(invalid)?
            .into_iter()
            .map(|id| id.to_string())
            .collect())
    }

    /// Verify writer authority and record execution intent before host effects.
    fn start_tool_call(&mut self, invocation: &str) -> PyResult<()> {
        self.inner
            .start_tool_call(invocation.parse().map_err(invalid)?)
            .map_err(invalid)
    }

    /// Commit observed bytes immediately. Only an explicitly typed tool error
    /// uses `failed`; string contents never determine the recorded outcome.
    #[pyo3(signature = (invocation, bytes, kind="observed", retained_sources_json="[]"))]
    fn record_tool_return(
        &mut self,
        invocation: &str,
        bytes: &[u8],
        kind: &str,
        retained_sources_json: &str,
    ) -> PyResult<String> {
        let retained_sources = decode::<Vec<ContentId>>(retained_sources_json)?;
        let returned = match kind {
            "observed" => ToolReturn::Observed {
                bytes,
                retained_sources: &retained_sources,
            },
            "failed" => ToolReturn::Failed {
                bytes,
                retained_sources: &retained_sources,
            },
            "retrieval" | "host" => {
                if !retained_sources.is_empty() {
                    return Err(invalid(
                        "retained sources apply only to observed or failed returns",
                    ));
                }
                let text = std::str::from_utf8(bytes).map_err(invalid)?;
                if kind == "retrieval" {
                    ToolReturn::Retrieval(text)
                } else {
                    ToolReturn::Host(text)
                }
            }
            _ => {
                return Err(invalid(
                    "tool return kind must be observed, failed, retrieval, or host",
                ))
            }
        };
        Ok(self
            .inner
            .record_tool_return(invocation.parse().map_err(invalid)?, returned)
            .map_err(invalid)?
            .to_string())
    }

    /// Record the provider envelope after committing the observed return.
    fn record_tool_delivery(&mut self, invocation: &str, message_json: &str) -> PyResult<()> {
        self.inner
            .record_tool_delivery(invocation.parse().map_err(invalid)?, &decode(message_json)?)
            .map_err(invalid)
    }

    /// Attach retained external sources after the raw return is committed.
    fn record_tool_sources(&mut self, invocation: &str, sources_json: &str) -> PyResult<()> {
        self.inner
            .record_tool_sources(
                invocation.parse().map_err(invalid)?,
                &decode::<Vec<ContentId>>(sources_json)?,
            )
            .map_err(invalid)
    }

    /// Close a queued call using an explicitly host-authored substitute.
    fn resolve_tool_call(&mut self, invocation: &str, message_json: &str) -> PyResult<()> {
        self.inner
            .resolve_tool_call(invocation.parse().map_err(invalid)?, &decode(message_json)?)
            .map_err(invalid)
    }

    /// Close remaining protocol slots without replaying external work.
    fn interrupt_tool_batch(&mut self, reason: &str) -> PyResult<String> {
        json(&self.inner.interrupt_tool_batch(reason).map_err(invalid)?)
    }

    fn tool_call(&self, invocation: &str) -> PyResult<String> {
        json(
            self.inner
                .tool_call(invocation.parse().map_err(invalid)?)
                .map_err(invalid)?,
        )
    }

    fn record_outcome(&mut self, reply: &str, outcome: &str, delivered: &str) -> PyResult<()> {
        self.inner
            .record_outcome(reply.parse().map_err(invalid)?, outcome, delivered)
            .map_err(invalid)
    }

    fn record_host_message(&mut self, text: &str, parent: &str) -> PyResult<String> {
        Ok(self
            .inner
            .record_host_message(text, parent.parse().map_err(invalid)?)
            .map_err(invalid)?
            .to_string())
    }

    fn record_host_envelope(&mut self, message_json: &str, parent: &str) -> PyResult<String> {
        Ok(self
            .inner
            .record_host_envelope(&decode(message_json)?, parent.parse().map_err(invalid)?)
            .map_err(invalid)?
            .to_string())
    }

    fn record_model_message(&mut self, reply: &str, text: &str) -> PyResult<String> {
        Ok(self
            .inner
            .record_model_message(reply.parse().map_err(invalid)?, text)
            .map_err(invalid)?
            .to_string())
    }

    fn last_message(&self) -> Option<String> {
        self.inner.last_message().map(|id| id.to_string())
    }

    fn catalog(&mut self, messages_json: &str, max_bytes: usize) -> PyResult<String> {
        json(
            &self
                .inner
                .catalog(&decode::<Vec<serde_json::Value>>(messages_json)?, max_bytes)
                .map_err(invalid)?,
        )
    }

    fn project_selection(
        &mut self,
        messages_json: &str,
        selected_cids: Vec<String>,
        max_bytes: usize,
    ) -> PyResult<String> {
        json(
            &self
                .inner
                .project_selection(
                    &decode::<Vec<serde_json::Value>>(messages_json)?,
                    &selected_cids,
                    max_bytes,
                )
                .map_err(invalid)?,
        )
    }

    fn record_navigation_request(&mut self, catalog_json: &str, prompt: &str) -> PyResult<String> {
        Ok(self
            .inner
            .record_navigation_request(&decode(catalog_json)?, prompt)
            .map_err(invalid)?
            .to_string())
    }

    fn record_navigation_reply(&mut self, request: &str, reply: &str) -> PyResult<()> {
        self.inner
            .record_navigation_reply(request.parse().map_err(invalid)?, reply)
            .map_err(invalid)
    }

    fn record_navigation_failure(&mut self, request: &str, error: &str) -> PyResult<()> {
        self.inner
            .record_navigation_failure(request.parse().map_err(invalid)?, error)
            .map_err(invalid)
    }

    fn record_adjudication_request(&mut self, reply: &str, prompt: &str) -> PyResult<String> {
        Ok(self
            .inner
            .record_adjudication_request(reply.parse().map_err(invalid)?, prompt)
            .map_err(invalid)?
            .to_string())
    }

    fn record_adjudication_reply(&mut self, request: &str, reply: &str) -> PyResult<()> {
        self.inner
            .record_adjudication_reply(request.parse().map_err(invalid)?, reply)
            .map_err(invalid)
    }

    fn record_adjudication_failure(&mut self, request: &str, error: &str) -> PyResult<()> {
        self.inner
            .record_adjudication_failure(request.parse().map_err(invalid)?, error)
            .map_err(invalid)
    }

    /// Replay the verified bytes of a previously recorded request.
    fn replay<'py>(&self, py: Python<'py>, request: &str) -> PyResult<Bound<'py, PyBytes>> {
        let bytes = self
            .inner
            .replay(request.parse().map_err(invalid)?)
            .map_err(invalid)?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Observations which still need a verdict; a pending reply is not success.
    fn pending_replies(&self) -> Vec<String> {
        self.inner
            .pending_replies()
            .into_iter()
            .map(|id| id.to_string())
            .collect()
    }
}

fn receipt(request: agent_harness::PreparedRequest) -> PyResult<String> {
    json(&serde_json::json!({
        "id": request.id,
        "projection": request.projection,
        "bytes": String::from_utf8(request.bytes).map_err(invalid)?,
    }))
}

pub(crate) fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let module = PyModule::new(py, "harness")?;
    module.add_class::<PySession>()?;
    parent.add_submodule(&module)
}
