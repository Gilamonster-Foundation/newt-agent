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
use std::sync::Arc;

use async_trait::async_trait;
use newt_core::router::Tier;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};
use crate::palette::MiniModel;

/// An in-process inference backend over a [palette](crate::palette) mini model.
#[derive(Debug)]
pub struct EmbeddedBackend {
    name: String,
    model: &'static MiniModel,
    assets: ModelAssets,
    device: Option<candle_core::Device>,
}

#[derive(Debug, Clone)]
struct ModelAssets {
    gguf_path: PathBuf,
    tokenizer_path: PathBuf,
    pinned: Option<Arc<PinnedAssets>>,
}

struct PinnedAssets {
    weights: Vec<u8>,
    tokenizer: Vec<u8>,
}

impl std::fmt::Debug for PinnedAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedAssets")
            .field("weights_bytes", &self.weights.len())
            .field("tokenizer_bytes", &self.tokenizer.len())
            .finish()
    }
}

trait ReadSeek: std::io::Read + std::io::Seek {}
impl<T: std::io::Read + std::io::Seek> ReadSeek for T {}

impl ModelAssets {
    fn weights(&self) -> anyhow::Result<Box<dyn ReadSeek + '_>> {
        match &self.pinned {
            Some(pinned) => Ok(Box::new(std::io::Cursor::new(&pinned.weights))),
            None => Ok(Box::new(std::fs::File::open(&self.gguf_path)?)),
        }
    }

    fn tokenizer(&self) -> anyhow::Result<tokenizers::Tokenizer> {
        match &self.pinned {
            Some(pinned) => tokenizers::Tokenizer::from_bytes(&pinned.tokenizer),
            None => tokenizers::Tokenizer::from_file(&self.tokenizer_path),
        }
        .map_err(|error| anyhow::anyhow!("load tokenizer: {error}"))
    }
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
                "tokenizer not found: {} — run `newt models pull {}` (it fetches tokenizer.json \
                 from https://huggingface.co/{} next to the GGUF; the quant GGUF repo does not \
                 ship one)",
                tokenizer_path.display(),
                model.name,
                model.tokenizer_repo
            );
        }
        Ok(Self {
            name: format!("embedded:{}", model.name),
            model,
            assets: ModelAssets {
                gguf_path,
                tokenizer_path,
                pinned: None,
            },
            device: None,
        })
    }

    /// Build an auxiliary on CPU regardless of ambient accelerator settings.
    pub fn new_cpu(model_name: &str, gguf_path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let mut backend = Self::new(model_name, gguf_path)?;
        backend.device = Some(candle_core::Device::Cpu);
        backend.assets.pinned = Some(Arc::new(PinnedAssets {
            weights: std::fs::read(&backend.assets.gguf_path)?,
            tokenizer: std::fs::read(&backend.assets.tokenizer_path)?,
        }));
        Ok(backend)
    }

    /// Identities of the immutable bytes the CPU constructor retained for inference.
    #[must_use]
    pub fn pinned_asset_ids(
        &self,
    ) -> Option<(
        content_addressable::RawContentId,
        content_addressable::RawContentId,
    )> {
        self.assets.pinned.as_ref().map(|assets| {
            (
                content_addressable::RawContentId::from_content(&assets.weights),
                content_addressable::RawContentId::from_content(&assets.tokenizer),
            )
        })
    }

    /// The resolved palette model.
    #[must_use]
    pub fn model(&self) -> &MiniModel {
        self.model
    }

    /// Complete with a budget covering admission, model loading and generation.
    /// Returning on timeout or dropping this future signals cooperative worker
    /// cancellation; one synchronous load/forward may still be finishing.
    ///
    /// # Errors
    /// An unrepresentable or expired deadline, busy worker, or inference error.
    pub async fn complete_with_timeout(
        &self,
        req: ChatRequest,
        timeout: std::time::Duration,
    ) -> anyhow::Result<ChatReply> {
        let deadline = std::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow::anyhow!("embedded inference deadline is out of range"))?;
        self.complete_until(req, Some(deadline)).await
    }

    async fn complete_until(
        &self,
        req: ChatRequest,
        deadline: Option<std::time::Instant>,
    ) -> anyhow::Result<ChatReply> {
        let max_tokens = req.max_tokens.unwrap_or(512) as usize;
        let prompt = engine::format_chatml(&req.messages);
        let assets = self.assets.clone();
        let arch = self.model.arch;
        let model_id = self.model.name.to_string();
        let device = self.device.clone();
        // candle is synchronous + CPU/GPU-bound; keep it off the async runtime.
        let content = run_generation(generation_admission(), deadline, move |checkpoint| {
            let device = device.map(Ok).unwrap_or_else(engine::device)?;
            engine::generate(&assets, arch, &prompt, max_tokens, &device, checkpoint)
        })
        .await?;
        Ok(ChatReply {
            content,
            model_id,
            usage: None,
        })
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
        self.complete_until(req, None).await
    }
}

