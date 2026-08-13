//! **Endpoint probe + served-model adoption** — the shared core of
//! server-authoritative backends (#1136, epic #1126 Phase B).
//!
//! One module that the TUI session start, `newt setup`, and `newt doctor` all
//! call: fetch what an endpoint actually serves (`/api/tags` for Ollama,
//! `/v1/models` for OpenAI-compatible), then make the pure [`adopt`] decision —
//! which model this session uses and which [`Serving`] shape the backend has.
//!
//! The laws (docs/design + #1126):
//! - **The server dictates.** An *instance* backend (vLLM: `/v1/models` merely
//!   declares its one model) has its served model adopted **unconditionally** —
//!   a requested/configured model that disagrees is ignored and flagged so the
//!   caller can say so honestly.
//! - **A multiplexer negotiates.** Ollama-style backends honor the requested
//!   model (session override), else the declared config model, else the first
//!   served model.
//! - **Offline never silently fails over.** `adopt` is only called with a real
//!   probe result; an unreachable endpoint is the caller's fallback path (file
//!   hint + banner), not this module's.

use crate::config::{
    BackendConfig, BackendKind, Engine, ManagedMode, OpenAiApi as OpenAiApiSurface, Serving,
};

/// HTTP status returned by a model-list probe. Keeping the status typed lets
/// endpoint detection distinguish authentication and unsupported APIs from a
/// host that could not be reached, while preserving the existing `HTTP ...`
/// display consumed by `newt doctor`.
#[derive(Debug)]
struct ProbeHttpStatus(reqwest::StatusCode);

impl std::fmt::Display for ProbeHttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 13:18 EDT | Date: 2026-08-12

impl std::error::Error for ProbeHttpStatus {}

#[derive(Debug)]
struct ProbeResponseShape(&'static str);

impl std::fmt::Display for ProbeResponseShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid model-list response: missing `{}` array", self.0)
    }
}

impl std::error::Error for ProbeResponseShape {}

/// One backend API's wire behavior — how it lists models, reads its context
/// window, and derives its serving axis. Each [`BackendKind`] has one impl, so
/// callers ASK THE BACKEND (`api_for(kind).list_models(…)`) instead of matching
/// on `kind` at a dozen sites. Adding a backend = one impl, not N new branches.
#[async_trait::async_trait]
pub trait BackendApi: Send + Sync {
    /// The models this endpoint serves. `Vec` order is server order.
    async fn list_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> anyhow::Result<Vec<String>>;

    /// The declared context window (tokens) for `model`, if the API exposes
    /// one. `None` when the API can't be asked (the caller keeps its default).
    async fn context_window(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
    ) -> Option<u32>;

    /// Derive the serving axis from how many models the endpoint reported.
    fn serving(&self, served_count: usize) -> Serving;

    /// The models currently WARM (loaded in memory) at this endpoint, in
    /// server order. `None` = this backend/engine cannot report warmth
    /// (capability absent or unreachable) — callers fall back to served
    /// order. `Some(vec![])` = authoritative "nothing loaded". Fail-soft
    /// like [`BackendApi::context_window`].
    async fn warm_models(
        &self,
        _client: &reqwest::Client,
        _endpoint: &str,
        _api_key: Option<&str>,
    ) -> Option<Vec<String>> {
        None
    }
}

/// The `BackendApi` for a wire kind — a `&'static` ZST, so no allocation.
pub fn api_for(kind: BackendKind) -> &'static dyn BackendApi {
    match kind {
        BackendKind::Ollama => &OllamaApi,
        BackendKind::Openai => &OpenAiApi,
        BackendKind::Embedded => &EmbeddedApi,
        BackendKind::Anthropic => &AnthropicApi,
    }
}

/// The engine-refined `BackendApi`: same wire behavior as [`api_for`], plus
/// the engine's warmth capability where one exists (llama.cpp's `/models`
/// load states, vLLM's single resident model). Unknown/undetected engine ==
/// `api_for(kind)`.
pub fn api_for_engine(kind: BackendKind, engine: Option<Engine>) -> &'static dyn BackendApi {
    match (kind, engine) {
        (BackendKind::Openai, Some(Engine::LlamaCpp)) => &LlamaCppApi,
        (BackendKind::Openai, Some(Engine::Vllm)) => &VllmApi,
        _ => api_for(kind),
    }
}

/// The `anthropic-version` header value both the probe and the `/v1/messages`
/// transport send. ONE const so the two can never drift.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_GENERATION_PROBE_BODY_BYTES: usize = 64 * 1024;

async fn read_generation_probe_body(
    mut response: reqwest::Response,
) -> Result<(reqwest::StatusCode, Vec<u8>), GenerationFailure> {
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_GENERATION_PROBE_BODY_BYTES as u64)
    {
        return Err(GenerationFailure::ResponseTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| GenerationFailure::Transport)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_GENERATION_PROBE_BODY_BYTES {
            return Err(GenerationFailure::ResponseTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, body))
}

fn auth_rejection(status: reqwest::StatusCode) -> Option<GenerationCheck> {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    )
    .then(|| GenerationCheck::Rejected(status.as_u16()))
}

/// Result of a minimal real generation request used to gate setup. Model
/// catalogs are discovery only: they can be public even when generation is
/// not authorized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationCheck {
    Accepted(Option<OpenAiApiSurface>),
    Rejected(u16),
    Unverified(GenerationFailure),
}

/// Terminal-safe reason why a generation probe could not verify an endpoint.
///
/// Provider response bodies and transport error strings never cross this type
/// boundary, so an echoed bearer token, refusal, or control sequence cannot be
/// rendered by setup diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationFailure {
    Transport,
    ResponseTooLarge,
    HttpStatus(u16),
    InvalidJson,
    InvalidEnvelope,
    InvalidResponsesPayload,
    UnsupportedBackend,
}

impl std::fmt::Display for GenerationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport => formatter.write_str("the generation request or response failed"),
            Self::ResponseTooLarge => formatter.write_str("the generation response was too large"),
            Self::HttpStatus(status) => {
                write!(
                    formatter,
                    "HTTP {status} did not accept the generation request"
                )
            }
            Self::InvalidJson => formatter.write_str("the generation response was not valid JSON"),
            Self::InvalidEnvelope => {
                formatter.write_str("the generation response had no valid output envelope")
            }
            Self::InvalidResponsesPayload => {
                formatter.write_str("the Responses generation payload was unusable")
            }
            Self::UnsupportedBackend => {
                formatter.write_str("this backend does not use an HTTP generation probe")
            }
        }
    }
}

#[derive(Clone, Copy)]
enum RequiredEnvelope {
    OllamaMessage,
    OpenAiChoices,
    AnthropicContent,
}

fn classify_generation_response(
    status: reqwest::StatusCode,
    body: &[u8],
    required: RequiredEnvelope,
    api: Option<OpenAiApiSurface>,
) -> GenerationCheck {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return GenerationCheck::Rejected(status.as_u16());
    }
    if !status.is_success() {
        return GenerationCheck::Unverified(GenerationFailure::HttpStatus(status.as_u16()));
    }
    let parsed: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => {
            return GenerationCheck::Unverified(GenerationFailure::InvalidJson);
        }
    };
    let valid = match required {
        RequiredEnvelope::OllamaMessage => {
            parsed["message"].is_object()
                && (parsed["message"]["content"].is_string()
                    || parsed["message"]["tool_calls"].is_array())
        }
        RequiredEnvelope::OpenAiChoices => parsed["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .is_some_and(|choice| {
                let message = &choice["message"];
                message["content"].is_string()
                    || message["reasoning"].is_string()
                    || message["reasoning_content"].is_string()
                    || message["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty())
            }),
        RequiredEnvelope::AnthropicContent => parsed["content"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
    };
    if valid {
        GenerationCheck::Accepted(api)
    } else {
        GenerationCheck::Unverified(GenerationFailure::InvalidEnvelope)
    }
}

fn openai_chat_probe_body(model: &str, modern_budget: bool) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "Reply with OK."}],
        "stream": false,
    });
    let budget = if modern_budget {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    body[budget] = serde_json::json!(8);
    body
}

fn rejects_legacy_max_tokens(status: reqwest::StatusCode, body: &[u8]) -> bool {
    if !matches!(status.as_u16(), 400 | 422) {
        return false;
    }
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    text.contains("max_tokens")
        && [
            "unsupported",
            "not supported",
            "unknown",
            "unrecognized",
            "unexpected",
            "deprecated",
        ]
        .iter()
        .any(|needle| text.contains(needle))
}

async fn send_openai_chat_probe(
    client: &reqwest::Client,
    base: &str,
    model: &str,
    api_key: Option<&str>,
    modern_budget: bool,
) -> Result<(reqwest::StatusCode, Vec<u8>), GenerationCheck> {
    let mut request = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&openai_chat_probe_body(model, modern_budget));
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|_| GenerationCheck::Unverified(GenerationFailure::Transport))?;
    if let Some(rejected) = auth_rejection(response.status()) {
        return Err(rejected);
    }
    read_generation_probe_body(response)
        .await
        .map_err(GenerationCheck::Unverified)
}

fn classify_responses_generation(status: reqwest::StatusCode, body: &[u8]) -> GenerationCheck {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return GenerationCheck::Rejected(status.as_u16());
    }
    if !status.is_success() {
        return GenerationCheck::Unverified(GenerationFailure::HttpStatus(status.as_u16()));
    }
    let parsed: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => {
            return GenerationCheck::Unverified(GenerationFailure::InvalidJson);
        }
    };
    // A deliberately tiny probe budget can end a real Responses generation as
    // `incomplete/max_output_tokens`. Treat that as authentication evidence only
    // when the partial body would otherwise be a usable, non-refusal response.
    // Re-running the ONE fail-closed decoder with a synthetic terminal status
    // preserves its top-level-error/refusal/recognized-output invariants instead
    // of accepting any arbitrary non-empty `output` array.
    let mut decodable = parsed.clone();
    if parsed["status"] == "incomplete"
        && parsed["incomplete_details"]["reason"] == "max_output_tokens"
    {
        decodable["status"] = serde_json::Value::String("completed".into());
    }
    match crate::responses_wire::decode_response(&decodable) {
        Ok(_) => GenerationCheck::Accepted(Some(OpenAiApiSurface::Responses)),
        Err(_) => GenerationCheck::Unverified(GenerationFailure::InvalidResponsesPayload),
    }
}

async fn send_responses_generation_probe(
    request: reqwest::RequestBuilder,
    api_key: Option<&str>,
) -> GenerationCheck {
    let request = match api_key.filter(|key| !key.trim().is_empty()) {
        Some(key) => request.bearer_auth(key),
        None => request,
    };
    match request.send().await {
        Ok(response) => {
            if let Some(rejected) = auth_rejection(response.status()) {
                return rejected;
            }
            match read_generation_probe_body(response).await {
                Ok((status, body)) => classify_responses_generation(status, &body),
                Err(error) => GenerationCheck::Unverified(error),
            }
        }
        Err(_) => GenerationCheck::Unverified(GenerationFailure::Transport),
    }
}

