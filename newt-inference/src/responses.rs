//! Responses-API transport for OpenAI-compatible backends.
//!
//! A backend configured with `api = "responses"` (e.g. `gpt-5.6-sol`) speaks the
//! newer Responses API (`POST /v1/responses`), NOT Chat Completions. The
//! interactive `newt solve` path already honours this through the agentic loop,
//! but the ACP worker consumes the [`InferenceBackend`] seam directly — so
//! without an api-aware transport here, `newt worker` drove a Responses-only
//! model over `/v1/chat/completions` and 400d on function tools.
//!
//! [`openai_inference_backend`] is the single factory that both surfaces should
//! use to pick the transport from `BackendConfig.api`.
//!
//! Scope: this seam carries the SIMPLE completion shape ([`ChatRequest`] =
//! system/user messages → assistant text). Multi-turn function-calling lives in
//! the agentic loop, not in this backend.

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};
use crate::retry::{with_backoff, RetryPolicy};
use async_trait::async_trait;
use newt_core::router::Tier;
use std::sync::Arc;

/// OpenAI-compatible backend speaking the Responses API (`POST /v1/responses`).
pub struct ResponsesBackend {
    endpoint: String,
    model: String,
    client: reqwest::Client,
    api_key: Option<String>,
    retry: RetryPolicy,
}

impl ResponsesBackend {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            client: reqwest::Client::new(),
            api_key: None,
            retry: RetryPolicy::from_env(),
        }
    }

    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key.filter(|k| !k.is_empty());
        self
    }

    /// Build from a [`BackendConfig`](newt_core::BackendConfig) — mirrors
    /// [`crate::local::LocalVllmBackend::from_config`] so the two transports are
    /// constructed identically apart from the wire API.
    pub fn from_config(cfg: &newt_core::BackendConfig) -> Self {
        let model = cfg.effective_model().unwrap_or_default().to_string();
        Self::new(cfg.endpoint.clone(), model).with_api_key(cfg.resolve_api_key())
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => rb.bearer_auth(key),
            None => rb,
        }
    }

    async fn try_complete(&self, req: &ChatRequest) -> anyhow::Result<ChatReply> {
        // Responses shape: the system prompt(s) become `instructions`; the rest
        // of the turn is `input` as role/content items.
        let mut instructions = String::new();
        let mut input = Vec::new();
        for m in &req.messages {
            if m.role == "system" {
                if !instructions.is_empty() {
                    instructions.push('\n');
                }
                instructions.push_str(&m.content);
            } else {
                input.push(serde_json::json!({ "role": &m.role, "content": &m.content }));
            }
        }
        let mut body = serde_json::json!({
            "model": self.model,
            "input": input,
            "stream": false,
        });
        if !instructions.is_empty() {
            body["instructions"] = serde_json::json!(instructions);
        }
        if let Some(max) = req.max_tokens {
            body["max_output_tokens"] = serde_json::json!(max);
        }

        let url = format!("{}/v1/responses", self.endpoint.trim_end_matches('/'));
        let resp = self
            .authed(self.client.post(&url).json(&body))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Responses request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Responses API returned {status}: {text}");
        }

        let json: serde_json::Value = resp.json().await?;
        let content = extract_output_text(&json);
        let model_id = json["model"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.model.clone());
        // Responses usage is `input_tokens`/`output_tokens`; accept the Chat
        // Completions names too for lenient servers.
        let usage = {
            let input = json["usage"]["input_tokens"]
                .as_u64()
                .or_else(|| json["usage"]["prompt_tokens"].as_u64())
                .map(|n| n as u32);
            let output = json["usage"]["output_tokens"]
                .as_u64()
                .or_else(|| json["usage"]["completion_tokens"].as_u64())
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
}

