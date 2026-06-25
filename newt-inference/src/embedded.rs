//! In-process inference backend (#639) — the opt-in `embedded` cargo feature.
//!
//! An [`EmbeddedBackend`] runs a small quantized model from the
//! [`palette`](crate::palette) **in process** (no HTTP — `endpoint() -> None`),
//! so the summarizer and small auxiliary calls never contend with the primary
//! model. This module is the **scaffold**: the backend, the model-file
//! resolution, and the feature switch live here; the candle / Metal generation
//! engine is the next increment (see `docs/decisions/embedded_inference.md`).
//! Until it lands, [`complete`](EmbeddedBackend::complete) fails *clearly* rather
//! than returning a silent empty reply.

use std::path::PathBuf;

use async_trait::async_trait;
use newt_core::router::Tier;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};
use crate::palette::MiniModel;

/// An in-process inference backend over a [palette](crate::palette) mini model.
#[derive(Debug)]
pub struct EmbeddedBackend {
    name: String,
    model: &'static MiniModel,
    gguf_path: PathBuf,
}

impl EmbeddedBackend {
    /// Resolve a palette model by alias + its local GGUF path. Fails clearly when
    /// the alias is unknown or the file is absent — **nothing is auto-downloaded**
    /// into a small box (#639: "no silent download into a 16 GB / ~19 GB-free box").
    ///
    /// # Errors
    /// An unknown palette alias, or a `gguf_path` that does not exist.
    pub fn new(model_name: &str, gguf_path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let model = crate::palette::find(model_name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown mini model '{model_name}'; choose one of: {}",
                crate::palette::palette()
                    .iter()
                    .map(|m| m.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let gguf_path = gguf_path.into();
        if !gguf_path.exists() {
            anyhow::bail!(
                "model file not found: {} — fetch {} from https://huggingface.co/{} first \
                 (nothing is auto-downloaded)",
                gguf_path.display(),
                model.gguf_file,
                model.hf_repo
            );
        }
        Ok(Self {
            name: format!("embedded:{}", model.name),
            model,
            gguf_path,
        })
    }

    /// The resolved palette model.
    #[must_use]
    pub fn model(&self) -> &MiniModel {
        self.model
    }
}

#[async_trait]
impl InferenceBackend for EmbeddedBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn model_id(&self) -> &str {
        self.model.name
    }

    fn supports_tier(&self, _tier: Tier) -> bool {
        // A small auxiliary model serves whatever bounded call selects it (the
        // summarizer); tier ranking is the caller's concern.
        true
    }

    /// In-process: there is no network host, so the net-axis caveat check is
    /// vacuously satisfied (see the trait doc).
    fn endpoint(&self) -> Option<&str> {
        None
    }

    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatReply> {
        // SCAFFOLD: the model + arch + file are resolved; the in-process
        // generation engine (candle/Metal — docs/decisions/embedded_inference.md)
        // is the next increment. Fail clearly until it lands — never a silent
        // empty reply (the summarizer must know it did not run).
        anyhow::bail!(
            "embedded inference engine not yet wired for '{}' ({:?}, {}); the {} backend is the \
             #639 scaffold — see docs/decisions/embedded_inference.md",
            self.model.name,
            self.model.arch,
            self.gguf_path.display(),
            self.name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_an_unknown_palette_alias() {
        let err = EmbeddedBackend::new("no-such-model", "/tmp/x.gguf").unwrap_err();
        assert!(err.to_string().contains("unknown mini model"));
    }

    #[test]
    fn new_rejects_a_missing_model_file_without_downloading() {
        let err = EmbeddedBackend::new("qwen2.5-0.5b", "/nonexistent/qwen.gguf").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("model file not found"));
        assert!(msg.contains("nothing is auto-downloaded"));
    }

    #[tokio::test]
    async fn complete_fails_clearly_until_the_engine_lands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qwen.gguf");
        std::fs::write(&path, b"placeholder").unwrap();
        let be = EmbeddedBackend::new("qwen2.5-0.5b", &path).unwrap();
        assert_eq!(be.endpoint(), None, "in-process: no network host");
        assert_eq!(be.model_id(), "qwen2.5-0.5b");
        let err = be
            .complete(ChatRequest::new().user("hi"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not yet wired"));
    }
}