async fn send_generation_probe(
    request: reqwest::RequestBuilder,
    api_key: Option<&str>,
    required: RequiredEnvelope,
    api: Option<OpenAiApiSurface>,
) -> GenerationCheck {
    let request = match api_key.filter(|key| !key.trim().is_empty()) {
        Some(key) => request.bearer_auth(key),
        None => request,
    };
    match request.send().await {
        Ok(response) => {
            if let Some(rejected) = auth_rejection(response.status()) {
                return rejected;
            }
            match read_generation_probe_body(response).await {
                Ok((status, body)) => classify_generation_response(status, &body, required, api),
                Err(error) => GenerationCheck::Unverified(error),
            }
        }
        Err(_) => GenerationCheck::Unverified(GenerationFailure::Transport),
    }
}

/// Verify a selected endpoint/model with a minimal real generation request.
/// `api = None` auto-negotiates an OpenAI-compatible endpoint by trying Chat
/// Completions first and Responses only when Chat is absent or explicitly
/// redirects to Responses.
pub async fn verify_generation(
    client: &reqwest::Client,
    kind: BackendKind,
    api: Option<OpenAiApiSurface>,
    endpoint: &str,
    model: &str,
    api_key: Option<&str>,
) -> GenerationCheck {
    let base = endpoint.trim_end_matches('/');
    match kind {
        BackendKind::Ollama => {
            let body = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": false,
                "options": {"num_predict": 1},
            });
            send_generation_probe(
                client.post(format!("{base}/api/chat")).json(&body),
                api_key,
                RequiredEnvelope::OllamaMessage,
                None,
            )
            .await
        }
        BackendKind::Openai => {
            if api != Some(OpenAiApiSurface::Responses) {
                let (mut status, mut body) =
                    match send_openai_chat_probe(client, base, model, api_key, false).await {
                        Ok(result) => result,
                        Err(result) => return result,
                    };
                if rejects_legacy_max_tokens(status, &body) {
                    (status, body) =
                        match send_openai_chat_probe(client, base, model, api_key, true).await {
                            Ok(result) => result,
                            Err(result) => return result,
                        };
                }
                if status.is_success() || api == Some(OpenAiApiSurface::ChatCompletions) {
                    return classify_generation_response(
                        status,
                        &body,
                        RequiredEnvelope::OpenAiChoices,
                        Some(OpenAiApiSurface::ChatCompletions),
                    );
                }
                let text = String::from_utf8_lossy(&body);
                if status != reqwest::StatusCode::NOT_FOUND && !is_responses_only_error(&text) {
                    return classify_generation_response(
                        status,
                        &body,
                        RequiredEnvelope::OpenAiChoices,
                        Some(OpenAiApiSurface::ChatCompletions),
                    );
                }
            }
            let body = crate::responses_wire::generation_probe_body(model);
            send_responses_generation_probe(
                client.post(format!("{base}/v1/responses")).json(&body),
                api_key,
            )
            .await
        }
        BackendKind::Anthropic => {
            let body = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello"}],
                "max_tokens": 1,
            });
            let request = client
                .post(format!("{base}/v1/messages"))
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body);
            let request = match api_key.filter(|key| !key.trim().is_empty()) {
                Some(key) => request.header("x-api-key", key),
                None => request,
            };
            match request.send().await {
                Ok(response) => {
                    if let Some(rejected) = auth_rejection(response.status()) {
                        return rejected;
                    }
                    match read_generation_probe_body(response).await {
                        Ok((status, body)) => classify_generation_response(
                            status,
                            &body,
                            RequiredEnvelope::AnthropicContent,
                            None,
                        ),
                        Err(error) => GenerationCheck::Unverified(error),
                    }
                }
                Err(_) => GenerationCheck::Unverified(GenerationFailure::Transport),
            }
        }
        BackendKind::Embedded => GenerationCheck::Unverified(GenerationFailure::UnsupportedBackend),
    }
}

/// Ollama: `/api/tags` to list, `/api/show` for the window, always a
/// time-multiplexer (many models loaded on demand). Bearer auth is sent when
/// a key is supplied (Ollama Cloud, `https://ollama.com`); LAN Ollama ignores
/// an unexpected Authorization header.
pub struct OllamaApi;
/// OpenAI-compatible (vLLM / gateways): `/v1/models` to list + read
/// `max_model_len`, bearer auth, serving derived from the served count.
pub struct OpenAiApi;
/// The in-process GGUF engine: no HTTP; runs exactly one model.
pub struct EmbeddedApi;
/// Anthropic's Messages API: `GET /v1/models` with `x-api-key` +
/// `anthropic-version` headers (never a bearer), paginated via `after_id`;
/// always a multiplexer (the hosted API fronts the whole Claude family).
pub struct AnthropicApi;
/// llama.cpp's `llama-server` behind the OpenAI wire: delegates the wire
/// behavior to [`OpenAiApi`]; adds warmth from the non-`/v1` `/models`
/// route, whose entries carry load states.
pub struct LlamaCppApi;
/// vLLM behind the OpenAI wire: delegates the wire behavior to
/// [`OpenAiApi`]; its served model IS the resident model, so warmth is the
/// served list itself.
pub struct VllmApi;

/// Attach `Authorization: Bearer <key>` when a non-empty key is supplied.
/// Ollama Cloud (`https://ollama.com`) requires it on the native API; LAN
/// Ollama ignores an unexpected auth header, and the key is only ever sent
/// when the operator configured one.
fn maybe_bearer(req: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key.filter(|k| !k.trim().is_empty()) {
        Some(key) => req.bearer_auth(key),
        None => req,
    }
}

#[async_trait::async_trait]
impl BackendApi for OllamaApi {
    async fn list_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
        let resp = maybe_bearer(client.get(&url), api_key).send().await?;
        if !resp.status().is_success() {
            return Err(ProbeHttpStatus(resp.status()).into());
        }
        let json: serde_json::Value = resp.json().await?;
        let models = json["models"]
            .as_array()
            .ok_or(ProbeResponseShape("models"))?;
        Ok(models
            .iter()
            .filter_map(|m| m["name"].as_str().map(str::to_string))
            .collect())
    }

    async fn context_window(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
    ) -> Option<u32> {
        let url = format!("{}/api/show", endpoint.trim_end_matches('/'));
        let resp = maybe_bearer(
            client
                .post(&url)
                .json(&serde_json::json!({ "name": model })),
            api_key,
        )
        .send()
        .await
        .ok()?;
        let json: serde_json::Value = resp.json().await.ok()?;
        parse_ollama_show_window(&json)
    }

    fn serving(&self, _served_count: usize) -> Serving {
        // Ollama loads models on demand — always a multiplexer, even if only
        // one model happens to be pulled today.
        Serving::Multiplexer
    }

    async fn warm_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> Option<Vec<String>> {
        let ps = fetch_ollama_ps(client, endpoint, api_key).await.ok()?;
        Some(ps.into_iter().map(|m| m.name).collect())
    }
}

#[async_trait::async_trait]
impl BackendApi for LlamaCppApi {
    async fn list_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        OpenAiApi.list_models(client, endpoint, api_key).await
    }

    async fn context_window(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
    ) -> Option<u32> {
        OpenAiApi
            .context_window(client, endpoint, model, api_key)
            .await
    }

    fn serving(&self, served_count: usize) -> Serving {
        OpenAiApi.serving(served_count)
    }

    async fn warm_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> Option<Vec<String>> {
        // llama-server's non-/v1 `/models` route carries per-entry load
        // state (vLLM 404s here, so this is also engine-distinctive). No
        // state fields at all → capability absent (`None`) — never guess.
        let url = format!("{}/models", endpoint.trim_end_matches('/'));
        let resp = maybe_bearer(client.get(&url), api_key).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: serde_json::Value = resp.json().await.ok()?;
        parse_llamacpp_models_warm(&json)
    }
}

#[async_trait::async_trait]
impl BackendApi for VllmApi {
    async fn list_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        OpenAiApi.list_models(client, endpoint, api_key).await
    }

    async fn context_window(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
    ) -> Option<u32> {
        OpenAiApi
            .context_window(client, endpoint, model, api_key)
            .await
    }

    fn serving(&self, served_count: usize) -> Serving {
        OpenAiApi.serving(served_count)
    }

    async fn warm_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> Option<Vec<String>> {
        // A vLLM instance's served model IS resident (KV pre-allocated at
        // startup) — the served list is the warm list.
        self.list_models(client, endpoint, api_key).await.ok()
    }
}

#[async_trait::async_trait]
impl BackendApi for OpenAiApi {
    async fn list_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let json = openai_models_json(client, endpoint, api_key).await?;
        let models = json["data"].as_array().ok_or(ProbeResponseShape("data"))?;
        Ok(models
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect())
    }

    async fn context_window(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
    ) -> Option<u32> {
        // #1195: vLLM has no `/api/show`; its `/v1/models` declares
        // `max_model_len` — the authoritative window (vLLM pre-allocates KV for
        // exactly it). Without this a 256k model got NO window and compacted at
        // a tiny default.
        let json = openai_models_json(client, endpoint, api_key).await.ok()?;
        parse_openai_models_window(&json, model)
    }

    fn serving(&self, served_count: usize) -> Serving {
        // A vLLM instance declares exactly one model; a gateway fronting a
        // fleet lists many.
        if served_count == 1 {
            Serving::Instance
        } else {
            Serving::Multiplexer
        }
    }
}

#[async_trait::async_trait]
impl BackendApi for EmbeddedApi {
    async fn list_models(
        &self,
        _client: &reqwest::Client,
        _endpoint: &str,
        _api_key: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        // In-process: nothing to list over the wire.
        Ok(Vec::new())
    }

    async fn context_window(
        &self,
        _client: &reqwest::Client,
        _endpoint: &str,
        _model: &str,
        _api_key: Option<&str>,
    ) -> Option<u32> {
        None
    }

    fn serving(&self, _served_count: usize) -> Serving {
        Serving::Instance
    }
}

#[async_trait::async_trait]
impl BackendApi for AnthropicApi {
    async fn list_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        api_key: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let entries = anthropic_models_entries(client, endpoint, api_key).await?;
        Ok(entries
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect())
    }

    async fn context_window(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
    ) -> Option<u32> {
        // Newer API responses declare `max_input_tokens` per model entry;
        // absent → None (the caller keeps its default). Fail-soft like the
        // other impls.
        let entries = anthropic_models_entries(client, endpoint, api_key)
            .await
            .ok()?;
        entries
            .iter()
            .find(|m| m["id"].as_str() == Some(model))
            .and_then(|m| m["max_input_tokens"].as_u64())
            .and_then(|w| u32::try_from(w).ok())
    }

    fn serving(&self, _served_count: usize) -> Serving {
        // The hosted API fronts the whole Claude family — always many models,
        // picked per request.
        Serving::Multiplexer
    }
}

