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
    // INERT-CODE-RATCHET: F11 WIRE: adoption computes warm-model and pin-conflict facts that the UI never reads.
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
#[path = "backend_probe_tests/mod.rs"]
mod tests;
