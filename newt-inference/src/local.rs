//! Local inference backends — the only backends compiled into the default
//! Newt binary. Cloud APIs live behind opt-in `ProviderPluginBackend`.

use async_trait::async_trait;
use newt_core::router::Tier;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};

pub struct LocalOllamaBackend {
    endpoint: String,
    model: String,
    client: reqwest::Client,
}

impl LocalOllamaBackend {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Override the HTTP client timeout. Useful for testing.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("build client");
        self
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

    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatReply> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": req.messages.iter().map(|m| {
                serde_json::json!({ "role": &m.role, "content": &m.content })
            }).collect::<Vec<_>>(),
            "stream": false,
            "options": req.max_tokens.map(|t| serde_json::json!({ "num_predict": t })),
        });

        let url = format!("{}/api/chat", self.endpoint.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Ollama request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama returned {status}: {text}");
        }

        let json: serde_json::Value = resp.json().await?;
        let content = json["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        Ok(ChatReply {
            content,
            model_id: self.model.clone(),
        })
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
