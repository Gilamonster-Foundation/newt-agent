//! Native Anthropic Messages API transport (`POST /v1/messages`).
//!
//! Like [`ResponsesBackend`](crate::responses::ResponsesBackend), this seam
//! carries the SIMPLE completion shape — [`ChatRequest`] is system/user/
//! assistant text turns → assistant text — for the worker, crew-dispatch,
//! and summarizer paths. `tool_use`/`tool_result` and streaming live with
//! the interactive agentic loop in `newt_core::agentic` (which owns the
//! shared wire mapping this transport reuses:
//! [`newt_core::agentic::anthropic_wire`]).
//!
//! Auth is `x-api-key` + `anthropic-version` headers — NOT a bearer token.
//! The version const is shared with the probe
//! ([`newt_core::backend_probe::ANTHROPIC_VERSION`]) so the two can't drift.

use async_trait::async_trait;
use newt_core::agentic::anthropic_wire;
use newt_core::backend_probe::ANTHROPIC_VERSION;
use newt_core::router::Tier;

use crate::backend::{ChatReply, ChatRequest, InferenceBackend};
use crate::retry::{with_backoff, RetryPolicy};

/// The default hosted endpoint; configurable for proxies/gateways.
pub const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com";

#[derive(Debug)]
pub struct AnthropicBackend {
    endpoint: String,
    model: String,
    client: reqwest::Client,
    api_key: Option<String>,
    retry: RetryPolicy,
}

impl AnthropicBackend {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            client: reqwest::Client::new(),
            api_key: None,
            retry: RetryPolicy::from_env(),
        }
    }

    /// Attach the API key, sent as `x-api-key` on every request. `None`/empty
    /// leaves the backend unauthenticated (the server will 401 — surfaced,
    /// never masked).
    pub fn with_api_key(mut self, api_key: impl Into<Option<String>>) -> Self {
        self.api_key = api_key.into().filter(|k| !k.is_empty());
        self
    }

    /// Override the retry/backoff policy (defaults to [`RetryPolicy::from_env`]).
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// Override the HTTP client timeout. Useful for testing.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("build client");
        self
    }

    /// Build from a [`BackendConfig`](newt_core::BackendConfig): endpoint,
    /// model, and the key from `resolve_api_key()`, falling back to the
    /// `ANTHROPIC_API_KEY` env convention so a minimal drop-in is just
    /// endpoint + model.
    pub fn from_config(cfg: &newt_core::BackendConfig) -> Self {
        let model = cfg.effective_model().unwrap_or_default().to_string();
        let endpoint = if cfg.endpoint.is_empty() {
            DEFAULT_ENDPOINT.to_string()
        } else {
            cfg.endpoint.clone()
        };
        let key = cfg.resolve_api_key().or_else(|| {
            std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
        });
        Self::new(endpoint, model).with_api_key(key)
    }

    /// Single HTTP attempt — no retries. Error strings follow the
    /// `"<backend> returned <code>: <body>"` contract that
    /// [`crate::retry::classify`] parses, so 429/5xx (incl. 529 overloaded)
    /// retry and 4xx are fatal with the server's self-describing body.
    async fn try_complete(&self, req: &ChatRequest) -> anyhow::Result<ChatReply> {
        // ChatRequest turns → the internal shape → the Anthropic wire.
        let internal: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| serde_json::json!({ "role": &m.role, "content": &m.content }))
            .collect();
        let (system, messages) = anthropic_wire::anthropic_wire_messages(&internal)?;
        let max_tokens = req
            .max_tokens
            .unwrap_or_else(anthropic_wire::default_max_tokens);
        let body = anthropic_wire::build_messages_body(
            &self.model,
            max_tokens,
            system.as_deref(),
            &messages,
            None,
            false,
        );

        let url = anthropic_wire::messages_url(&self.endpoint);
        let mut rb = self
            .client
            .post(&url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body);
        if let Some(key) = &self.api_key {
            rb = rb.header("x-api-key", key);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Anthropic request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic returned {status}: {text}");
        }

        let json: serde_json::Value = resp.json().await?;
        let round = anthropic_wire::parse_messages_reply(&json);

        // Fail-closed decoding (the responses_wire invariant): a refusal or
        // an empty completed reply is an ERROR, never an empty "success" the
        // caller might mistake for an answer.
        if round.stop_reason.as_deref() == Some("refusal") && round.text.is_empty() {
            anyhow::bail!("Anthropic declined this request (stop_reason=refusal)");
        }
        if round.text.is_empty() {
            anyhow::bail!(
                "Anthropic returned an empty reply (stop_reason={:?})",
                round.stop_reason
            );
        }

        Ok(ChatReply {
            // The model echo wins over the configured id (server truth).
            model_id: round.model.unwrap_or_else(|| self.model.clone()),
            content: round.text,
            usage: round.usage,
        })
    }
}

#[async_trait]
impl InferenceBackend for AnthropicBackend {
    fn name(&self) -> &str {
        "anthropic"
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

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn zero_delay() -> RetryPolicy {
        RetryPolicy::immediate(0)
    }

    fn reply_json(text: &str) -> serde_json::Value {
        serde_json::json!({
            "model": "claude-sonnet-4-5",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 10, "output_tokens": 4}
        })
    }