/// How many `/v1/models` pages [`anthropic_models_entries`] will follow. A
/// probe must stay bounded — the full catalog fits in far fewer pages, and a
/// misbehaving proxy that always answers `has_more: true` must not hang setup.
const ANTHROPIC_MODELS_PAGE_CAP: usize = 5;

/// GET `/v1/models` with the Anthropic headers (`x-api-key` +
/// `anthropic-version`), following `after_id` pagination up to
/// [`ANTHROPIC_MODELS_PAGE_CAP`] pages. Returns the concatenated `data`
/// entries. Shared by [`AnthropicApi::list_models`] and
/// [`AnthropicApi::context_window`] — one round-trip shape, one place.
async fn anthropic_models_entries(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let base = format!("{}/v1/models?limit=100", endpoint.trim_end_matches('/'));
    let mut entries = Vec::new();
    let mut after_id: Option<String> = None;
    for _ in 0..ANTHROPIC_MODELS_PAGE_CAP {
        let url = match &after_id {
            Some(id) => format!("{base}&after_id={id}"),
            None => base.clone(),
        };
        let mut req = client
            .get(&url)
            .header("anthropic-version", ANTHROPIC_VERSION);
        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            req = req.header("x-api-key", key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(ProbeHttpStatus(resp.status()).into());
        }
        let json: serde_json::Value = resp.json().await?;
        let page = json["data"].as_array().ok_or(ProbeResponseShape("data"))?;
        entries.extend(page.iter().cloned());
        let has_more = json["has_more"].as_bool().unwrap_or(false);
        after_id = json["last_id"].as_str().map(str::to_string);
        if !has_more || after_id.is_none() {
            break;
        }
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Engine fingerprinting (which ENGINE is behind an OpenAI-wire endpoint?)
// ---------------------------------------------------------------------------

/// What a fingerprint probe expects to see in the JSON at its path. Pure data
/// consumed by [`fingerprint_matches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FingerprintMarker {
    /// A top-level object key with this name exists.
    HasKey(&'static str),
    /// Any of these top-level keys exists (version drift tolerance).
    HasAnyKey(&'static [&'static str]),
    /// A `data` array (or top-level array) whose entries carry a load-state
    /// field (`state`/`status`) — llama-server's non-`/v1` `/models` shape.
    ModelsArrayWithState,
}

/// One engine fingerprint: GET `path` on the endpoint, expect `marker`.
/// Pure data — the three-Cs table [`detect_engine`] consults. Row order is
/// the fallback chain; first match in table order wins.
#[derive(Debug, Clone, Copy)]
pub struct EngineFingerprint {
    pub engine: Engine,
    pub path: &'static str,
    pub marker: FingerprintMarker,
}

/// The built-in fingerprint table for OpenAI-wire engines, in fallback-chain
/// order: llama.cpp `/props` → vLLM `/version` → older llama.cpp builds via
/// the non-`/v1` `/models` load-state shape. `/health` is deliberately NOT a
/// fingerprint — both llama.cpp and vLLM serve it, so it distinguishes
/// nothing. (A droppable-TOML overlay for this table is a flagged follow-up;
/// the typed [`Engine`] enum is required for dispatch either way.)
pub fn builtin_engine_fingerprints() -> &'static [EngineFingerprint] {
    &[
        EngineFingerprint {
            engine: Engine::LlamaCpp,
            path: "/props",
            marker: FingerprintMarker::HasAnyKey(&["default_generation_settings", "model_path"]),
        },
        EngineFingerprint {
            engine: Engine::Vllm,
            path: "/version",
            marker: FingerprintMarker::HasKey("version"),
        },
        EngineFingerprint {
            engine: Engine::LlamaCpp,
            path: "/models",
            marker: FingerprintMarker::ModelsArrayWithState,
        },
    ]
}

/// Does `json` satisfy `marker`? Pure — unit-tested without a server.
pub fn fingerprint_matches(marker: &FingerprintMarker, json: &serde_json::Value) -> bool {
    match marker {
        FingerprintMarker::HasKey(key) => json.get(key).is_some(),
        FingerprintMarker::HasAnyKey(keys) => keys.iter().any(|k| json.get(k).is_some()),
        FingerprintMarker::ModelsArrayWithState => {
            let entries = json["data"].as_array().or_else(|| json.as_array());
            entries.is_some_and(|arr| {
                !arr.is_empty()
                    && arr
                        .iter()
                        .all(|e| e.get("state").is_some() || e.get("status").is_some())
            })
        }
    }
}

