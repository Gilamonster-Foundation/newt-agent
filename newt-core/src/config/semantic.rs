//! Embedding backend, retrieval depth, and indexing-failure settings.

use serde::{Deserialize, Serialize};

use super::BackendKind;

/// `[context.semantic]` — the embedding RAG-for-code feature's settings (Step
/// 26.5.4, #582).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticConfig {
    /// Embedding model used to index the repo + embed queries. Default
    /// `nomic-embed-text` (the HTTP path). The model must exist on the embeddings
    /// endpoint (see `embeddings_endpoint`); when it can't be reached the feature
    /// follows `on_embed_failure`.
    ///
    /// For the **embedded backend** (`embeddings_api = "embedded"`, #720) this is
    /// only a label — the model is loaded from `embedding_model_path` — and it
    /// should name a **candle-clean standard-BERT** model (e.g.
    /// `bge-small-en-v1.5`), NOT `nomic-embed-text`, which candle 0.8 cannot load.
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Local model **directory** for the embedded embedder (#720): a
    /// candle-clean standard-BERT model dir holding
    /// `config.json` + `tokenizer.json` + `model.safetensors` (e.g. a fetched
    /// `BAAI/bge-small-en-v1.5`). `None` (default) ⇒ the embedded path can't
    /// load and reports a clear error. When `embeddings_api` and
    /// `embeddings_endpoint` are unset, a configured path selects embedded
    /// embeddings automatically. Ignored by explicit HTTP embeddings targets.
    /// Mirrors the summarizer's `model_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model_path: Option<String>,
    /// How many code chunks to retrieve per turn. Default 5.
    #[serde(default = "default_semantic_top_k")]
    pub top_k: usize,
    /// Dedicated endpoint that serves embeddings (e.g. an Ollama
    /// `http://host:11434`). `None` (default) leaves semantic retrieval on the
    /// embedded path unless `embeddings_api` explicitly selects an HTTP protocol.
    /// Set this to a real embeddings host when remote/vector-server embeddings
    /// are a deliberate performance choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings_endpoint: Option<String>,
    /// Wire protocol of `embeddings_endpoint` — `ollama` (`/api/embeddings`) or
    /// `openai` (`/v1/embeddings`). `embedded` selects the in-process embedder.
    /// `None` (default) selects embedded embeddings when `embeddings_endpoint`
    /// is also unset; with an explicit endpoint, `None` assumes `ollama`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings_api: Option<BackendKind>,
    /// What to do when embedding fails structurally (wrong endpoint / model
    /// absent): `disable` (default) stops indexing after the first failure with
    /// one actionable message; `warn` logs per-chunk and keeps trying.
    #[serde(default)]
    pub on_embed_failure: OnEmbedFailure,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            embedding_model: default_embedding_model(),
            embedding_model_path: None,
            top_k: default_semantic_top_k(),
            embeddings_endpoint: None,
            embeddings_api: None,
            on_embed_failure: OnEmbedFailure::default(),
        }
    }
}

/// Policy when an embedding request fails structurally during indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnEmbedFailure {
    /// Stop indexing on the first failure and log one actionable error — a
    /// structural failure (wrong endpoint / missing model) is total, not
    /// transient, so degrading per-chunk just produces an empty index quietly.
    #[default]
    Disable,
    /// Log every failed chunk and keep going (the historical behaviour).
    Warn,
}

fn default_embedding_model() -> String {
    "nomic-embed-text".to_string()
}

fn default_semantic_top_k() -> usize {
    5
}