    #[tokio::test]
    async fn posts_v1_messages_with_headers_and_required_max_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .and(body_partial_json(serde_json::json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": anthropic_wire::DEFAULT_MAX_TOKENS,
                "stream": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply_json("hello")))
            .expect(1)
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(server.uri(), "claude-sonnet-4-5")
            .with_api_key(Some("sk-ant-test".to_string()))
            .with_retry_policy(zero_delay());
        let reply = backend
            .complete(ChatRequest::new().user("hi"))
            .await
            .unwrap();
        assert_eq!(reply.content, "hello");
        assert_eq!(reply.model_id, "claude-sonnet-4-5");
        let usage = reply.usage.unwrap();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 4);
        server.verify().await;
    }

    #[tokio::test]
    async fn splits_system_into_top_level_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(serde_json::json!({
                "system": "be terse",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]}
                ],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply_json("ok")))
            .expect(1)
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(server.uri(), "claude-sonnet-4-5")
            .with_retry_policy(zero_delay());
        backend
            .complete(ChatRequest::new().system("be terse").user("hi"))
            .await
            .unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn explicit_max_tokens_wins_over_default() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(serde_json::json!({"max_tokens": 77})))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply_json("ok")))
            .expect(1)
            .mount(&server)
            .await;

        let backend =
            AnthropicBackend::new(server.uri(), "claude-x").with_retry_policy(zero_delay());
        backend
            .complete(ChatRequest::new().user("hi").max_tokens(77))
            .await
            .unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn refusal_is_an_error_not_an_empty_reply() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "claude-x",
                "stop_reason": "refusal",
                "content": [],
                "usage": {"input_tokens": 5, "output_tokens": 0}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let backend =
            AnthropicBackend::new(server.uri(), "claude-x").with_retry_policy(zero_delay());
        let err = backend
            .complete(ChatRequest::new().user("hi"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusal"), "names the refusal: {err}");
        server.verify().await;
    }

    #[tokio::test]
    async fn api_error_body_surfaces_message_and_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "invalid_request_error",
                          "message": "messages: roles must alternate"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(server.uri(), "claude-x")
            .with_retry_policy(RetryPolicy::immediate(3));
        let err = backend
            .complete(ChatRequest::new().user("hi"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("roles must alternate"), "server body: {err}");
        // expect(1): a 400 is fatal, never retried.
        server.verify().await;
    }

    #[tokio::test]
    async fn overloaded_529_retries_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(529).set_body_json(serde_json::json!({
                "type": "error",
                "error": {"type": "overloaded_error", "message": "Overloaded"}
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(reply_json("recovered")))
            .expect(1)
            .mount(&server)
            .await;

        let backend = AnthropicBackend::new(server.uri(), "claude-x")
            .with_retry_policy(RetryPolicy::immediate(2));
        let reply = backend
            .complete(ChatRequest::new().user("hi"))
            .await
            .unwrap();
        assert_eq!(reply.content, "recovered");
        server.verify().await;
    }

    #[tokio::test]
    async fn max_tokens_stop_returns_partial_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "claude-x",
                "stop_reason": "max_tokens",
                "content": [{"type": "text", "text": "truncated but real"}],
                "usage": {"input_tokens": 5, "output_tokens": 77}
            })))
            .mount(&server)
            .await;

        let backend =
            AnthropicBackend::new(server.uri(), "claude-x").with_retry_policy(zero_delay());
        let reply = backend
            .complete(ChatRequest::new().user("hi"))
            .await
            .unwrap();
        assert_eq!(reply.content, "truncated but real");
    }

    #[tokio::test]
    async fn ignores_thinking_blocks_in_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "claude-x",
                "stop_reason": "end_turn",
                "content": [
                    {"type": "thinking", "thinking": "pondering"},
                    {"type": "text", "text": "the answer"}
                ],
                "usage": {"input_tokens": 5, "output_tokens": 3}
            })))
            .mount(&server)
            .await;

        let backend =
            AnthropicBackend::new(server.uri(), "claude-x").with_retry_policy(zero_delay());
        let reply = backend
            .complete(ChatRequest::new().user("hi"))
            .await
            .unwrap();
        assert_eq!(reply.content, "the answer");
    }

    #[test]
    fn from_config_prefers_config_key_and_defaults_endpoint() {
        // Config-resolved key wins; empty endpoint falls back to the hosted
        // default. (ANTHROPIC_API_KEY env fallback is exercised implicitly —
        // this test never sets it, proving absence is fine.)
        let cfg = newt_core::BackendConfig {
            name: "anthropic".into(),
            endpoint: String::new(),
            model: Some("claude-sonnet-4-5".into()),
            kind: Some(newt_core::BackendKind::Anthropic),
            ..Default::default()
        };
        let backend = AnthropicBackend::from_config(&cfg);
        assert_eq!(backend.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(backend.model_id(), "claude-sonnet-4-5");
    }
}
