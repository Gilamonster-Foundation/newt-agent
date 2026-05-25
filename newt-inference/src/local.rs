//! Local inference backends — the only backends compiled into the default
//! Newt binary. Cloud APIs live behind opt-in `ProviderPluginBackend`.

use async_trait::async_trait;
use newt_core::router::Tier;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};

pub struct LocalOllamaBackend {
    endpoint: String,
    model: String,
}

impl LocalOllamaBackend {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl InferenceBackend for LocalOllamaBackend {
    fn name(&self) -> &str {
        "ollama-local"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn supports_tier(&self, _tier: Tier) -> bool {
        true
    }

    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatReply> {
        anyhow::bail!(
            "LocalOllamaBackend.complete not yet implemented (endpoint={}, model={})",
            self.endpoint,
            self.model
        )
    }
}

pub struct LocalVllmBackend {
    endpoint: String,
    model: String,
}

impl LocalVllmBackend {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }
}

#[async_trait]
impl InferenceBackend for LocalVllmBackend {
    fn name(&self) -> &str {
        "vllm-local"
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn supports_tier(&self, _tier: Tier) -> bool {
        true
    }

    async fn complete(&self, _req: ChatRequest) -> anyhow::Result<ChatReply> {
        anyhow::bail!(
            "LocalVllmBackend.complete not yet implemented (endpoint={}, model={})",
            self.endpoint,
            self.model
        )
    }
}
