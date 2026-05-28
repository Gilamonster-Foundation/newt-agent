//! Subprocess runner that drives a `newt worker` over ACP stdio JSON-RPC.
//!
//! Stub — implementation lands in the next commit.

use std::path::PathBuf;

use newt_acp_worker::TaskReply;
use serde::{Deserialize, Serialize};

use crate::cases::TestCase;

/// What the runner returns after one full case execution.
///
/// `workspace` and `baseline` are tempdir paths kept alive by the
/// containing [`RunOutcome`] until it's dropped.
#[derive(Debug)]
pub struct RunOutcome {
    pub case: TestCase,
    pub reply: TaskReply,
    pub workspace: PathBuf,
    pub baseline: PathBuf,
    /// Kept alive so the temp dirs aren't pruned out from under the
    /// evaluators. Dropped when the outcome is dropped.
    pub _workspace_guard: tempfile::TempDir,
    pub _baseline_guard: tempfile::TempDir,
}

/// How to run the worker. `mock_endpoint` is set in mock mode (the
/// wiremock URL); `None` in live mode (the worker discovers Ollama via
/// `OLLAMA_HOST` or its default endpoint list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub worker_bin: PathBuf,
    pub mock_endpoint: Option<String>,
    pub model_override: Option<String>,
}

impl RunnerConfig {
    pub fn new(worker_bin: impl Into<PathBuf>) -> Self {
        Self {
            worker_bin: worker_bin.into(),
            mock_endpoint: None,
            model_override: None,
        }
    }

    pub fn with_mock_endpoint(mut self, url: impl Into<String>) -> Self {
        self.mock_endpoint = Some(url.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model_override = Some(model.into());
        self
    }
}

/// Run one case end-to-end. Stub — fills out in the next commit.
pub async fn run_case(_case: &TestCase, _config: &RunnerConfig) -> anyhow::Result<RunOutcome> {
    anyhow::bail!("runner not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builders_compose() {
        let cfg = RunnerConfig::new("/tmp/newt")
            .with_mock_endpoint("http://127.0.0.1:8080")
            .with_model("llama3.1:8b");
        assert_eq!(cfg.worker_bin, PathBuf::from("/tmp/newt"));
        assert_eq!(cfg.mock_endpoint.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(cfg.model_override.as_deref(), Some("llama3.1:8b"));
    }
}
