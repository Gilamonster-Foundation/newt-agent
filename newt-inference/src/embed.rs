//! In-process **embedder** (#720) — the opt-in `embedded` cargo feature.
//!
//! [`CandleEmbedder`] is the durable answer to "embeddings fail when the model
//! isn't pulled, and pulling onto the DGX evicts the chat model": it computes
//! semantic-retrieval embeddings **in process** on the laptop (pure-Rust
//! **candle**), so retrieval never touches the DGX chat model's VRAM. It
//! implements [`newt_core::Embedder`], the same seam the HTTP `EmbeddingsClient`
//! does, so indexing + retrieval are unchanged — only the construction branches.
//!
//! ## Why a standard-BERT safetensors model (not `nomic-embed-text`)
//!
//! candle-transformers 0.8 ships a standard [`bert::BertModel`] but **no**
//! `nomic_bert` module (nomic-embed-text uses rotary + SwiGLU a standard BERT
//! can't load), and candle has **no quantized BERT** (so a GGUF embedder won't
//! load either). The embedded backend therefore loads a **candle-clean standard
//! BERT in safetensors** — default [`BAAI/bge-small-en-v1.5`](https://huggingface.co/BAAI/bge-small-en-v1.5)
//! (384-dim). `nomic-embed-text` stays available over the existing HTTP path.
//!
//! ## Device + threading discipline (mirrors [`EmbeddedBackend`](crate::embedded))
//!
//! - **CPU by default**, never contending the GPU the primary model uses
//!   (`NEWT_EMBEDDED_DEVICE = cpu|metal|cuda|auto`; same selector as the
//!   generation engine).
//! - **No silent download.** The model files must be pre-placed in a local dir;
//!   an absent dir/file is a clear error naming the dir and the HF repo, never a
//!   download into a small box.
//! - candle is synchronous and a loaded model holds device handles that are not
//!   guaranteed `Send`/`Sync` across every accelerator. The model therefore lives
//!   on a **dedicated worker thread** (loaded once), and [`CandleEmbedder`] holds
//!   only a channel `Sender` (always `Send + Sync`). `embed` ships the text to the
//!   worker and awaits the reply — the forward runs off the async runtime, and the
//!   model is loaded exactly once for the whole session (not per chunk).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use newt_core::Embedder;
use tokenizers::Tokenizer;

/// The default candle-clean embedding model — a standard BERT in safetensors
/// (384-dim). Named in the no-files error so the human knows what to place.
pub const DEFAULT_EMBED_HF_REPO: &str = "BAAI/bge-small-en-v1.5";

/// Files a candle standard-BERT model dir must contain (config + tokenizer +
/// safetensors weights). Checked in `new` so the no-download contract is total.
const REQUIRED_FILES: &[&str] = &["config.json", "tokenizer.json", "model.safetensors"];

/// One embed request handed to the worker thread, with a one-shot reply channel.
struct EmbedJob {
    text: String,
    reply: tokio::sync::oneshot::Sender<anyhow::Result<Vec<f32>>>,
}

/// An in-process [`Embedder`] over a candle standard-BERT model (#720).
///
/// Construction validates the model dir (no download); the heavy load happens
/// once on a dedicated worker thread. `embed` is a channel round-trip, so the
/// type is trivially `Send + Sync` regardless of the chosen device.
#[derive(Debug)]
pub struct CandleEmbedder {
    /// Human label (the configured `embedding_model`), used in error messages.
    label: String,
    /// Hands work to the model's dedicated thread.
    tx: tokio::sync::mpsc::Sender<EmbedJob>,
}