/// Fingerprint the ENGINE behind an endpoint.
///
/// `kind == Ollama` short-circuits to `Some(Engine::Ollama)` with zero HTTP
/// (the `/api/tags` race already proved it); `kind == Openai` walks
/// [`builtin_engine_fingerprints`] in order; every other kind (Embedded,
/// Anthropic) has no engine axis and returns `None`. Fail-soft: every
/// network error is just "no match" — an unfingerprintable endpoint stays a
/// perfectly usable generic OpenAI backend, it merely reports no warmth.
pub async fn detect_engine(
    client: &reqwest::Client,
    endpoint: &str,
    kind: BackendKind,
    api_key: Option<&str>,
) -> Option<Engine> {
    match kind {
        BackendKind::Ollama => return Some(Engine::Ollama),
        BackendKind::Openai => {}
        BackendKind::Embedded | BackendKind::Anthropic => return None,
    }
    let base = endpoint.trim_end_matches('/');
    for fp in builtin_engine_fingerprints() {
        let url = format!("{base}{}", fp.path);
        let Ok(resp) = maybe_bearer(client.get(&url), api_key).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(json) = resp.json::<serde_json::Value>().await else {
            continue;
        };
        if fingerprint_matches(&fp.marker, &json) {
            return Some(fp.engine);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Warm/loaded-model fetchers (the ONE home for /api/ps — #1312 discipline)
// ---------------------------------------------------------------------------

/// One Ollama `/api/ps` entry — the superset of what the dgx CLI table and
/// the TUI residency probe each need, so both can route through here instead
/// of keeping their own copies.
#[derive(Debug, Clone, PartialEq)]
pub struct PsModel {
    pub name: String,
    pub size_bytes: Option<u64>,
    pub size_vram_bytes: Option<u64>,
    pub expires_at: Option<String>,
}

/// Parse an Ollama `/api/ps` response body. Pure — unit-tested without a
/// server. Entries without a `name` are skipped.
pub fn parse_ollama_ps(json: &serde_json::Value) -> Vec<PsModel> {
    json["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(PsModel {
                        name: m["name"].as_str()?.to_string(),
                        size_bytes: m["size"].as_u64(),
                        size_vram_bytes: m["size_vram"].as_u64(),
                        expires_at: m["expires_at"].as_str().map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// GET `/api/ps` — the models Ollama currently holds in memory. Thin IO
/// shell over [`parse_ollama_ps`]; bearer-aware for Ollama Cloud.
pub async fn fetch_ollama_ps(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<PsModel>> {
    let url = format!("{}/api/ps", endpoint.trim_end_matches('/'));
    let resp = maybe_bearer(client.get(&url), api_key).send().await?;
    if !resp.status().is_success() {
        return Err(ProbeHttpStatus(resp.status()).into());
    }
    let json: serde_json::Value = resp.json().await?;
    Ok(parse_ollama_ps(&json))
}

/// Extract the WARM subset from llama-server's non-`/v1` `/models` response.
/// Pure. `None` when no entry carries a load-state field at all (capability
/// absent — adoption falls back to served order rather than guessing).
pub fn parse_llamacpp_models_warm(json: &serde_json::Value) -> Option<Vec<String>> {
    let entries = json["data"].as_array().or_else(|| json.as_array())?;
    let state_of = |e: &serde_json::Value| {
        e["state"]
            .as_str()
            .or_else(|| e["status"].as_str())
            // llama-swap reports status as an OBJECT: `{"value":"loaded", …}`
            // — read the nested value so a router that swaps models on demand
            // still reveals which model is resident (else adopt-warm never fires
            // on it). See ADR docs/decisions/managed_backend.md.
            .or_else(|| e["status"]["value"].as_str())
            .map(str::to_ascii_lowercase)
    };
    if !entries.iter().any(|e| state_of(e).is_some()) {
        return None;
    }
    Some(
        entries
            .iter()
            .filter(|e| state_of(e).is_some_and(|s| s == "loaded"))
            .filter_map(|e| e["id"].as_str().or_else(|| e["model"].as_str()))
            .map(str::to_string)
            .collect(),
    )
}

/// GET `/v1/models`, sending a bearer token when present (authenticated
/// gateways 401 otherwise). Shared by [`OpenAiApi::list_models`] and
/// [`OpenAiApi::context_window`] — one round-trip shape, one place.
async fn openai_models_json(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let url = format!("{}/v1/models", endpoint.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(ProbeHttpStatus(resp.status()).into());
    }
    Ok(resp.json().await?)
}

/// The protocol and served models discovered at one HTTP endpoint.
///
/// `endpoint` is the caller's base URL with trailing slashes removed so it can
/// be persisted directly: request paths are appended by the selected
/// [`BackendApi`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointProbeResult {
    pub endpoint: String,
    pub kind: BackendKind,
    pub models: Vec<String>,
    pub serving: Serving,
    /// The fingerprinted engine (ollama | llama.cpp | vllm), when one could
    /// be told apart. `None` = generic/unknown — fully usable, no warmth.
    pub engine: Option<Engine>,
    /// The warm (loaded-in-memory) subset of `models`, in server order.
    /// Empty = none reported or capability absent. Fail-soft.
    pub warm: Vec<String>,
}

#[derive(Debug)]
struct ProbeFailure {
    status: Option<reqwest::StatusCode>,
    reached_http_service: bool,
    detail: String,
}

impl ProbeFailure {
    fn from_error(error: anyhow::Error) -> Self {
        if let Some(status) = error.downcast_ref::<ProbeHttpStatus>() {
            return Self {
                status: Some(status.0),
                reached_http_service: true,
                detail: error.to_string(),
            };
        }

        if error.downcast_ref::<ProbeResponseShape>().is_some() {
            return Self {
                status: None,
                reached_http_service: true,
                detail: error.to_string(),
            };
        }

        let reached_http_service = error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_decode() || error.status().is_some());
        Self {
            status: None,
            reached_http_service,
            detail: error.to_string(),
        }
    }

    fn is_auth_failure(&self) -> bool {
        matches!(
            self.status,
            Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN)
        )
    }

    /// A typed auth error for this probe failure, worded per protocol
    /// surface (`"OpenAI-compatible"` + `GET /v1/models`, or `"Ollama"` +
    /// `GET /api/tags` for Ollama Cloud).
    fn authentication_error_for(
        &self,
        endpoint: &str,
        supplied_key: bool,
        protocol: &str,
        probe: &str,
    ) -> Option<anyhow::Error> {
        if !self.is_auth_failure() {
            return None;
        }
        if supplied_key {
            Some(anyhow::anyhow!(
                "authentication rejected by {protocol} inference endpoint {endpoint} \
                 ({probe} returned {}); check the bearer token",
                self.detail
            ))
        } else {
            Some(anyhow::anyhow!(
                "authentication required by {protocol} inference endpoint {endpoint} \
                 ({probe} returned {}); supply a bearer token",
                self.detail
            ))
        }
    }

    fn authentication_error(&self, endpoint: &str, supplied_key: bool) -> Option<anyhow::Error> {
        self.authentication_error_for(
            endpoint,
            supplied_key,
            "OpenAI-compatible",
            "GET /v1/models",
        )
    }
}

/// Detect whether `endpoint` speaks Ollama or an OpenAI-compatible API.
///
/// Both cheap model-list probes run concurrently. Ollama wins when both APIs
/// answer because Ollama's native surface carries more backend-specific
/// behavior than its OpenAI compatibility shim — for Ollama Cloud
/// (`https://ollama.com`, which serves both surfaces) that lands on the
/// native protocol. The optional bearer token is sent to BOTH probes: Ollama
/// Cloud 401s `/api/tags` without it, LAN Ollama ignores it, and it is only
/// ever sent when the operator configured one.
///
/// The result also carries the fingerprinted [`Engine`] and the warm
/// (loaded) model subset, both fail-soft — see [`detect_engine`] and
/// [`BackendApi::warm_models`].
pub async fn detect_endpoint(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
) -> anyhow::Result<EndpointProbeResult> {
    let endpoint = endpoint.trim_end_matches('/');
    let ollama_api = api_for(BackendKind::Ollama);
    let openai_api = api_for(BackendKind::Openai);
    let (ollama, openai) = tokio::join!(
        ollama_api.list_models(client, endpoint, api_key),
        openai_api.list_models(client, endpoint, api_key),
    );
    let supplied_key = api_key.is_some_and(|key| !key.trim().is_empty());

    match (ollama, openai) {
        (Ok(ollama_models), Ok(openai_models))
            if ollama_models.is_empty() && !openai_models.is_empty() =>
        {
            Ok(finish_probe(
                client,
                endpoint,
                BackendKind::Openai,
                openai_models,
                api_key,
            )
            .await)
        }
        (Ok(models), Err(openai_error)) if models.is_empty() => {
            let openai = ProbeFailure::from_error(openai_error);
            if let Some(error) = openai.authentication_error(endpoint, supplied_key) {
                return Err(error);
            }
            Ok(finish_probe(client, endpoint, BackendKind::Ollama, models, api_key).await)
        }
        (Ok(models), _) => {
            Ok(finish_probe(client, endpoint, BackendKind::Ollama, models, api_key).await)
        }
        (Err(_), Ok(models)) => {
            Ok(finish_probe(client, endpoint, BackendKind::Openai, models, api_key).await)
        }
        (Err(ollama_error), Err(openai_error)) => {
            let ollama = ProbeFailure::from_error(ollama_error);
            let openai = ProbeFailure::from_error(openai_error);
            if let Some(error) = openai.authentication_error(endpoint, supplied_key) {
                return Err(error);
            }
            // Ollama Cloud with a wrong/missing token: /api/tags 401s while
            // /v1/models fails some other way — name the bearer token rather
            // than reporting "unsupported endpoint".
            if let Some(error) =
                ollama.authentication_error_for(endpoint, supplied_key, "Ollama", "GET /api/tags")
            {
                return Err(error);
            }

            if ollama.reached_http_service || openai.reached_http_service {
                anyhow::bail!(
                    "unsupported inference endpoint {endpoint}: neither Ollama GET /api/tags nor \
                     OpenAI-compatible GET /v1/models succeeded \
                     (Ollama: {}; OpenAI-compatible: {})",
                    ollama.detail,
                    openai.detail
                );
            }

            anyhow::bail!(
                "unreachable inference endpoint {endpoint}: both protocol probes failed \
                 (Ollama GET /api/tags: {}; OpenAI-compatible GET /v1/models: {})",
                ollama.detail,
                openai.detail
            );
        }
    }
}

/// Assemble the final probe result: derive serving from the served count,
/// fingerprint the engine, and fetch the warm subset. Engine and warmth are
/// fail-soft (≤2 extra bounded round-trips); a vLLM instance reuses the
/// already-fetched served list as its warm list (zero extra HTTP).
async fn finish_probe(
    client: &reqwest::Client,
    endpoint: &str,
    kind: BackendKind,
    models: Vec<String>,
    api_key: Option<&str>,
) -> EndpointProbeResult {
    let serving = api_for(kind).serving(models.len());
    let engine = detect_engine(client, endpoint, kind, api_key).await;
    let warm = match engine {
        // The vLLM served list IS the warm list — no second fetch.
        Some(Engine::Vllm) => models.clone(),
        _ => api_for_engine(kind, engine)
            .warm_models(client, endpoint, api_key)
            .await
            .unwrap_or_default(),
    };
    EndpointProbeResult {
        endpoint: endpoint.to_string(),
        kind,
        models,
        serving,
        engine,
        warm,
    }
}

/// True when an OpenAI error body says the model is served only on
/// `/v1/responses` (gpt-5-codex et al.). Pure — unit-tested without a server.
pub fn is_responses_only_error(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("v1/responses")
        && (lower.contains("only supported")
            || lower.contains("only available")
            || lower.contains("unsupported_api")
            // gpt-5.6-era phrasing: "Function tools with reasoning_effort are
            // not supported for <model> in /v1/chat/completions. To use
            // function tools, use /v1/responses …" — the server naming
            // /v1/responses as the fix IS the responses-only signal.
            || lower.contains("not supported"))
}

/// Detect which OpenAI HTTP surface `endpoint` wants for `model`.
///
/// Posts a one-token probe to `/v1/chat/completions`. A responses-only error
/// body selects [`OpenAiApiSurface::Responses`]; any other reachable chat-surface
/// outcome (2xx or garden-variety 4xx/5xx) keeps [`OpenAiApiSurface::ChatCompletions`].
/// When chat returns a bare 404, a second one-token probe against
/// `/v1/responses` decides. Unreachable endpoints error out so the caller can
/// keep its file hint.
pub async fn detect_openai_api(
    client: &reqwest::Client,
    endpoint: &str,
    model: &str,
    api_key: Option<&str>,
) -> anyhow::Result<OpenAiApiSurface> {
    let base = endpoint.trim_end_matches('/');
    let chat_url = format!("{base}/v1/chat/completions");
    // The probe must look like a real agent request or it lies: gpt-5.6-class
    // models accept a bare chat completion yet reject FUNCTION TOOLS outside
    // /v1/responses, so a tool-free probe adopts chat_completions and every
    // actual turn then 400s. One inert tool makes the server show its hand.
    // `max_completion_tokens` (not the deprecated `max_tokens`) for the same
    // reason — reasoning models reject `max_tokens` before evaluating tools.
    let chat_body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_completion_tokens": 1,
        "stream": false,
        "tools": [{
            "type": "function",
            "function": {
                "name": "probe_noop",
                "description": "capability probe — never called",
                "parameters": {"type": "object", "properties": {}},
            },
        }],
    });
    let mut req = client.post(&chat_url).json(&chat_body);
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        req = req.bearer_auth(key);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        return Ok(OpenAiApiSurface::ChatCompletions);
    }
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        anyhow::bail!(
            "authentication rejected while detecting the OpenAI API surface (HTTP {status})"
        );
    }
    if is_responses_only_error(&body) {
        return Ok(OpenAiApiSurface::Responses);
    }
    if status != reqwest::StatusCode::NOT_FOUND {
        // Surface exists (auth/rate-limit/validation) — stick with chat.
        return Ok(OpenAiApiSurface::ChatCompletions);
    }

    // Bare 404 on chat: see whether /v1/responses is the live surface.
    let responses_url = format!("{base}/v1/responses");
    let responses_body = crate::responses_wire::generation_probe_body(model);
    let mut req = client.post(&responses_url).json(&responses_body);
    if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
        req = req.bearer_auth(key);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if matches!(
                status,
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) {
                anyhow::bail!(
                    "authentication rejected while detecting the OpenAI API surface (HTTP {status})"
                );
            }
            if status.is_success()
                || status != reqwest::StatusCode::NOT_FOUND
                || is_responses_only_error(&body)
            {
                Ok(OpenAiApiSurface::Responses)
            } else {
                Ok(OpenAiApiSurface::ChatCompletions)
            }
        }
        Err(_) => Ok(OpenAiApiSurface::ChatCompletions),
    }
}

/// Extract the context window from an Ollama `/api/show` response: the
/// architecture `*.context_length` (smallest if several), capped by a
/// Modelfile `num_ctx` override. Pure — unit-tested without a server.
pub fn parse_ollama_show_window(json: &serde_json::Value) -> Option<u32> {
    let arch_limit: Option<u32> = json["model_info"].as_object().and_then(|info| {
        if let Some(v) = info.get("context_length").and_then(|v| v.as_u64()) {
            return Some(v as u32);
        }
        info.iter()
            .filter(|(k, _)| k.ends_with(".context_length"))
            .filter_map(|(_, v)| v.as_u64())
            .map(|v| v as u32)
            .min()
    });
    let modelfile_ctx: Option<u32> = json["parameters"].as_str().and_then(|params| {
        params.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            if parts.next()? == "num_ctx" {
                parts.next()?.parse::<u32>().ok()
            } else {
                None
            }
        })
    });
    match (arch_limit, modelfile_ctx) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Extract a model's `max_model_len` from an OpenAI `/v1/models` response
/// (#1195): the entry whose `id` matches, else the first that declares a
/// window (a single-model vLLM instance). Pure.
pub fn parse_openai_models_window(json: &serde_json::Value, model: &str) -> Option<u32> {
    let data = json["data"].as_array()?;
    let window_of = |e: &serde_json::Value| {
        [
            "max_model_len",
            "context_window",
            "context_length",
            "max_input_tokens",
        ]
        .into_iter()
        .find_map(|field| e[field].as_u64())
        .and_then(|value| u32::try_from(value).ok())
    };
    if let Some(w) = data
        .iter()
        .find(|e| e["id"].as_str() == Some(model))
        .and_then(&window_of)
    {
        return Some(w);
    }
    data.iter().find_map(&window_of)
}

/// What an endpoint reported it serves. Produced by the fetchers below (or a
/// test), consumed by [`adopt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// Model names/ids as the server listed them, in server order.
    pub models: Vec<String>,
    /// The WARM (loaded-in-memory) subset, in server order. Empty = none
    /// reported or capability absent. Entries not present in `models` are
    /// ignored by [`adopt`] (a stale `/api/ps` race must not adopt a model
    /// the server no longer lists).
    pub warm: Vec<String>,
}

