//! Error type for the newt-coder plugin.
//!
//! Mirrors the failure-mode taxonomy in
//! `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`:
//! workspace scan failure, prompt-too-large guard trip, malformed
//! emission, file-write failure, and inference-backend errors are
//! distinct variants so callers (and tests) can pattern-match.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoderError {
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("prompt too large: {actual} chars > cap {cap}")]
    PromptTooLarge { actual: usize, cap: usize },
    #[error("emission malformed: {0}")]
    BadEmission(String),
    #[error("file write failed: {0}")]
    FileWrite(String),
    #[error("inference: {0}")]
    Inference(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CoderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_error_renders() {
        let e = CoderError::Workspace("missing dir".to_string());
        assert!(e.to_string().contains("missing dir"));
    }

    #[test]
    fn prompt_too_large_renders() {
        let e = CoderError::PromptTooLarge {
            actual: 100,
            cap: 50,
        };
        let s = e.to_string();
        assert!(s.contains("100"));
        assert!(s.contains("50"));
    }

    #[test]
    fn bad_emission_renders() {
        let e = CoderError::BadEmission("no FILE: header".to_string());
        assert!(e.to_string().contains("no FILE: header"));
    }

    #[test]
    fn file_write_renders() {
        let e = CoderError::FileWrite("permission denied".to_string());
        assert!(e.to_string().contains("permission denied"));
    }

    #[test]
    fn inference_renders() {
        let e = CoderError::Inference("backend offline".to_string());
        assert!(e.to_string().contains("backend offline"));
    }

    #[test]
    fn io_error_converts() {
        let io: std::io::Error = std::io::Error::new(std::io::ErrorKind::NotFound, "x");
        let e: CoderError = io.into();
        assert!(matches!(e, CoderError::Io(_)));
    }
}
