//! Local inference backends — the only backends compiled into the default
//! Newt binary. Cloud APIs live behind opt-in `ProviderPluginBackend`.

use async_trait::async_trait;
use newt_core::router::Tier;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};
use crate::retry::{with_backoff, RetryPolicy};

#[derive(Debug)]
pub struct LocalOllamaBackend {
    endpoint: String,
    model: String,
    client: reqwest::Client,
    retry: RetryPolicy,
}

impl LocalOllamaBackend {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            client: reqwest::Client::new(),
            retry: RetryPolicy::from_env(),
        }
    }

    /// Override the retry/backoff policy (defaults to [`RetryPolicy::from_env`]).
    /// Used by tests to inject a zero-delay policy; production callers can tune
    /// it via the `NEWT_HTTP_*` env vars instead.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
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
            "http://REDACTED-HOST:11434".to_string(),
            "http://REDACTED-HOST:11434".to_string(),
            "http://REDACTED-HOST:11434".to_string(),
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
    /// [`crate::retry::classify`] can classify for the backoff loop.
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
        // #385: strip inline <think>…</think> reasoning from the content.
        let (content, _reasoning) =
            newt_core::split_reasoning(json["message"]["content"].as_str().unwrap_or(""));

        // Extract token counts from Ollama's response if present.
        let usage = {
            let input = json["prompt_eval_count"].as_u64().map(|n| n as u32);
            let output = json["eval_count"].as_u64().map(|n| n as u32);
            input.zip(output).map(|(i, o)| newt_core::TokenUsage {
                input_tokens: i,
                output_tokens: o,
            })
        };

        Ok(ChatReply {
            content,
            model_id: self.model.clone(),
            usage,
        })
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

    fn endpoint(&self) -> Option<&str> {
        Some(&self.endpoint)
    }

    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatReply> {
        with_backoff(&self.retry, || self.try_complete(&req)).await
    }
}

/// Metadata for a single model exposed by a vLLM server's `/v1/models`
/// endpoint. The shape mirrors the OpenAI-compatible response — we
/// only surface the fields the rest of Newt cares about today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
}

/// A backend that speaks the OpenAI-compatible HTTP API exposed by a
/// local vLLM server (`POST /v1/chat/completions`, `GET /v1/models`).
///
/// vLLM endpoints are explicit — unlike Ollama, vLLM has no canonical
/// default port, so we deliberately skip endpoint auto-discovery here.
/// Callers must supply the endpoint via config or CLI flag.
#[derive(Debug)]
pub struct LocalVllmBackend {
    endpoint: String,
    model: String,
    client: reqwest::Client,
    /// Optional bearer token sent as `Authorization: Bearer <token>`.
    /// `None` for unauthenticated local servers (the default); `Some`
    /// for hosted OpenAI-compatible endpoints that require an API key.
    api_key: Option<String>,
    retry: RetryPolicy,
}

impl LocalVllmBackend {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            client: reqwest::Client::new(),
            api_key: None,
            retry: RetryPolicy::from_env(),
        }
    }

    /// Override the retry/backoff policy (defaults to [`RetryPolicy::from_env`]).
    /// Used by tests to inject a zero-delay policy; production callers can tune
    /// it via the `NEWT_HTTP_*` env vars instead.
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// Return the configured endpoint URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Attach a bearer token, sent as `Authorization: Bearer <token>` on
    /// every request. A `None` argument (or an empty token) leaves the
    /// backend unauthenticated, so callers can pass a resolved
    /// `Option<String>` straight through.
    pub fn with_api_key(mut self, api_key: impl Into<Option<String>>) -> Self {
        self.api_key = api_key.into().filter(|k| !k.is_empty());
        self
    }

    /// Build from a [`BackendConfig`](newt_core::BackendConfig), wiring up
    /// the endpoint, model, and bearer auth resolved from the config's
    /// `api_key_env` / `api_key_file`. Used by the worker to construct an
    /// authenticated OpenAI-compatible backend from `~/.newt/config.toml`.
    pub fn from_config(cfg: &newt_core::BackendConfig) -> Self {
        Self::new(cfg.endpoint.clone(), cfg.model.clone()).with_api_key(cfg.resolve_api_key())
    }

    /// Apply bearer auth to a request builder when a token is configured.
    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => rb.bearer_auth(key),
            None => rb,
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

    /// Single HTTP attempt — no retries. Returns a structured error that
    /// [`crate::retry::classify`] can classify for the backoff loop.
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
            .authed(self.client.post(&url).json(&body))
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
        // #385: strip inline <think>…</think> reasoning from the content.
        let (content, _reasoning) = newt_core::split_reasoning(
            json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or(""),
        );
        // Prefer the model echoed back by the server (helps callers
        // distinguish aliases) but fall back to the configured id.
        let model_id = json["model"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.model.clone());

        // Extract exact token counts from the OpenAI-compatible `usage`
        // object when present. Preferring the server's reported counts
        // over heuristic estimation directly addresses #247 (token
        // estimator error) on the vLLM path.
        let usage = {
            let input = json["usage"]["prompt_tokens"].as_u64().map(|n| n as u32);
            let output = json["usage"]["completion_tokens"]
                .as_u64()
                .map(|n| n as u32);
            input.zip(output).map(|(i, o)| newt_core::TokenUsage {
                input_tokens: i,
                output_tokens: o,
            })
        };

        Ok(ChatReply {
            content,
            model_id,
            usage,
        })
    }

    /// List the models the vLLM server is currently serving.
    ///
    /// Mirrors the OpenAI `GET /v1/models` response shape:
    ///
    /// ```json
    /// { "data": [{ "id": "llama3.1:8b", "object": "model" }, ...] }
    /// ```
    ///
    /// Used by `newt doctor` (follow-up) to probe vLLM endpoints.
    pub async fn list_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        let url = format!("{}/v1/models", self.endpoint.trim_end_matches('/'));
        let resp = self
            .authed(self.client.get(&url))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("vLLM list_models request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("vLLM list_models returned {status}: {text}");
        }

        let json: serde_json::Value = resp.json().await?;
        let data = json["data"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("vLLM list_models response missing 'data' array"))?;

        let models = data
            .iter()
            .filter_map(|entry| {
                entry["id"]
                    .as_str()
                    .map(|id| ModelInfo { id: id.to_string() })
            })
            .collect();

        Ok(models)
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

    fn endpoint(&self) -> Option<&str> {
        Some(&self.endpoint)
    }

    async fn complete(&self, req: ChatRequest) -> anyhow::Result<ChatReply> {
        with_backoff(&self.retry, || self.try_complete(&req)).await
    }
}
