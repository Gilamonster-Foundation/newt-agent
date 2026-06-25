//! In-process inference backend (#639) — the opt-in `embedded` cargo feature.
//!
//! An [`EmbeddedBackend`] runs a small quantized model from the
//! [`palette`](crate::palette) **in process** (no HTTP — `endpoint() -> None`),
//! so the summarizer + small auxiliary calls never contend with the primary
//! model. The generation engine is pure-Rust **candle** with **adaptive,
//! non-contending** device selection: **CPU by default** (never fights the GPU
//! the primary uses), with `embedded-metal` / `embedded-cuda` accelerators opt-in
//! via `NEWT_EMBEDDED_DEVICE = cpu|metal|cuda|auto`.
//!
//! Engine scope (first increment): the **Qwen2** architecture (`qwen2.5-*`, the
//! default summarizer picks). Other palette arches load to a clear "not yet
//! supported" error rather than mis-generating.

use std::path::{Path, PathBuf};

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
    tokenizer_path: PathBuf,
}

impl EmbeddedBackend {
    /// Resolve a palette model by alias + its local GGUF path. The matching
    /// `tokenizer.json` is expected **next to** the GGUF (candle needs it
    /// separately from the GGUF weights). Fails clearly when the alias is unknown
    /// or a file is absent — **nothing is auto-downloaded** into a small box
    /// (#639: "no silent download into a 16 GB / ~19 GB-free box").
    ///
    /// # Errors
    /// An unknown palette alias, or a missing GGUF / `tokenizer.json`.
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
        let tokenizer_path = gguf_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("tokenizer.json");
        if !tokenizer_path.exists() {
            anyhow::bail!(
                "tokenizer not found: {} — place tokenizer.json next to the GGUF (fetch it from \
                 https://huggingface.co/{})",
                tokenizer_path.display(),
                model.hf_repo
            );
        }
        Ok(Self {
            name: format!("embedded:{}", model.name),
            model,
            gguf_path,
            tokenizer_path,
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

    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatReply> {
        let max_tokens = req.max_tokens.unwrap_or(512) as usize;
        let prompt = engine::format_chatml(&req.messages);
        let gguf = self.gguf_path.clone();
        let tok = self.tokenizer_path.clone();
        let arch = self.model.arch;
        let model_id = self.model.name.to_string();
        // candle is synchronous + CPU/GPU-bound; keep it off the async runtime.
        let content = tokio::task::spawn_blocking(move || {
            engine::generate(&gguf, &tok, arch, &prompt, max_tokens)
        })
        .await
        .map_err(|e| anyhow::anyhow!("embedded inference task panicked: {e}"))??;
        Ok(ChatReply {
            content,
            model_id,
            usage: None,
        })
    }
}

/// The candle generation engine (synchronous; called via `spawn_blocking`).
mod engine {
    use anyhow::Context;
    use candle_core::quantized::gguf_file;
    use candle_core::{Device, Tensor};
    use candle_transformers::generation::LogitsProcessor;
    use candle_transformers::models::quantized_qwen2::ModelWeights as Qwen2;
    use tokenizers::Tokenizer;

    use crate::backend::Message;
    use crate::palette::ModelArch;

    /// The CUDA device, if the `embedded-cuda` feature is compiled and it inits.
    fn cuda_device() -> Option<Device> {
        #[cfg(feature = "embedded-cuda")]
        {
            return Device::new_cuda(0).ok();
        }
        #[allow(unreachable_code)]
        None
    }

    /// The Metal device, if the `embedded-metal` feature is compiled and it inits.
    fn metal_device() -> Option<Device> {
        #[cfg(feature = "embedded-metal")]
        {
            return Device::new_metal(0).ok();
        }
        #[allow(unreachable_code)]
        None
    }

    /// Pick the inference device with **smart, non-contending defaults**. The
    /// default is **CPU** — guaranteed not to fight whatever GPU the primary model
    /// (or another agent) uses, which is the whole point of #639. An accelerator
    /// is opt-in, the same code adapting to whatever the box provides:
    ///
    /// `NEWT_EMBEDDED_DEVICE = cpu (default) | metal | cuda | auto`
    ///
    /// `auto` uses the first compiled accelerator that initializes (CUDA, then
    /// Metal), else CPU. A named accelerator that isn't compiled-in or fails to
    /// init falls back to CPU (the small summarizer must always run) — it never
    /// errors out of an inference call over device choice.
    fn device() -> anyhow::Result<Device> {
        let want = std::env::var("NEWT_EMBEDDED_DEVICE").unwrap_or_else(|_| "cpu".into());
        let want = want.trim().to_ascii_lowercase();
        let chosen = match want.as_str() {
            "cuda" => cuda_device(),
            "metal" => metal_device(),
            "auto" => cuda_device().or_else(metal_device),
            // "cpu" or anything unrecognized → the safe, non-contending default.
            _ => Some(Device::Cpu),
        };
        Ok(chosen.unwrap_or_else(|| {
            if want != "cpu" {
                tracing::warn!(
                    requested = %want,
                    "embedded inference: requested device unavailable; using CPU"
                );
            }
            Device::Cpu
        }))
    }

    /// Format chat messages as Qwen2's ChatML prompt, ending at the assistant turn.
    pub(super) fn format_chatml(messages: &[Message]) -> String {
        let mut s = String::new();
        for m in messages {
            s.push_str("<|im_start|>");
            s.push_str(&m.role);
            s.push('\n');
            s.push_str(&m.content);
            s.push_str("<|im_end|>\n");
        }
        s.push_str("<|im_start|>assistant\n");
        s
    }

    /// Load the model, run generation, decode. Qwen2 only for now.
    pub(super) fn generate(
        gguf_path: &std::path::Path,
        tokenizer_path: &std::path::Path,
        arch: ModelArch,
        prompt: &str,
        max_tokens: usize,
    ) -> anyhow::Result<String> {
        if arch != ModelArch::Qwen2 {
            anyhow::bail!(
                "the embedded engine currently supports the Qwen2 architecture only \
                 (use a qwen2.5-* model); {arch:?} support is a follow-up"
            );
        }
        let device = device()?;
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("load tokenizer {}: {e}", tokenizer_path.display()))?;

        let mut file = std::fs::File::open(gguf_path)
            .with_context(|| format!("open {}", gguf_path.display()))?;
        let content = gguf_file::Content::read(&mut file)
            .with_context(|| format!("read GGUF {}", gguf_path.display()))?;
        let mut model = Qwen2::from_gguf(content, &mut file, &device)
            .context("load Qwen2 weights from GGUF")?;

        let encoding = tokenizer
            .encode(prompt, true)
            .map_err(|e| anyhow::anyhow!("tokenize prompt: {e}"))?;
        let prompt_tokens: Vec<u32> = encoding.get_ids().to_vec();
        // Qwen2 ChatML end-of-turn; fall back to <|endoftext|>.
        let eos = tokenizer
            .token_to_id("<|im_end|>")
            .or_else(|| tokenizer.token_to_id("<|endoftext|>"))
            .unwrap_or(151_645);

        let mut logits_processor = LogitsProcessor::new(42, Some(0.2), None);
        let mut generated: Vec<u32> = Vec::new();
        let mut pos = 0usize;
        let mut next: Vec<u32> = prompt_tokens;
        for _ in 0..max_tokens {
            let input = Tensor::new(next.as_slice(), &device)?.unsqueeze(0)?;
            let logits = model.forward(&input, pos)?;
            // Reduce to the last position's logits, [vocab].
            let logits = logits.squeeze(0)?;
            let logits = if logits.rank() == 2 {
                logits.get(logits.dim(0)? - 1)?
            } else {
                logits
            };
            pos += next.len();
            let token = logits_processor.sample(&logits)?;
            if token == eos {
                break;
            }
            generated.push(token);
            next = vec![token];
        }
        tokenizer
            .decode(&generated, true)
            .map_err(|e| anyhow::anyhow!("decode reply: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Message;

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

    #[test]
    fn new_requires_a_tokenizer_next_to_the_gguf() {
        // GGUF exists but no tokenizer.json beside it → a clear error.
        let dir = tempfile::tempdir().unwrap();
        let gguf = dir.path().join("qwen.gguf");
        std::fs::write(&gguf, b"placeholder").unwrap();
        let err = EmbeddedBackend::new("qwen2.5-0.5b", &gguf).unwrap_err();
        assert!(err.to_string().contains("tokenizer not found"));
    }

    #[test]
    fn format_chatml_wraps_roles_and_opens_the_assistant_turn() {
        let msgs = vec![Message::system("be brief"), Message::user("summarize this")];
        let p = engine::format_chatml(&msgs);
        assert!(p.contains("<|im_start|>system\nbe brief<|im_end|>"));
        assert!(p.contains("<|im_start|>user\nsummarize this<|im_end|>"));
        assert!(p.ends_with("<|im_start|>assistant\n"));
    }

    /// On-device smoke test: load a real qwen2.5 GGUF + tokenizer.json and
    /// generate. `#[ignore]` (needs the model files); run with
    /// `NEWT_EMBEDDED_SMOKE_GGUF=/path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf \
    ///  cargo test -p newt-inference --features embedded smoke -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs a real GGUF + tokenizer.json; set NEWT_EMBEDDED_SMOKE_GGUF"]
    async fn smoke_generate_on_cpu() {
        let gguf = std::env::var("NEWT_EMBEDDED_SMOKE_GGUF")
            .expect("set NEWT_EMBEDDED_SMOKE_GGUF to a qwen2.5 GGUF (tokenizer.json beside it)");
        let be = EmbeddedBackend::new("qwen2.5-0.5b", &gguf).unwrap();
        let reply = be
            .complete(
                ChatRequest::new()
                    .system("You are a terse assistant. Reply in one short sentence.")
                    .user("Say hello and name one primary color.")
                    .max_tokens(40),
            )
            .await
            .unwrap();
        eprintln!("EMBEDDED SMOKE REPLY: {:?}", reply.content);
        assert!(
            !reply.content.trim().is_empty(),
            "expected a non-empty generation"
        );
    }
}