// Process-wide: rebuilding a per-turn backend must not admit another model
// while a cancelled call is still finishing its current synchronous step.
fn generation_admission() -> std::sync::Arc<tokio::sync::Semaphore> {
    static ADMISSION: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    ADMISSION
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
}

/// Cancellation is cooperative, not a hard wall-clock interruption of Candle:
/// one synchronous load/forward may finish after the caller returns. The worker
/// keeps admission until it actually exits, including on panic; busy calls fail
/// rather than building an unbounded blocking-task queue.
async fn run_generation<F>(
    admission: std::sync::Arc<tokio::sync::Semaphore>,
    deadline: Option<std::time::Instant>,
    generate: F,
) -> anyhow::Result<String>
where
    F: FnOnce(&dyn Fn() -> anyhow::Result<()>) -> anyhow::Result<String> + Send + 'static,
{
    anyhow::ensure!(
        deadline.is_none_or(|end| std::time::Instant::now() < end),
        "embedded inference deadline exceeded"
    );
    let permit = admission.try_acquire_owned().map_err(|_| {
        anyhow::anyhow!("embedded inference busy: another generation is still running")
    })?;
    // Dropping the caller's actual future drops this receiver. The synchronous
    // worker can observe that through the existing oneshot channel, without a
    // second cancellation protocol or an unsafe attempt to kill its thread.
    let (caller, _caller_lifetime) = tokio::sync::oneshot::channel::<()>();
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let checkpoint = || {
            anyhow::ensure!(!caller.is_closed(), "embedded inference cancelled");
            anyhow::ensure!(
                deadline.is_none_or(|end| std::time::Instant::now() < end),
                "embedded inference deadline exceeded"
            );
            Ok(())
        };
        checkpoint()?;
        let result = generate(&checkpoint);
        checkpoint()?;
        result
    });
    let result = match deadline {
        Some(end) => tokio::time::timeout_at(tokio::time::Instant::from_std(end), worker)
            .await
            .map_err(|_| anyhow::anyhow!(
                "embedded inference deadline exceeded; the current synchronous step may still be finishing"
            ))?,
        None => worker.await,
    };
    result.map_err(|e| anyhow::anyhow!("embedded inference task panicked: {e}"))?
}

/// The candle generation engine (synchronous; called via `spawn_blocking`).
pub(crate) mod engine {
    use anyhow::Context;
    use candle_core::quantized::gguf_file;
    use candle_core::{Device, Tensor};
    use candle_transformers::generation::LogitsProcessor;
    use candle_transformers::models::quantized_qwen2::ModelWeights as Qwen2;

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
    ///
    /// Shared (`pub(crate)`) so the embedded [`Embedder`](crate::embed) reuses the
    /// exact same non-contending device policy — one place decides CPU-vs-GPU.
    pub(crate) fn device() -> anyhow::Result<Device> {
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
        assets: &super::ModelAssets,
        arch: ModelArch,
        prompt: &str,
        max_tokens: usize,
        device: &Device,
        checkpoint: &dyn Fn() -> anyhow::Result<()>,
    ) -> anyhow::Result<String> {
        checkpoint()?;
        if arch != ModelArch::Qwen2 {
            anyhow::bail!(
                "the embedded engine currently supports the Qwen2 architecture only \
                 (use a qwen2.5-* model); {arch:?} support is a follow-up"
            );
        }
        checkpoint()?;
        let tokenizer = assets.tokenizer()?;
        checkpoint()?;
        let mut file = assets.weights()?;
        let content = gguf_file::Content::read(&mut file).context("read GGUF assets")?;
        checkpoint()?;
        let mut model =
            Qwen2::from_gguf(content, &mut file, device).context("load Qwen2 weights from GGUF")?;
        checkpoint()?;
        let encoding = tokenizer
            .encode(prompt, true)
            .map_err(|e| anyhow::anyhow!("tokenize prompt: {e}"))?;
        checkpoint()?;
        let prompt_tokens: Vec<u32> = encoding.get_ids().to_vec();
        // Qwen2 ChatML end-of-turn; fall back to <|endoftext|>.
        let eos = tokenizer
            .token_to_id("<|im_end|>")
            .or_else(|| tokenizer.token_to_id("<|endoftext|>"))
            .unwrap_or(151_645);

        let mut logits_processor = LogitsProcessor::new(42, Some(0.2), None);
        let generated = generate_tokens(
            prompt_tokens,
            eos,
            max_tokens,
            checkpoint,
            |next, pos| {
                let input = Tensor::new(next, device)?.unsqueeze(0)?;
                let logits = model.forward(&input, pos)?.squeeze(0)?;
                Ok(if logits.rank() == 2 {
                    logits.get(logits.dim(0)? - 1)?
                } else {
                    logits
                })
            },
            |logits| Ok(logits_processor.sample(logits)?),
        )?;
        checkpoint()?;
        tokenizer
            .decode(&generated, true)
            .map_err(|e| anyhow::anyhow!("decode reply: {e}"))
    }