/// Concatenate the `output_text` parts of a Responses reply's `output[]`,
/// falling back to a flat top-level `output_text` string. Mirrors the agentic
/// loop's extraction (`newt-core::agentic`).
fn extract_output_text(json: &serde_json::Value) -> String {
    if let Some(items) = json["output"].as_array() {
        let mut out = String::new();
        for item in items {
            if let Some(parts) = item["content"].as_array() {
                for part in parts {
                    if part["type"] == "output_text" {
                        if let Some(t) = part["text"].as_str() {
                            out.push_str(t);
                        }
                    }
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    json["output_text"].as_str().unwrap_or("").to_string()
}

#[async_trait]
impl InferenceBackend for ResponsesBackend {
    fn name(&self) -> &str {
        "openai-responses"
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

/// Select the [`InferenceBackend`] transport for an OpenAI-compatible backend by
/// its declared wire `api`. This is the single seam that makes every
/// `InferenceBackend` consumer (notably the ACP worker) api-aware:
/// `api = "responses"` posts to `/v1/responses`; anything else uses Chat
/// Completions. The exhaustive match means a new `OpenAiApi` variant must be
/// handled here, not silently defaulted to Chat Completions.
pub fn openai_inference_backend(cfg: &newt_core::BackendConfig) -> Arc<dyn InferenceBackend> {
    use newt_core::OpenAiApi;
    match cfg.api.unwrap_or_default() {
        OpenAiApi::Responses => Arc::new(ResponsesBackend::from_config(cfg)),
        OpenAiApi::ChatCompletions => Arc::new(crate::local::LocalVllmBackend::from_config(cfg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::{BackendConfig, BackendKind, OpenAiApi};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn backend_cfg(endpoint: &str, api: OpenAiApi) -> BackendConfig {
        BackendConfig {
            name: "sol".into(),
            kind: Some(BackendKind::Openai),
            api: Some(api),
            endpoint: endpoint.into(),
            model: Some("gpt-5.6-sol".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn responses_backend_posts_to_v1_responses_and_extracts_output_text() {
        let server = MockServer::start().await;
        // The ONLY endpoint that may be hit is /v1/responses. If the backend
        // touched /v1/chat/completions there is no mock for it → 404 → error.
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "gpt-5.6-sol",
                "output": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "the answer"}]
                }],
                "usage": {"input_tokens": 11, "output_tokens": 3}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let backend =
            ResponsesBackend::from_config(&backend_cfg(&server.uri(), OpenAiApi::Responses));
        let reply = backend
            .complete(ChatRequest::new().system("sys").user("hi"))
            .await
            .expect("responses completion");
        assert_eq!(reply.content, "the answer");
        assert_eq!(reply.usage.unwrap().input_tokens, 11);
        // Mock's `.expect(1)` on /v1/responses is verified on server drop.
    }

    #[tokio::test]
    async fn responses_backend_never_calls_chat_completions() {
        let server = MockServer::start().await;
        // Only /v1/responses is mocked; assert /v1/chat/completions is NEVER hit.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output_text": "ok"
            })))
            .mount(&server)
            .await;

        let backend =
            ResponsesBackend::from_config(&backend_cfg(&server.uri(), OpenAiApi::Responses));
        let reply = backend
            .complete(ChatRequest::new().user("hi"))
            .await
            .unwrap();
        assert_eq!(reply.content, "ok");
    }

    #[test]
    fn factory_selects_transport_by_api() {
        // The seam the ACP worker uses: api=responses → Responses transport;
        // chat_completions (and the default) → Chat Completions transport.
        let responses = openai_inference_backend(&backend_cfg("http://x", OpenAiApi::Responses));
        assert_eq!(responses.name(), "openai-responses");
        let chat = openai_inference_backend(&backend_cfg("http://x", OpenAiApi::ChatCompletions));
        assert_eq!(chat.name(), "vllm-local");
        // A config with no explicit api defaults to Chat Completions (unchanged).
        let mut no_api = backend_cfg("http://x", OpenAiApi::ChatCompletions);
        no_api.api = None;
        assert_eq!(openai_inference_backend(&no_api).name(), "vllm-local");
    }
}