impl Served {
    /// A served list with no warmth information — the shape every pre-warm
    /// caller had.
    pub fn from_models(models: Vec<String>) -> Self {
        Self {
            models,
            warm: Vec::new(),
        }
    }
}

/// The adoption decision for one backend at session start / backend switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    /// The model this session should use. `None` only when the server listed
    /// nothing AND neither request nor config named one (caller surfaces that).
    pub model: Option<String>,
    /// The serving shape: declared in the file if set, else derived from what
    /// the probe saw ([`derive_serving`]).
    pub serving: Serving,
    /// True when a requested/declared model was overridden by an instance
    /// backend's served reality — the caller should tell the user (`/model` is
    /// fixed on an instance; restart the server or `/backends` to switch).
    pub requested_ignored: bool,
    /// True when a requested model is NOT in a multiplexer's served list
    /// (#1122 fail-soft: a restored/typo'd model must not brick the session) —
    /// the caller warns and the adoption fell back to declared/first-served.
    pub requested_unavailable: bool,
    /// True when a `ManagedMode::Shared` backend adopted a currently-WARM model
    /// instead of forcing its pinned/first model to load — the cooperative path
    /// that avoids swapping a model another agent may be using. Always false for
    /// unmanaged / `Dedicated` backends and for instance serving.
    pub adopted_warm: bool,
    /// `Some(pinned)` when adopt-warm took the cooperative default (`model` = the
    /// warm model) but a DIFFERENT model was pinned/configured — the pinned model
    /// is named here so an interactive caller can offer "adopt the warm model, or
    /// force a swap to your pin?"; a headless caller keeps the cooperative default
    /// and never silently evicts the warm model. `None` = no conflict.
    pub pin_conflict: Option<String>,
}

/// The pure adoption rule. `served` is what the probe saw; `requested` is the
/// session's explicit ask (e.g. `/model X` / env override), which outranks the
/// file's declared model on a multiplexer and is overridden (flagged) on an
/// instance.
pub fn adopt(backend: &BackendConfig, served: &Served, requested: Option<&str>) -> Adoption {
    let serving = backend.serving.unwrap_or_else(|| {
        // Serving derivation needs a concrete wire kind. Callers that omit
        // `kind` must probe first (`detect_endpoint`) and pass the detected
        // kind on the backend view; until then treat as multiplexer (Ollama
        // law) so adopt stays pure and never invents an openai/instance shape.
        let kind = backend.kind.unwrap_or(BackendKind::Ollama);
        api_for(kind).serving(served.models.len())
    });
    match serving {
        Serving::Instance => {
            // The server dictates: the one served model, unconditionally.
            let adopted = served.models.first().cloned();
            let asked = requested
                .map(str::to_string)
                .or_else(|| backend.effective_model().map(str::to_string));
            let requested_ignored = match (&adopted, &asked) {
                (Some(a), Some(r)) => a != r,
                // Server listed nothing: fall back to what was asked; nothing
                // was overridden.
                (None, _) => false,
                (_, None) => false,
            };
            Adoption {
                model: adopted.or(asked),
                serving,
                requested_ignored,
                requested_unavailable: false,
                // An instance serves one bound model — there is nothing to
                // adopt-warm and no swap to force.
                adopted_warm: false,
                pin_conflict: None,
            }
        }
        Serving::Multiplexer => {
            // #1122 fail-soft: a requested model (session override OR the
            // settings-restore channel) that the endpoint does NOT serve is
            // dropped with a flag — a typo'd restore must never brick every
            // future launch. An EMPTY served list (endpoint mid-restart)
            // trusts the request rather than second-guessing it.
            let requested_ok =
                requested.map(|r| served.models.is_empty() || served.models.iter().any(|m| m == r));
            let requested_unavailable = requested_ok == Some(false);
            // The model this session pins: a served session request outranks
            // the file's declared model.
            let pin: Option<String> = requested
                .filter(|_| requested_ok == Some(true))
                .map(str::to_string)
                .or_else(|| backend.effective_model().map(str::to_string));
            // The first WARM model the server still lists (a stale `/api/ps`
            // entry not in `models` is ignored — see [`Served::warm`]).
            let warm: Option<String> = served
                .warm
                .iter()
                .find(|w| served.models.contains(w))
                .cloned();

            // `ManagedMode::Shared` adopt-warm: a cooperative guest PREFERS a
            // warm model over forcing its pin to load — a swap that would evict
            // the model another agent may be using. When the pin differs from
            // the warm model, take the cooperative default (warm) and surface
            // the pin as a force-swap choice via `pin_conflict`. Every other
            // case keeps the historical precedence (pin → first-warm →
            // first-served), where warmth is only a tiebreaker and never
            // overrides an explicit choice.
            let shared = backend.managed == Some(ManagedMode::Shared);
            let (model, adopted_warm, pin_conflict) = match warm {
                Some(w) if shared && pin.as_deref() != Some(w.as_str()) => {
                    let conflict = pin.filter(|p| p != &w);
                    (Some(w), true, conflict)
                }
                other => {
                    let model = pin.or(other).or_else(|| served.models.first().cloned());
                    (model, false, None)
                }
            };
            Adoption {
                model,
                serving,
                requested_ignored: false,
                requested_unavailable,
                adopted_warm,
                pin_conflict,
            }
        }
    }
}

