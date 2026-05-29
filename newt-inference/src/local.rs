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
        // If OLLAMA_HOST is explicitly set, use it VERBATIM — no probe,
        // no fallback. User intent overrides discovery. This eliminates
        // the silent-fallthrough foot-gun where a stale env var (or a
        // mocked endpoint missing /api/tags) causes discover() to hit
        // a different Ollama than the user asked for.
        //
        // Use `discover_strict` if you want the env host to be probed.
        if let Some(host) = env_host {
            tracing::info!(
                endpoint = %host,
                "Ollama endpoint chosen via OLLAMA_HOST (verbatim, not probed)"
            );
            return Ok(Self::new(host, model));
        }

        let probe_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(500))
            .build()?;

        for endpoint in candidates {
            if Self::probe(&probe_client, endpoint).await {
                tracing::info!(endpoint = %endpoint, "Ollama endpoint chosen by discovery probe");
                return Ok(Self::new(endpoint, model));
            }
        }

        anyhow::bail!(
            "no reachable Ollama endpoint found (tried {} candidates)",
            candidates.len()
        )
    }

    /// Like [`discover`](Self::discover) but requires successful
    /// probing — no silent fallthrough. Even if `OLLAMA_HOST` is
    /// set, the probe must succeed. Use this in tests and CI to
    /// assert a specific endpoint is reachable rather than just
    /// trusted by env-var contract.
    pub async fn discover_strict(model: &str) -> anyhow::Result<Self> {
        let env_host = std::env::var("OLLAMA_HOST").ok();
        Self::discover_strict_with_env(model, env_host.as_deref(), &Self::default_endpoints()).await
    }

    /// Like [`discover_strict`](Self::discover_strict), but with an
    /// explicit env-host override and candidate list. Useful for
    /// testing without mutating process-global environment variables.
    pub async fn discover_strict_with_env(
        model: &str,
        env_host: Option<&str>,
        candidates: &[String],
    ) -> anyhow::Result<Self> {
        // Build the full candidate list with the env-host (if any) at
        // the front. We probe every candidate, including the env-host.
        let all_candidates: Vec<&str> = env_host
            .into_iter()
            .chain(candidates.iter().map(String::as_str))
            .collect();

        let probe_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(500))
            .build()?;

        for endpoint in &all_candidates {
            if Self::probe(&probe_client, endpoint).await {
                tracing::info!(endpoint = %endpoint, "Ollama endpoint chosen (strict)");
                return Ok(Self::new(*endpoint, model));
            }
        }

        anyhow::bail!(
            "discover_strict: no reachable Ollama endpoint (tried {} candidates)",
            all_candidates.len()
        )
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

/// A backend that speaks the OpenAI-compatible HTTP API exposed by a
/// local vLLM server (`POST /v1/chat/completions`).
///
/// vLLM endpoints are explicit — unlike Ollama, vLLM has no canonical
/// default port, so we deliberately skip endpoint auto-discovery here.
/// Callers must supply the endpoint via config or CLI flag.
#[derive(Debug)]
pub struct LocalVllmBackend {
    endpoint: String,
    model: String,
    client: reqwest::Client,
}

impl LocalVllmBackend {
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

    /// Single HTTP attempt — no retries. Returns a structured error that
    /// [`is_retryable`](Self::is_retryable) can classify.
    async fn try_complete(&self, req: &ChatRequest) -> anyhow::Result<ChatReply> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": req.messages.iter().map(|m| {
                serde_json::json!({ "role": &m.role, "content": &m.content })
            }).collect::<Vec<_>>(),
            "stream": false,
        });
        if let Some(max) = req.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }

        let url = format!(
            "{}/v1/chat/completions",
            self.endpoint.trim_end_matches('/')
        );
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("vLLM request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("vLLM returned {status}: {text}");
        }

        let json: serde_json::Value = resp.json().await?;
        // OpenAI-compatible: choices[0].message.content
        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        // Prefer the model echoed back by the server (helps callers
        // distinguish aliases) but fall back to the configured id.
        let model_id = json["model"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.model.clone());

        Ok(ChatReply { content, model_id })
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
        // 5xx status codes extracted from our "vLLM returned {status}" message.
        if let Some(rest) = msg.strip_prefix("vLLM returned ") {
            if let Some(code_str) = rest.split_whitespace().next() {
                if let Ok(code) = code_str.parse::<u16>() {
                    return (500..600).contains(&code);
                }
            }
        }
        false
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
                    tracing::warn!(attempt, error = %e, "retrying vLLM request");
                    last_err = e;
                }
            }
        }

        Err(last_err)
    }
}
