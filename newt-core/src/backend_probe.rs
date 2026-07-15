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

use crate::config::{BackendConfig, BackendKind, Serving};

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
}

/// The `BackendApi` for a wire kind — a `&'static` ZST, so no allocation.
pub fn api_for(kind: BackendKind) -> &'static dyn BackendApi {
    match kind {
        BackendKind::Ollama => &OllamaApi,
        BackendKind::Openai => &OpenAiApi,
        BackendKind::Embedded => &EmbeddedApi,
    }
}

/// Ollama: `/api/tags` to list, `/api/show` for the window, always a
/// time-multiplexer (many models loaded on demand), no bearer auth.
pub struct OllamaApi;
/// OpenAI-compatible (vLLM / gateways): `/v1/models` to list + read
/// `max_model_len`, bearer auth, serving derived from the served count.
pub struct OpenAiApi;
/// The in-process GGUF engine: no HTTP; runs exactly one model.
pub struct EmbeddedApi;

#[async_trait::async_trait]
impl BackendApi for OllamaApi {
    async fn list_models(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        _api_key: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("HTTP {}", resp.status());
        }
        let json: serde_json::Value = resp.json().await?;
        Ok(json["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn context_window(
        &self,
        client: &reqwest::Client,
        endpoint: &str,
        model: &str,
        _api_key: Option<&str>,
    ) -> Option<u32> {
        let url = format!("{}/api/show", endpoint.trim_end_matches('/'));
        let resp = client
            .post(&url)
            .json(&serde_json::json!({ "name": model }))
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
        Ok(json["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
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
        anyhow::bail!("HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
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
    let window_of = |e: &serde_json::Value| e["max_model_len"].as_u64().map(|v| v as u32);
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
}

/// The pure adoption rule. `served` is what the probe saw; `requested` is the
/// session's explicit ask (e.g. `/model X` / env override), which outranks the
/// file's declared model on a multiplexer and is overridden (flagged) on an
/// instance.
pub fn adopt(backend: &BackendConfig, served: &Served, requested: Option<&str>) -> Adoption {
    let serving = backend
        .serving
        .unwrap_or_else(|| api_for(backend.kind).serving(served.models.len()));
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
            let model = requested
                .filter(|_| requested_ok == Some(true))
                .map(str::to_string)
                .or_else(|| backend.effective_model().map(str::to_string))
                .or_else(|| served.models.first().cloned());
            Adoption {
                model,
                serving,
                requested_ignored: false,
                requested_unavailable,
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
    use crate::config::BackendKind;

    fn openai_backend(model: Option<&str>, serving: Option<Serving>) -> BackendConfig {
        BackendConfig {
            name: "b".into(),
            endpoint: "http://h:8000".into(),
            model: model.map(str::to_string),
            kind: BackendKind::Openai,
            serving,
            ..Default::default()
        }
    }

    fn served(models: &[&str]) -> Served {
        Served {
            models: models.iter().map(|m| m.to_string()).collect(),
        }
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
        assert_eq!(parse_openai_models_window(&json, "other"), Some(262144), "single-instance fallback");
        assert_eq!(parse_openai_models_window(&serde_json::json!({"data":[{"id":"m"}]}), "m"), None);
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
            kind: BackendKind::Ollama,
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
            kind: BackendKind::Ollama,
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
}