/// List models from an Ollama endpoint via `GET /api/tags`.
pub async fn fetch_ollama_models(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Vec<String>> {
    OllamaApi.list_models(client, url, None).await
}

/// List models from an OpenAI-compatible endpoint via `GET /v1/models`,
/// sending `Authorization: Bearer <token>` when the backend has one —
/// authenticated gateways 401 the probe otherwise and the session never
/// adopts.
pub async fn fetch_openai_models_auth(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    OpenAiApi.list_models(client, url, api_key).await
}

/// Unauthenticated variant (kept for callers without a key in hand).
pub async fn fetch_openai_models(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<Vec<String>> {
    fetch_openai_models_auth(client, url, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendKind, OpenAiApi as OpenAiApiSurface};
    use std::time::Duration;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn openai_backend(model: Option<&str>, serving: Option<Serving>) -> BackendConfig {
        BackendConfig {
            name: "b".into(),
            endpoint: "http://h:8000".into(),
            model: model.map(str::to_string),
            kind: Some(BackendKind::Openai),
            serving,
            ..Default::default()
        }
    }

    fn served(models: &[&str]) -> Served {
        Served::from_models(models.iter().map(|m| m.to_string()).collect())
    }

    fn served_warm(models: &[&str], warm: &[&str]) -> Served {
        Served {
            models: models.iter().map(|m| m.to_string()).collect(),
            warm: warm.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn managed_mux(model: Option<&str>, mode: ManagedMode) -> BackendConfig {
        BackendConfig {
            managed: Some(mode),
            ..openai_backend(model, Some(Serving::Multiplexer))
        }
    }

    #[tokio::test]
    async fn generation_probe_requires_an_authenticated_valid_chat_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi"}}]
            })))
            .mount(&server)
            .await;

        let result = verify_generation(
            &reqwest::Client::new(),
            BackendKind::Openai,
            Some(OpenAiApiSurface::ChatCompletions),
            &server.uri(),
            "selected-model",
            Some("secret-token"),
        )
        .await;

        assert_eq!(
            result,
            GenerationCheck::Accepted(Some(OpenAiApiSurface::ChatCompletions))
        );
        let requests = server.received_requests().await.expect("journal");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "selected-model");
        assert_eq!(body["messages"][0]["content"], "Reply with OK.");
        assert_eq!(body["max_tokens"], 8);
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["stream"], false);
    }

    #[tokio::test]
    async fn generation_probe_rejects_auth_and_malformed_success_envelopes() {
        let auth = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&auth)
            .await;
        assert_eq!(
            verify_generation(
                &reqwest::Client::new(),
                BackendKind::Openai,
                Some(OpenAiApiSurface::ChatCompletions),
                &auth.uri(),
                "m",
                None,
            )
            .await,
            GenerationCheck::Rejected(403)
        );

        let malformed = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "chat.completion"
            })))
            .mount(&malformed)
            .await;
        assert!(matches!(
            verify_generation(
                &reqwest::Client::new(),
                BackendKind::Openai,
                Some(OpenAiApiSurface::ChatCompletions),
                &malformed.uri(),
                "m",
                None,
            )
            .await,
            GenerationCheck::Unverified(GenerationFailure::InvalidEnvelope)
        ));
    }

    #[tokio::test]
    async fn generation_probe_negotiates_the_responses_surface_after_chat_404() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "completed",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "hi"}]
                }]
            })))
            .mount(&server)
            .await;

        assert_eq!(
            verify_generation(
                &reqwest::Client::new(),
                BackendKind::Openai,
                None,
                &server.uri(),
                "responses-model",
                None,
            )
            .await,
            GenerationCheck::Accepted(Some(OpenAiApiSurface::Responses))
        );
        let requests = server.received_requests().await.expect("journal");
        let responses: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(responses["store"], false);
    }

    #[test]
    fn incomplete_responses_probe_requires_clean_recognized_partial_output() {
        let partial = serde_json::json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "partial"}]
            }]
        });
        assert_eq!(
            classify_responses_generation(
                reqwest::StatusCode::OK,
                &serde_json::to_vec(&partial).unwrap(),
            ),
            GenerationCheck::Accepted(Some(OpenAiApiSurface::Responses))
        );

        let mut with_error = partial.clone();
        with_error["error"] = serde_json::json!({"message": "provider failure"});
        assert!(matches!(
            classify_responses_generation(
                reqwest::StatusCode::OK,
                &serde_json::to_vec(&with_error).unwrap(),
            ),
            GenerationCheck::Unverified(GenerationFailure::InvalidResponsesPayload)
        ));

        let refusal = serde_json::json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{
                "type": "message",
                "content": [{"type": "refusal", "refusal": "declined"}]
            }]
        });
        assert!(matches!(
            classify_responses_generation(
                reqwest::StatusCode::OK,
                &serde_json::to_vec(&refusal).unwrap(),
            ),
            GenerationCheck::Unverified(GenerationFailure::InvalidResponsesPayload)
        ));

        let unrecognized = serde_json::json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "reasoning", "summary": []}]
        });
        assert!(matches!(
            classify_responses_generation(
                reqwest::StatusCode::OK,
                &serde_json::to_vec(&unrecognized).unwrap(),
            ),
            GenerationCheck::Unverified(GenerationFailure::InvalidResponsesPayload)
        ));
    }

    #[tokio::test]
    async fn responses_probe_never_returns_provider_text_or_bearer_material() {
        const BEARER_SENTINEL: &str = "probe-secret-must-not-escape";
        const BODY_SENTINEL: &str = "provider-body-must-not-escape";
        let escape = char::from(27);
        let bell = char::from(7);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(header("authorization", format!("Bearer {BEARER_SENTINEL}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "completed",
                "error": {"message": format!(
                    "{BEARER_SENTINEL} {BODY_SENTINEL} {escape}[31mred{bell}"
                )},
                "output": [{
                    "type": "message",
                    "content": [{
                        "type": "refusal",
                        "refusal": format!("{BODY_SENTINEL} {escape}[2J")
                    }]
                }]
            })))
            .mount(&server)
            .await;

        let result = verify_generation(
            &reqwest::Client::new(),
            BackendKind::Openai,
            Some(OpenAiApiSurface::Responses),
            &server.uri(),
            "model",
            Some(BEARER_SENTINEL),
        )
        .await;
        let GenerationCheck::Unverified(reason) = result else {
            panic!("provider error/refusal must fail closed: {result:?}");
        };
        let rendered = reason.to_string();

        assert_eq!(reason, GenerationFailure::InvalidResponsesPayload);
        assert!(!rendered.contains(BEARER_SENTINEL));
        assert!(!rendered.contains(BODY_SENTINEL));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.chars().any(char::is_control));
    }

    // ── ManagedMode::Shared adopt-warm (ADR docs/decisions/managed_backend.md) ──

    #[test]
    fn shared_adopts_warm_when_nothing_is_pinned() {
        // A cooperative guest with no pin uses whatever model is already loaded.
        let b = managed_mux(None, ManagedMode::Shared);
        let a = adopt(&b, &served_warm(&["cold", "warm-y"], &["warm-y"]), None);
        assert_eq!(a.model.as_deref(), Some("warm-y"));
        assert!(a.adopted_warm);
        assert_eq!(a.pin_conflict, None);
    }

    #[test]
    fn shared_adopts_warm_over_a_conflicting_pin_and_offers_the_force_choice() {
        // Pinned "mine" but "warm-y" is loaded: adopt the warm one (never evict
        // another agent's model silently) and hand the pin back as a force choice.
        let b = managed_mux(Some("mine"), ManagedMode::Shared);
        let a = adopt(&b, &served_warm(&["mine", "warm-y"], &["warm-y"]), None);
        assert_eq!(
            a.model.as_deref(),
            Some("warm-y"),
            "cooperative default = warm"
        );
        assert!(a.adopted_warm);
        assert_eq!(
            a.pin_conflict.as_deref(),
            Some("mine"),
            "the pin is surfaced as the force-swap choice"
        );
    }

    #[test]
    fn shared_keeps_the_pin_when_the_pin_is_already_warm() {
        // No swap, no conflict — the pinned model happens to be resident.
        let b = managed_mux(Some("mine"), ManagedMode::Shared);
        let a = adopt(&b, &served_warm(&["mine", "other"], &["mine"]), None);
        assert_eq!(a.model.as_deref(), Some("mine"));
        assert!(!a.adopted_warm);
        assert_eq!(a.pin_conflict, None);
    }

    #[test]
    fn shared_loads_the_pin_when_nothing_is_warm() {
        // Nothing resident: fall back to the pin (an unavoidable cold load).
        let b = managed_mux(Some("mine"), ManagedMode::Shared);
        let a = adopt(&b, &served_warm(&["mine", "other"], &[]), None);
        assert_eq!(a.model.as_deref(), Some("mine"));
        assert!(!a.adopted_warm);
        assert_eq!(a.pin_conflict, None);
    }

    #[test]
    fn shared_surfaces_the_conflict_for_a_session_request_too() {
        // An explicit /model request is still a pin: on a Shared box it does not
        // silently force a swap — the warm model wins by default and the request
        // is offered as the force choice (the two-agent clash the ADR guards).
        let b = managed_mux(Some("declared"), ManagedMode::Shared);
        let a = adopt(
            &b,
            &served_warm(&["asked", "warm-y"], &["warm-y"]),
            Some("asked"),
        );
        assert_eq!(a.model.as_deref(), Some("warm-y"));
        assert!(a.adopted_warm);
        assert_eq!(a.pin_conflict.as_deref(), Some("asked"));
    }

    #[test]
    fn dedicated_forces_the_pin_and_never_adopts_warm() {
        // "I own this box": force the configured model even if another is warm.
        let b = managed_mux(Some("mine"), ManagedMode::Dedicated);
        let a = adopt(&b, &served_warm(&["mine", "warm-y"], &["warm-y"]), None);
        assert_eq!(a.model.as_deref(), Some("mine"), "dedicated forces its pin");
        assert!(!a.adopted_warm);
        assert_eq!(a.pin_conflict, None);
    }

    #[test]
    fn unmanaged_keeps_precedence_warm_is_only_a_tiebreaker() {
        // Regression: an ordinary (unmanaged) backend is unchanged — the declared
        // pin wins over a differently-warm model (warmth never overrides a pin).
        let b = openai_backend(Some("declared"), Some(Serving::Multiplexer));
        let a = adopt(&b, &served_warm(&["declared", "warm-y"], &["warm-y"]), None);
        assert_eq!(a.model.as_deref(), Some("declared"));
        assert!(!a.adopted_warm);
        assert_eq!(a.pin_conflict, None);
    }

    #[tokio::test]
    async fn detect_endpoint_prefers_ollama_when_both_protocols_answer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "qwen3:30b"}, {"name": "llama3.1:8b"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "openai-shim-model"}]
            })))
            .mount(&server)
            .await;

        let result = detect_endpoint(&reqwest::Client::new(), &format!("{}/", server.uri()), None)
            .await
            .unwrap();

        assert_eq!(result.endpoint, server.uri());
        assert_eq!(result.kind, BackendKind::Ollama);
        assert_eq!(result.models, vec!["qwen3:30b", "llama3.1:8b"]);
        assert_eq!(result.serving, Serving::Multiplexer);
    }

    #[tokio::test]
    async fn detect_endpoint_prefers_the_nonempty_openai_surface() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "served-model"}]
            })))
            .mount(&server)
            .await;

        let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
            .await
            .unwrap();

        assert_eq!(result.kind, BackendKind::Openai);
        assert_eq!(result.models, vec!["served-model"]);
        assert_eq!(result.serving, Serving::Instance);
    }

    #[tokio::test]
    async fn detect_endpoint_finds_authenticated_openai_instance() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "ornith-35b"}]
            })))
            .mount(&server)
            .await;

        let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), Some("secret-token"))
            .await
            .unwrap();

        assert_eq!(result.kind, BackendKind::Openai);
        assert_eq!(result.models, vec!["ornith-35b"]);
        assert_eq!(result.serving, Serving::Instance);
    }

    #[tokio::test]
    async fn detect_endpoint_derives_openai_gateway_as_multiplexer() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "model-a"}, {"id": "model-b"}]
            })))
            .mount(&server)
            .await;

        let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
            .await
            .unwrap();

        assert_eq!(result.kind, BackendKind::Openai);
        assert_eq!(result.models, vec!["model-a", "model-b"]);
        assert_eq!(result.serving, Serving::Multiplexer);
    }

    #[tokio::test]
    async fn detect_endpoint_ignores_success_with_the_wrong_protocol_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "real-model"}]
            })))
            .mount(&server)
            .await;

        let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
            .await
            .unwrap();

        assert_eq!(result.kind, BackendKind::Openai);
        assert_eq!(result.models, vec!["real-model"]);
    }

    #[tokio::test]
    async fn detect_endpoint_reports_authentication_required_without_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let error = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("authentication required"), "{error}");
        assert!(error.contains("401"), "{error}");
        assert!(error.contains("bearer token"), "{error}");
        assert!(error.contains(&server.uri()), "{error}");
    }

    #[tokio::test]
    async fn detect_endpoint_reports_openai_auth_when_ollama_lists_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": []
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let error = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("authentication required"), "{error}");
    }

    #[tokio::test]
    async fn detect_endpoint_sends_the_bearer_to_both_probes() {
        // The Ollama Cloud contract (deliberate inversion of the former
        // `detect_endpoint_never_sends_the_openai_token_to_ollama`):
        // https://ollama.com 401s `/api/tags` without a bearer, so the key —
        // sent only when the operator configured one — goes to BOTH probes.
        // /api/tags answers ONLY with the bearer; both surfaces answering
        // also proves the tie-break stays native-Ollama.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "gpt-oss:120b"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "gpt-oss:120b"}]
            })))
            .mount(&server)
            .await;

        let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), Some("secret-token"))
            .await
            .unwrap();

        assert_eq!(result.kind, BackendKind::Ollama);
        assert_eq!(result.models, vec!["gpt-oss:120b"]);
        server.verify().await;
    }

    #[tokio::test]
    async fn detect_endpoint_reports_ollama_auth_rejected_with_token() {
        // Ollama Cloud with a wrong token: /api/tags 401s while /v1/models
        // 404s — the error must name the bearer token, not claim the endpoint
        // is "unsupported".
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let err = detect_endpoint(&reqwest::Client::new(), &server.uri(), Some("wrong-token"))
            .await
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("authentication rejected by Ollama"),
            "auth-classified, got: {err}"
        );
        assert!(err.contains("check the bearer token"), "actionable: {err}");
    }

    #[tokio::test]
    async fn detect_endpoint_reports_authentication_rejected_with_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer wrong-token"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let error = detect_endpoint(&reqwest::Client::new(), &server.uri(), Some("wrong-token"))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("authentication rejected"), "{error}");
        assert!(error.contains("403"), "{error}");
        assert!(error.contains("check the bearer token"), "{error}");
    }

    #[tokio::test]
    async fn detect_endpoint_reports_unsupported_http_service() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let error = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("unsupported inference endpoint"), "{error}");
        assert!(error.contains("/api/tags"), "{error}");
        assert!(error.contains("/v1/models"), "{error}");
        assert!(error.contains("HTTP 404"), "{error}");
    }

    #[tokio::test]
    async fn detect_endpoint_reports_unreachable_when_probes_time_out() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(250))
                    .set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();

        let error = detect_endpoint(&client, &server.uri(), None)
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("unreachable inference endpoint"), "{error}");
        assert!(error.contains(&server.uri()), "{error}");
    }

    #[test]
    fn serving_and_openai_window_via_the_trait() {
        // #backend-trait + #1195: the OpenAi impl derives serving from the
        // served count and reads max_model_len; Ollama is always a multiplexer.
        assert_eq!(OpenAiApi.serving(1), Serving::Instance);
        assert_eq!(OpenAiApi.serving(3), Serving::Multiplexer);
        assert_eq!(OllamaApi.serving(1), Serving::Multiplexer);
        assert_eq!(EmbeddedApi.serving(1), Serving::Instance);
        // The vLLM 256k window is read, not defaulted away.
        let json = serde_json::json!({
            "data": [{ "id": "ornith", "max_model_len": 262144u64 }]
        });
        assert_eq!(parse_openai_models_window(&json, "ornith"), Some(262144));
        assert_eq!(
            parse_openai_models_window(&json, "other"),
            Some(262144),
            "single-instance fallback"
        );
        assert_eq!(
            parse_openai_models_window(&serde_json::json!({"data":[{"id":"m"}]}), "m"),
            None
        );
    }

    #[test]
    fn openai_window_accepts_common_gateway_metadata_fields() {
        for field in ["context_window", "context_length", "max_input_tokens"] {
            let mut entry = serde_json::json!({"id": "hosted/model"});
            entry[field] = serde_json::json!(1_000_000u64);
            let json = serde_json::json!({"data": [entry]});
            assert_eq!(
                parse_openai_models_window(&json, "hosted/model"),
                Some(1_000_000),
                "field {field}"
            );
        }
    }

    #[test]
    fn instance_adopts_the_served_model_unconditionally() {
        // The DGX case: config says one thing, vLLM on :8000 serves another —
        // the server dictates, and the override is FLAGGED for honest UX.
        let b = openai_backend(Some("configured-model"), None);
        let a = adopt(&b, &served(&["ornith-1.0-35b"]), None);
        assert_eq!(a.serving, Serving::Instance, "derived: one served id");
        assert_eq!(a.model.as_deref(), Some("ornith-1.0-35b"));
        assert!(a.requested_ignored, "config disagreed and was overridden");

        // Agreement is not an override.
        let agree = adopt(&openai_backend(Some("m"), None), &served(&["m"]), None);
        assert!(!agree.requested_ignored);
    }

    #[test]
    fn instance_ignores_a_session_request_too() {
        let b = openai_backend(None, Some(Serving::Instance));
        let a = adopt(&b, &served(&["real"]), Some("wish"));
        assert_eq!(a.model.as_deref(), Some("real"));
        assert!(a.requested_ignored);
    }

    #[test]
    fn multiplexer_precedence_requested_then_declared_then_served() {
        let b = BackendConfig {
            name: "o".into(),
            endpoint: "http://h:11434".into(),
            model: Some("declared".into()),
            kind: Some(BackendKind::Ollama),
            ..Default::default()
        };
        // (C3/#1122: a request must be SERVED to win — unserved requests
        // drop fail-soft, covered by its own test below.)
        let s = served(&["asked", "first", "second"]);
        assert_eq!(
            adopt(&b, &s, Some("asked")).model.as_deref(),
            Some("asked"),
            "a served session request wins on a multiplexer"
        );
        assert_eq!(adopt(&b, &s, None).model.as_deref(), Some("declared"));
        let bare = BackendConfig {
            model: None,
            ..b.clone()
        };
        // First-SERVED (server order) when nothing is requested or declared.
        assert_eq!(adopt(&bare, &s, None).model.as_deref(), Some("asked"));
        assert!(!adopt(&b, &s, Some("asked")).requested_ignored);
    }

    #[test]
    fn openai_gateway_with_many_models_is_a_multiplexer() {
        let b = openai_backend(None, None);
        let a = adopt(&b, &served(&["a", "b", "c"]), Some("b"));
        assert_eq!(a.serving, Serving::Multiplexer);
        assert_eq!(a.model.as_deref(), Some("b"));
    }

    #[test]
    fn declared_serving_beats_derivation() {
        // A file that pins serving="instance" stays an instance even when the
        // gateway lists several models (operator knows best; doctor shows drift).
        let b = openai_backend(None, Some(Serving::Instance));
        let a = adopt(&b, &served(&["x", "y"]), None);
        assert_eq!(a.serving, Serving::Instance);
        assert_eq!(a.model.as_deref(), Some("x"));
    }

    #[test]
    fn multiplexer_drops_an_unserved_requested_model_fail_soft() {
        // #1122 (C3): the kid's-account case — a typo persisted to
        // settings.toml restores as `requested` forever. The endpoint doesn't
        // serve it → drop it (flagged), fall back to declared/first-served,
        // and the session comes up usable instead of 404ing every launch.
        let b = BackendConfig {
            name: "o".into(),
            endpoint: "http://h:11434".into(),
            model: Some("declared".into()),
            kind: Some(BackendKind::Ollama),
            ..Default::default()
        };
        let s = served(&["declared", "other"]);
        let a = adopt(&b, &s, Some("quen2.5-coder:7b"));
        assert_eq!(a.model.as_deref(), Some("declared"));
        assert!(a.requested_unavailable);
        // A SERVED requested model is honored, unflagged.
        let ok = adopt(&b, &s, Some("other"));
        assert_eq!(ok.model.as_deref(), Some("other"));
        assert!(!ok.requested_unavailable);
        // Empty served list (mid-restart): trust the request, unflagged.
        let trust = adopt(&b, &served(&[]), Some("anything"));
        assert_eq!(trust.model.as_deref(), Some("anything"));
        assert!(!trust.requested_unavailable);
    }

    #[test]
    fn empty_probe_falls_back_without_flagging() {
        // Reachable but nothing listed (vLLM mid-restart): fall back to the
        // request/config; nothing was overridden.
        let b = openai_backend(Some("hint"), Some(Serving::Instance));
        let a = adopt(&b, &served(&[]), None);
        assert_eq!(a.model.as_deref(), Some("hint"));
        assert!(!a.requested_ignored);
        // Nothing anywhere → None; the caller surfaces it.
        let bare = openai_backend(None, Some(Serving::Instance));
        assert_eq!(adopt(&bare, &served(&[]), None).model, None);
    }

    #[test]
    fn responses_only_error_recognizes_openai_wording() {
        assert!(is_responses_only_error(
            r#"{"error":{"message":"This model is only supported in v1/responses","code":"unsupported_api"}}"#
        ));
        // gpt-5.6-era phrasing, hit in field testing: tools work, but only on
        // the Responses surface.
        assert!(is_responses_only_error(
            r#"{"error":{"message":"Function tools with reasoning_effort are not supported for gpt-5.6-sol in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.","type":"invalid_request_error","param":"reasoning_effort"}}"#
        ));
        assert!(!is_responses_only_error(
            r#"{"error":{"message":"model not found"}}"#
        ));
        // "not supported" alone (no mention of /v1/responses) must NOT flip
        // the surface — plain tools-unsupported models stay on chat.
        assert!(!is_responses_only_error(
            r#"{"error":{"message":"tools are not supported by this model"}}"#
        ));
        assert!(!is_responses_only_error("HTTP 404"));
    }

    #[tokio::test]
    async fn detect_openai_api_probe_carries_a_tool_and_no_legacy_max_tokens() {
        // The probe must look like a real agent request (tools present,
        // `max_completion_tokens` not the deprecated `max_tokens`) or
        // tools-require-responses models pass it and 400 on real turns.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "ok"}}]
            })))
            .mount(&server)
            .await;
        detect_openai_api(&reqwest::Client::new(), &server.uri(), "m", None)
            .await
            .unwrap();
        let reqs = server.received_requests().await.expect("journal");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert!(body["tools"].is_array() && !body["tools"].as_array().unwrap().is_empty());
        assert_eq!(body["max_completion_tokens"], serde_json::json!(1));
        assert!(body.get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn detect_openai_api_adopts_responses_on_gpt56_tools_rejection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {
                    "message": "Function tools with reasoning_effort are not supported for gpt-5.6-sol in /v1/chat/completions. To use function tools, use /v1/responses or set reasoning_effort to 'none'.",
                    "type": "invalid_request_error",
                    "param": "reasoning_effort",
                }
            })))
            .mount(&server)
            .await;
        let api = detect_openai_api(&reqwest::Client::new(), &server.uri(), "gpt-5.6-sol", None)
            .await
            .unwrap();
        assert_eq!(api, OpenAiApiSurface::Responses);
    }

    #[tokio::test]
    async fn detect_openai_api_keeps_chat_when_completions_succeed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "ok"}}]
            })))
            .mount(&server)
            .await;

        let api = detect_openai_api(&reqwest::Client::new(), &server.uri(), "m", None)
            .await
            .unwrap();
        assert_eq!(api, OpenAiApiSurface::ChatCompletions);
    }

    #[tokio::test]
    async fn detect_openai_api_does_not_report_a_surface_after_auth_rejection() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let error = detect_openai_api(&reqwest::Client::new(), &server.uri(), "m", None)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("authentication rejected"));
    }

    #[tokio::test]
    async fn detect_openai_api_selects_responses_on_responses_only_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {
                    "message": "This model is only supported in v1/responses",
                    "code": "unsupported_api"
                }
            })))
            .mount(&server)
            .await;

        let api = detect_openai_api(&reqwest::Client::new(), &server.uri(), "gpt-5-codex", None)
            .await
            .unwrap();
        assert_eq!(api, OpenAiApiSurface::Responses);
    }

    #[tokio::test]
    async fn detect_openai_api_falls_through_to_responses_on_bare_chat_404() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "output": []
            })))
            .mount(&server)
            .await;

        let api = detect_openai_api(&reqwest::Client::new(), &server.uri(), "m", None)
            .await
            .unwrap();
        assert_eq!(api, OpenAiApiSurface::Responses);
    }

    // --- engine fingerprinting ---

    #[test]
    fn fingerprint_matches_shapes() {
        use FingerprintMarker::*;
        let props = serde_json::json!({"default_generation_settings": {}, "total_slots": 4});
        assert!(fingerprint_matches(
            &HasAnyKey(&["default_generation_settings", "model_path"]),
            &props
        ));
        let old_props = serde_json::json!({"model_path": "/models/x.gguf"});
        assert!(fingerprint_matches(
            &HasAnyKey(&["default_generation_settings", "model_path"]),
            &old_props
        ));
        let version = serde_json::json!({"version": "0.6.3"});
        assert!(fingerprint_matches(&HasKey("version"), &version));
        assert!(!fingerprint_matches(&HasKey("version"), &props));
        // llama-server /models: entries with load-state fields.
        let models = serde_json::json!({"data": [
            {"id": "a", "state": "loaded"},
            {"id": "b", "status": "unloaded"}
        ]});
        assert!(fingerprint_matches(&ModelsArrayWithState, &models));
        // OpenAI-shaped /models (no state fields) must NOT match.
        let openai = serde_json::json!({"data": [{"id": "a"}, {"id": "b"}]});
        assert!(!fingerprint_matches(&ModelsArrayWithState, &openai));
        // Empty array proves nothing.
        assert!(!fingerprint_matches(
            &ModelsArrayWithState,
            &serde_json::json!({"data": []})
        ));
    }

    #[tokio::test]
    async fn detect_engine_identifies_llamacpp_via_props() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_generation_settings": {}, "total_slots": 1
            })))
            .mount(&server)
            .await;
        let engine = detect_engine(
            &reqwest::Client::new(),
            &server.uri(),
            BackendKind::Openai,
            None,
        )
        .await;
        assert_eq!(engine, Some(Engine::LlamaCpp));
    }

    #[tokio::test]
    async fn detect_engine_identifies_vllm_via_version() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/version"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"version": "0.8.5.post1"})),
            )
            .mount(&server)
            .await;
        let engine = detect_engine(
            &reqwest::Client::new(),
            &server.uri(),
            BackendKind::Openai,
            None,
        )
        .await;
        assert_eq!(engine, Some(Engine::Vllm));
    }

    #[tokio::test]
    async fn detect_engine_old_llamacpp_falls_back_to_models_route() {
        // Older llama.cpp builds lack /props — the non-/v1 /models route with
        // load states is the terminal fingerprint in the fallback chain.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/version"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "qwen3-32b", "state": "loaded"}]
            })))
            .mount(&server)
            .await;
        let engine = detect_engine(
            &reqwest::Client::new(),
            &server.uri(),
            BackendKind::Openai,
            None,
        )
        .await;
        assert_eq!(engine, Some(Engine::LlamaCpp));
    }

    #[tokio::test]
    async fn detect_engine_unknown_for_generic_gateway() {
        // No fingerprint answers → None: the endpoint stays a fully usable
        // generic OpenAI backend, it merely reports no warmth.
        let server = MockServer::start().await;
        let engine = detect_engine(
            &reqwest::Client::new(),
            &server.uri(),
            BackendKind::Openai,
            None,
        )
        .await;
        assert_eq!(engine, None);
    }

    #[tokio::test]
    async fn detect_engine_short_circuits_for_ollama_kind() {
        // kind=Ollama needs zero HTTP — the /api/tags race already proved it.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let engine = detect_engine(
            &reqwest::Client::new(),
            &server.uri(),
            BackendKind::Ollama,
            None,
        )
        .await;
        assert_eq!(engine, Some(Engine::Ollama));
        server.verify().await;
    }

    // --- warm models ---

    #[test]
    fn parse_ollama_ps_reads_names_sizes_and_expiry() {
        // Fixture ported from the retired newt-tui parse_loaded_models and
        // newt-cli extract_ps copies — this parser is now the ONE home.
        let json = serde_json::json!({
            "models": [
                {
                    "name": "nemotron3:33b",
                    "size": 35_000_000_000u64,
                    "size_vram": 35_631_112_192u64,
                    "expires_at": "2026-06-06T12:00:00Z"
                },
                {"name": "tiny:1b"},
                {"x": 1}
            ]
        });
        let ps = parse_ollama_ps(&json);
        assert_eq!(ps.len(), 2, "nameless entries skipped");
        assert_eq!(ps[0].name, "nemotron3:33b");
        assert_eq!(ps[0].size_bytes, Some(35_000_000_000));
        assert_eq!(ps[0].size_vram_bytes, Some(35_631_112_192));
        assert!(ps[0].expires_at.is_some());
        assert_eq!(ps[1].name, "tiny:1b");
        assert_eq!(ps[1].size_bytes, None);
        assert!(parse_ollama_ps(&serde_json::json!({"models": []})).is_empty());
        assert!(parse_ollama_ps(&serde_json::json!(null)).is_empty());
    }

    #[tokio::test]
    async fn ollama_warm_models_reads_api_ps_with_bearer() {
        // The Ollama Cloud warmth contract: /api/ps carries the bearer when a
        // key is supplied.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{"name": "warm-a"}, {"name": "warm-b"}]
            })))
            .mount(&server)
            .await;
        let warm = OllamaApi
            .warm_models(&reqwest::Client::new(), &server.uri(), Some("tok"))
            .await;
        assert_eq!(warm, Some(vec!["warm-a".to_string(), "warm-b".to_string()]));
    }

    #[test]
    fn parse_llamacpp_models_warm_filters_by_load_state() {
        let json = serde_json::json!({"data": [
            {"id": "cold-model", "state": "unloaded"},
            {"id": "warm-model", "state": "loaded"},
            {"id": "other-warm", "status": "LOADED"}
        ]});
        assert_eq!(
            parse_llamacpp_models_warm(&json),
            Some(vec!["warm-model".to_string(), "other-warm".to_string()])
        );
    }

    #[test]
    fn parse_llamacpp_models_warm_none_when_no_state_fields() {
        // No entry carries a state field → capability absent (None), never a
        // guessed empty-warm claim.
        let json = serde_json::json!({"data": [{"id": "a"}, {"id": "b"}]});
        assert_eq!(parse_llamacpp_models_warm(&json), None);
    }

    #[test]
    fn parse_llamacpp_models_warm_reads_object_shaped_status() {
        // The live dgx1 llama-swap router reports `status` as an OBJECT
        // (`{"value":"loaded", "args":[…], "preset":"…"}`), not a bare string.
        // Regression: this build's warm model must be detected so a Managed
        // Shared backend can adopt-warm on it. Would return None before the fix.
        let json = serde_json::json!({"data": [
            {"id": "ornith-1.0-35b-q8", "status": {"value": "loaded", "args": ["--x"]}},
            {"id": "ornith_35b", "status": {"value": "unloaded"}}
        ]});
        assert_eq!(
            parse_llamacpp_models_warm(&json),
            Some(vec!["ornith-1.0-35b-q8".to_string()])
        );
    }

    #[tokio::test]
    async fn vllm_warm_models_is_the_served_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "resident-model"}]
            })))
            .mount(&server)
            .await;
        // /api/ps and /models must never be touched by the vLLM impl.
        Mock::given(method("GET"))
            .and(path("/api/ps"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let warm = VllmApi
            .warm_models(&reqwest::Client::new(), &server.uri(), None)
            .await;
        assert_eq!(warm, Some(vec!["resident-model".to_string()]));
        server.verify().await;
    }

    // --- adopt(): warm precedence ---

    #[test]
    fn multiplexer_prefers_warm_over_first_served() {
        let backend = BackendConfig {
            name: "b".into(),
            endpoint: "http://h:11434".into(),
            kind: Some(BackendKind::Ollama),
            ..Default::default()
        };
        let adoption = adopt(&backend, &served_warm(&["a", "b", "c"], &["c"]), None);
        assert_eq!(adoption.model.as_deref(), Some("c"));
        assert!(!adoption.requested_unavailable);
    }

    #[test]
    fn requested_and_declared_still_outrank_warm() {
        let declared = BackendConfig {
            name: "b".into(),
            endpoint: "http://h:11434".into(),
            model: Some("b".into()),
            kind: Some(BackendKind::Ollama),
            ..Default::default()
        };
        // Requested wins over everything.
        let adoption = adopt(&declared, &served_warm(&["a", "b", "c"], &["c"]), Some("a"));
        assert_eq!(adoption.model.as_deref(), Some("a"));
        // Declared wins over warm.
        let adoption = adopt(&declared, &served_warm(&["a", "b", "c"], &["c"]), None);
        assert_eq!(adoption.model.as_deref(), Some("b"));
    }

    #[test]
    fn stale_warm_entry_not_in_served_is_ignored() {
        let backend = BackendConfig {
            name: "b".into(),
            endpoint: "http://h:11434".into(),
            kind: Some(BackendKind::Ollama),
            ..Default::default()
        };
        // /api/ps race: the warm model was just removed from /api/tags.
        let adoption = adopt(&backend, &served_warm(&["a", "b"], &["gone"]), None);
        assert_eq!(
            adoption.model.as_deref(),
            Some("a"),
            "falls to first served"
        );
    }

    #[test]
    fn instance_adoption_unchanged_by_warm() {
        let backend = openai_backend(Some("requested"), Some(Serving::Instance));
        let adoption = adopt(&backend, &served_warm(&["served"], &["served"]), None);
        assert_eq!(adoption.model.as_deref(), Some("served"));
        assert!(adoption.requested_ignored);
    }

    // --- detect_endpoint: engine + warm population ---

    #[tokio::test]
    async fn detect_endpoint_populates_engine_and_warm() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "warm-model"}, {"id": "cold-model"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_generation_settings": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "warm-model", "state": "loaded"},
                    {"id": "cold-model", "state": "unloaded"}
                ]
            })))
            .mount(&server)
            .await;

        let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
            .await
            .unwrap();

        assert_eq!(result.kind, BackendKind::Openai);
        assert_eq!(result.engine, Some(Engine::LlamaCpp));
        assert_eq!(result.warm, vec!["warm-model"]);
    }

    #[tokio::test]
    async fn detect_endpoint_engine_and_warm_fail_soft() {
        // Fingerprints all 404 → engine None, warm empty, result still Ok.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "m1"}, {"id": "m2"}]
            })))
            .mount(&server)
            .await;

        let result = detect_endpoint(&reqwest::Client::new(), &server.uri(), None)
            .await
            .unwrap();

        assert_eq!(result.engine, None);
        assert!(result.warm.is_empty());
        assert_eq!(result.models, vec!["m1", "m2"]);
    }

    // --- AnthropicApi ---

    #[tokio::test]
    async fn anthropic_list_models_sends_required_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "claude-sonnet-4-5", "display_name": "Claude Sonnet 4.5"},
                    {"id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5"}
                ],
                "has_more": false
            })))
            .mount(&server)
            .await;

        let models = AnthropicApi
            .list_models(&reqwest::Client::new(), &server.uri(), Some("sk-ant-test"))
            .await
            .unwrap();
        assert_eq!(models, vec!["claude-sonnet-4-5", "claude-haiku-4-5"]);
    }

    #[tokio::test]
    async fn anthropic_list_models_follows_pagination_capped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(wiremock::matchers::query_param("after_id", "claude-a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "claude-b"}],
                "has_more": false,
                "last_id": "claude-b"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "claude-a"}],
                "has_more": true,
                "last_id": "claude-a"
            })))
            .mount(&server)
            .await;

        let models = AnthropicApi
            .list_models(&reqwest::Client::new(), &server.uri(), Some("k"))
            .await
            .unwrap();
        assert_eq!(models, vec!["claude-a", "claude-b"]);
    }

    #[tokio::test]
    async fn anthropic_context_window_reads_max_input_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "claude-sonnet-4-5", "max_input_tokens": 200_000},
                    {"id": "claude-legacy"}
                ],
                "has_more": false
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let window = AnthropicApi
            .context_window(&client, &server.uri(), "claude-sonnet-4-5", Some("k"))
            .await;
        assert_eq!(window, Some(200_000));
        // Absent field → None (caller keeps its default).
        let none = AnthropicApi
            .context_window(&client, &server.uri(), "claude-legacy", Some("k"))
            .await;
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn anthropic_list_models_401_is_a_typed_probe_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let err = AnthropicApi
            .list_models(&reqwest::Client::new(), &server.uri(), Some("bad-key"))
            .await
            .unwrap_err();
        let failure = ProbeFailure::from_error(err);
        assert!(failure.is_auth_failure(), "401 classifies as auth");
    }

    #[test]
    fn anthropic_serving_is_always_multiplexer() {
        assert_eq!(AnthropicApi.serving(1), Serving::Multiplexer);
        assert_eq!(AnthropicApi.serving(30), Serving::Multiplexer);
    }
}