impl CandleEmbedder {
    /// Build an embedder from a **local model dir** holding a candle-clean
    /// standard-BERT model (`config.json` + `tokenizer.json` +
    /// `model.safetensors`). `model_label` is the configured `embedding_model`
    /// (informational — the architecture comes from `config.json`).
    ///
    /// Validates that the dir and every required file exist **without
    /// downloading anything** (#639's "no silent download into a small box"),
    /// then spawns the worker thread that owns the loaded model.
    ///
    /// # Errors
    /// The dir is missing / not a directory, or a required file is absent.
    pub fn new(
        model_label: impl Into<String>,
        model_dir: impl Into<PathBuf>,
    ) -> anyhow::Result<Self> {
        let label = model_label.into();
        let dir = model_dir.into();
        if !dir.is_dir() {
            anyhow::bail!(
                "embedded embedder: model dir not found: {} — place a candle-clean standard-BERT \
                 model there ({}: config.json + tokenizer.json + model.safetensors). Nothing is \
                 auto-downloaded.",
                dir.display(),
                DEFAULT_EMBED_HF_REPO
            );
        }
        for file in REQUIRED_FILES {
            let p = dir.join(file);
            if !p.exists() {
                anyhow::bail!(
                    "embedded embedder: required file not found: {} — fetch a candle-clean \
                     standard-BERT model (e.g. {}) into {}. Nothing is auto-downloaded.",
                    p.display(),
                    DEFAULT_EMBED_HF_REPO,
                    dir.display()
                );
            }
        }

        // Bounded so a runaway producer can't grow the queue without bound; the
        // indexer embeds sequentially, so a small buffer is ample.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<EmbedJob>(64);
        let worker_dir = dir.clone();
        let worker_label = label.clone();
        std::thread::Builder::new()
            .name("newt-candle-embedder".to_string())
            .spawn(move || {
                // Load once on this thread; the model never crosses a thread
                // boundary (sidesteps device-handle Send/Sync entirely).
                match load_model(&worker_dir) {
                    Ok((model, tokenizer, device)) => {
                        while let Some(job) = rx.blocking_recv() {
                            let res = embed_one(&model, &tokenizer, &device, &job.text);
                            let _ = job.reply.send(res);
                        }
                    }
                    Err(e) => {
                        // Loading failed — fail every request with the reason
                        // (rather than dropping the channel into an opaque
                        // "worker stopped"), then drain so senders unblock.
                        let msg = format!("embedded embedder '{worker_label}' failed to load: {e}");
                        while let Some(job) = rx.blocking_recv() {
                            let _ = job.reply.send(Err(anyhow::anyhow!(msg.clone())));
                        }
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("spawn embedded embedder worker thread: {e}"))?;

        Ok(Self { label, tx })
    }
}

#[async_trait]
impl Embedder for CandleEmbedder {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let (reply, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(EmbedJob {
                text: text.to_string(),
                reply,
            })
            .await
            .map_err(|_| {
                anyhow::anyhow!("embedded embedder '{}' worker has stopped", self.label)
            })?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("embedded embedder '{}' dropped a reply", self.label))?
    }
}

/// Load the candle standard-BERT model (config + tokenizer + mmaped
/// safetensors) on the calling (worker) thread, choosing the same
/// non-contending device the generation engine uses.
fn load_model(dir: &Path) -> anyhow::Result<(BertModel, Tokenizer, Device)> {
    let device = crate::embedded::engine::device()?;
    let config_json = std::fs::read_to_string(dir.join("config.json"))
        .map_err(|e| anyhow::anyhow!("read {}: {e}", dir.join("config.json").display()))?;
    let config: Config = serde_json::from_str(&config_json)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", dir.join("config.json").display()))?;
    let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(anyhow::Error::msg)?;
    // SAFETY: `from_mmaped_safetensors` mmaps the weights read-only; the file
    // outlives the model (it is only read on this worker thread) and is not
    // mutated elsewhere, satisfying candle's mmap contract.
    let vb = unsafe {
        VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DTYPE, &device)?
    };
    let model = BertModel::load(vb, &config)?;
    Ok((model, tokenizer, device))
}

/// Embed one text: tokenize → forward → **mean-pool** over tokens → **L2
/// normalize**. The standard sentence-embedding recipe for a BERT encoder.
fn embed_one(
    model: &BertModel,
    tokenizer: &Tokenizer,
    device: &Device,
    text: &str,
) -> anyhow::Result<Vec<f32>> {
    let encoding = tokenizer
        .encode(text, true)
        .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
    let ids = encoding.get_ids().to_vec();
    if ids.is_empty() {
        anyhow::bail!("embedded embedder: text tokenized to zero tokens");
    }
    let input_ids = Tensor::new(ids.as_slice(), device)?.unsqueeze(0)?; // (1, seq)
    let token_type_ids = input_ids.zeros_like()?; // BERT segment ids: all zeros
    let hidden = model.forward(&input_ids, &token_type_ids, None)?; // (1, seq, hidden)
    let (_b, n, _h) = hidden.dims3()?;
    let pooled = (hidden.sum(1)? / (n as f64))?; // mean-pool → (1, hidden)
    let normalized = normalize_l2(&pooled)?;
    let out: Vec<f32> = normalized.squeeze(0)?.to_vec1::<f32>()?;
    Ok(out)
}

/// L2-normalize each row of a 2-D tensor (`v / ||v||`).
fn normalize_l2(v: &Tensor) -> candle_core::Result<Tensor> {
    v.broadcast_div(&v.sqr()?.sum_keepdim(1)?.sqrt()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_a_missing_model_dir_without_downloading() {
        // A path guaranteed not to exist — no temp dir is created (fs-free), and
        // nothing must be fetched: the contract is "pre-place or fail clearly".
        let err =
            CandleEmbedder::new("bge-small-en-v1.5", "/nonexistent/newt-bge-dir").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("model dir not found"), "got: {msg}");
        assert!(msg.contains("Nothing is auto-downloaded"), "got: {msg}");
        // The error must name the HF repo so the human knows what to place.
        assert!(msg.contains(DEFAULT_EMBED_HF_REPO), "got: {msg}");
    }

    #[test]
    fn new_names_the_dir_and_default_repo_in_the_error() {
        let err = CandleEmbedder::new("any-label", "/nonexistent/some-embed-model").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/nonexistent/some-embed-model"), "got: {msg}");
        assert!(msg.contains(DEFAULT_EMBED_HF_REPO), "got: {msg}");
    }

    /// On-device smoke test: load a real candle standard-BERT model dir
    /// (`config.json` + `tokenizer.json` + `model.safetensors`, e.g.
    /// `BAAI/bge-small-en-v1.5`) and embed. `#[ignore]` (needs the files);
    /// run with
    /// `NEWT_EMBED_SMOKE_DIR=/path/to/bge-small-en-v1.5 \
    ///  cargo test -p newt-inference --features embedded smoke_embed -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs a real candle BERT model dir; set NEWT_EMBED_SMOKE_DIR"]
    async fn smoke_embed_on_cpu() {
        let dir = std::env::var("NEWT_EMBED_SMOKE_DIR").expect(
            "set NEWT_EMBED_SMOKE_DIR to a candle standard-BERT model dir \
             (config.json + tokenizer.json + model.safetensors)",
        );
        let embedder = CandleEmbedder::new("bge-small-en-v1.5", &dir).unwrap();
        let v = embedder.embed("hello world").await.unwrap();
        eprintln!("EMBED SMOKE: dim={}", v.len());
        assert!(!v.is_empty(), "expected a non-empty embedding vector");
        // L2-normalized → unit length.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "expected unit-norm, got {norm}");
    }
}
