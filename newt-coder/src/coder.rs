//! Coder orchestrator — stubbed in commit 1, real implementation in commit 4.

use std::path::Path;
use std::sync::Arc;

use newt_inference::InferenceBackend;

use crate::error::Result;

pub struct Coder {
    #[allow(dead_code)]
    backend: Arc<dyn InferenceBackend>,
}

#[derive(Debug, Clone)]
pub struct CoderRun {
    pub emission_shape: String,
    pub model_id: String,
    pub files_written: Vec<String>,
    pub raw_reply: String,
}

impl Coder {
    pub fn new(backend: Arc<dyn InferenceBackend>) -> Self {
        Self { backend }
    }

    pub async fn run(&self, _workspace: &Path, _task: &str) -> Result<CoderRun> {
        // Placeholder — full implementation lands in commit 4.
        Ok(CoderRun {
            emission_shape: plugins_protocol::emission_shape::PROSE.to_string(),
            model_id: String::new(),
            files_written: Vec::new(),
            raw_reply: String::new(),
        })
    }
}
