//! Local inference backends — the only backends compiled into the default
//! Newt binary. Cloud APIs live behind opt-in `ProviderPluginBackend`.

use async_trait::async_trait;
use newt_core::router::Tier;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};

#[derive(Debug)]
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

    /// Return the configured endpoint URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Override the HTTP client timeout. Useful for testing.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("build client");
        self
    }

    /// Try endpoints in order, return the first reachable one.
    /// Reachability = GET /api/tags returns 2xx within 500ms.
    /// Checks `OLLAMA_HOST` env var first, then the default endpoint list.
    pub async fn discover(model: &str) -> anyhow::Result<Self> {
        let env_host = std::env::var("OLLAMA_HOST").ok();
        Self::discover_inner(model, env_host.as_deref(), &Self::default_endpoints()).await
    }

    /// Like [`discover`](Self::discover), but with a caller-supplied candidate
    /// list instead of the built-in defaults. `OLLAMA_HOST` is still checked
    /// first.
    pub async fn discover_with_candidates(
        model: &str,
        candidates: &[String],
    ) -> anyhow::Result<Self> {
        let env_host = std::env::var("OLLAMA_HOST").ok();
        Self::discover_inner(model, env_host.as_deref(), candidates).await
    }

    /// Like [`discover`](Self::discover), but with an explicit env-host
    /// override and candidate list. Useful for testing without mutating
    /// process-global environment variables.
    pub async fn discover_with_env(
        model: &str,
        env_host: Option<&str>,
        candidates: &[String],
    ) -> anyhow::Result<Self> {
        Self::discover_inner(model, env_host, candidates).await
    }

    async fn discover_inner(
        model: &str,
        env_host: Option<&str>,
        candidates: &[String],
    ) -> anyhow::Result<Self> {
        let probe_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(500))
            .build()?;

        if let Some(host) = env_host {
            if Self::probe(&probe_client, host).await {
                return Ok(Self::new(host, model));
            }
        }

        for endpoint in candidates {
            if Self::probe(&probe_client, endpoint).await {
                return Ok(Self::new(endpoint, model));
            }
        }

        anyhow::bail!("no reachable Ollama endpoint found")
    }

    /// The built-in fallback endpoint list for [`discover`](Self::discover).
    pub fn default_endpoints() -> Vec<String> {
        vec![
            "http://ollama-proxy.inference.svc.cluster.local:11434".to_string(),
            "http://ollama.home.lab:11434".to_string(),
            "http://dgx-ollama.home.lab:11434".to_string(),
            "http://gnuc-ollama.home.lab:11434".to_string(),
            "http://127.0.0.1:11434".to_string(),
        ]
    }

    async fn probe(client: &reqwest::Client, endpoint: &str) -> bool {
        let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
        match client.get(&url).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Single HTTP attempt — no retries. Returns a structured error that
    /// [`is_retryable`](Self::is_retryable) can classify.
    async fn try_complete(&self, req: &ChatRequest) -> anyhow::Result<ChatReply> {
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

    /// Returns `true` for errors worth retrying: connection failures and 5xx
    /// status codes. Returns `false` for 4xx (client errors) which won't
    /// succeed on retry.
    fn is_retryable(err: &anyhow::Error) -> bool {
        let msg = err.to_string();
        // Connection / timeout errors from reqwest.
        if msg.contains("request failed") {
            return true;
        }
        // 5xx status codes extracted from our "Ollama returned {status}" message.
        if let Some(rest) = msg.strip_prefix("Ollama returned ") {
            if let Some(code_str) = rest.split_whitespace().next() {
                // Handle both "503 Service Unavailable" and bare "503"
                if let Ok(code) = code_str.parse::<u16>() {
                    return (500..600).contains(&code);
                }
            }
        }
        false
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
        let retry_delays_ms: &[u64] = &[250, 500, 1000];
        let mut last_err = anyhow::anyhow!("no attempts made");

        for (attempt, delay_ms) in std::iter::once(0)
            .chain(retry_delays_ms.iter().copied())
            .enumerate()
        {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            match self.try_complete(&req).await {
                Ok(reply) => return Ok(reply),
                Err(e) => {
                    if !Self::is_retryable(&e) {
                        return Err(e);
                    }
                    tracing::warn!(attempt, error = %e, "retrying Ollama request");
                    last_err = e;
                }
            }
        }

        Err(last_err)
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