    pub(super) fn generate_tokens(
        prompt_tokens: Vec<u32>,
        eos: u32,
        max_tokens: usize,
        checkpoint: &dyn Fn() -> anyhow::Result<()>,
        mut forward: impl FnMut(&[u32], usize) -> anyhow::Result<Tensor>,
        mut sample: impl FnMut(&Tensor) -> anyhow::Result<u32>,
    ) -> anyhow::Result<Vec<u32>> {
        let mut generated = Vec::new();
        if max_tokens == 0 {
            return Ok(generated);
        }
        let (last, prefix) = prompt_tokens
            .split_last()
            .context("empty embedded prompt")?;
        // Candle 0.8's Qwen2 mask is square over the current sequence, not the
        // cached prefix plus that sequence. Single-token prefill avoids that
        // multi-token chunk mismatch and gives cancellation a checkpoint per
        // forward. It trades batch throughput for a smaller synchronous quantum.
        // Prompt EOS tokens are input, and prefill must not consume RNG draws.
        for (pos, token) in prefix.iter().enumerate() {
            checkpoint()?;
            let _ = forward(std::slice::from_ref(token), pos)?;
            checkpoint()?;
        }
        let mut next = *last;
        for pos in (prefix.len()..).take(max_tokens) {
            checkpoint()?;
            let logits = forward(std::slice::from_ref(&next), pos)?;
            checkpoint()?;
            let token = sample(&logits)?;
            checkpoint()?;
            if token == eos {
                break;
            }
            generated.push(token);
            next = token;
        }
        Ok(generated)
    }
}

#[cfg(test)]
#[path = "embedded_lifecycle_tests.rs"]
mod lifecycle_tests;

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

    /// Grounds the CPU-only auxiliary choice in the actual engine device value;
    /// no environment mutation or accelerator availability is needed.
    #[test]
    fn new_cpu_pins_the_device_passed_to_generation() {
        let dir = tempfile::tempdir().unwrap();
        let gguf = dir.path().join("model.gguf");
        std::fs::write(&gguf, b"placeholder").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"{}").unwrap();
        let backend = EmbeddedBackend::new_cpu("qwen2.5-0.5b", gguf).unwrap();
        assert!(matches!(backend.device, Some(candle_core::Device::Cpu)));
    }

    /// Grounds manifest identity in the exact reader/tokenizer used by the
    /// generation engine, even when the original filesystem paths change.
    #[test]
    fn cpu_assets_remain_bound_to_the_recorded_bytes_after_path_substitution() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let gguf = dir.path().join("model.gguf");
        let tokenizer = dir.path().join("tokenizer.json");
        std::fs::write(&gguf, b"original weights").unwrap();
        tokenizers::Tokenizer::new(tokenizers::models::bpe::BPE::default())
            .save(&tokenizer, false)
            .unwrap();
        let backend = EmbeddedBackend::new_cpu("qwen2.5-0.5b", &gguf).unwrap();
        let ids = backend.pinned_asset_ids().unwrap();
        std::fs::write(&gguf, b"replacement weights").unwrap();
        std::fs::write(&tokenizer, b"invalid tokenizer").unwrap();
        let mut bytes = Vec::new();
        backend
            .assets
            .weights()
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"original weights");
        assert_eq!(
            ids.0,
            content_addressable::RawContentId::from_content(&bytes)
        );
        assert_eq!(backend.pinned_asset_ids(), Some(ids));
        assert!(backend.assets.tokenizer().is_ok());
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
        let be = EmbeddedBackend::new_cpu("qwen2.5-0.5b", &gguf).unwrap();
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
