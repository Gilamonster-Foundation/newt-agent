//! **Hosted-provider PRESETS** — the pure-data roster behind `newt setup`'s
//! hosted-provider picker and the `newt providers` CLI.
//!
//! A preset is NOT a backend: it becomes a `backends/<name>.toml` drop-in
//! only when the operator selects it and settles a model + credential.
//! DISTINCT from `[[providers]]` / [`crate::config::ProviderConfig`], which
//! are subprocess provider *plugins* (executables speaking the
//! plugins-protocol JSON-RPC). See `docs/provider-presets.md`.
//!
//! ## Hermes Agent compatibility (the adoption feature)
//!
//! Field names deliberately mirror Hermes Agent's `ProviderProfile` 1:1
//! where newt has the concept, so a Hermes provider definition transposes
//! field-for-field. The drop-in loader accepts newt TOML **and** Hermes
//! YAML — including a full copied `~/.hermes/config.yaml` (detected by its
//! `custom_providers:` key), so Hermes users can copy configs straight in.
//! An inline `api_key` value in a copied Hermes config is NEVER loaded —
//! newt's no-inline-secrets law; the preset struct has no credential field
//! by construction (export the env var or paste at `newt setup`, which
//! stores keys encrypted).
//!
//! ## Deliberate exclusions (honesty over reach)
//!
//! - `api_mode = "bedrock_converse"` — newt has no Bedrock Converse wire.
//! - `auth_type` other than `api_key` (oauth_device_code / oauth_external /
//!   copilot / aws_sdk / external_process) — newt has no browser-OAuth,
//!   Copilot-token, or AWS-credential machinery. Such presets still PARSE
//!   (a copied Hermes file never fails to load) and show in pickers as
//!   "(unavailable: …)" rows with the reason — never silently dropped.
//! - Google Gemini's OpenAI-compat layer (`…/v1beta/openai/`): newt's
//!   OpenAI transports append `/v1/…` to the endpoint, so a base whose path
//!   is not `/v1`-shaped cannot be routed. Gemini models are reachable via
//!   the `openrouter` preset meanwhile.
//! - `default_headers` are carried for data fidelity but NOT yet sent
//!   (limitation L1 in the docs — threading custom headers through three
//!   transports is a deliberate follow-up).
//!
//! Three-Cs mechanics copied from [`crate::api_surface`] (LanguagePack):
//! built-ins in code as data, tolerant drop-in dir loader, merge-by-name
//! with later-layers-win, fixed precedence builtin < `~/.newt/providers/`
//! < `<workspace>/.newt/providers/`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{BackendConfig, BackendKind, Config, OpenAiApi};
use crate::router::Tier;

/// One hosted-provider preset. Fields mirror Hermes `ProviderProfile`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderPreset {
    /// Canonical id; a drop-in's filename stem overrides it (same rule as
    /// backend drop-ins).
    pub name: String,
    /// Alternative names for lookup.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Human label for pickers; falls back to `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where to create an API key — shown during setup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signup_url: Option<String>,
    /// API-key env vars in priority order. The wizard records the var that
    /// actually resolved (else `[0]` with an export instruction). Empty =
    /// the provider needs no key (e.g. LM Studio).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<String>,
    /// Hermes-style base URL (usually ending in `/v1`). Mapped onto
    /// `BackendConfig.endpoint` by [`endpoint_from_base_url`].
    pub base_url: String,
    /// Explicit model-catalog URL when it is not `{base_url}/models`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models_url: Option<String>,
    pub auth_type: AuthType,
    pub api_mode: ApiMode,
    /// Curated models offered when the live catalog can't be fetched;
    /// `[0]` is the default suggestion.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
    /// Carried for Hermes fidelity; NOT yet sent on the wire (L1).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub default_headers: BTreeMap<String, String>,
    /// Carried for Hermes fidelity; not yet consumed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_max_tokens: Option<u32>,
}

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 13:18 EDT | Date: 2026-08-12

impl ProviderPreset {
    /// The picker label: `display_name` else `name`.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

/// Hermes `auth_type` values. Every value PARSES; only `api_key` is
/// currently usable (see [`preset_support`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    #[default]
    ApiKey,
    OauthDeviceCode,
    OauthExternal,
    Copilot,
    AwsSdk,
    ExternalProcess,
}

impl AuthType {
    fn unsupported_reason(self) -> Option<&'static str> {
        match self {
            Self::ApiKey => None,
            Self::OauthDeviceCode => {
                Some("auth oauth_device_code needs a device-code OAuth flow newt does not have")
            }
            Self::OauthExternal => {
                Some("auth oauth_external needs an external sign-in flow newt does not have")
            }
            Self::Copilot => Some("auth copilot needs GitHub Copilot token refresh"),
            Self::AwsSdk => Some("auth aws_sdk needs the AWS credential chain"),
            Self::ExternalProcess => {
                Some("auth external_process runs a subprocess — use a [[providers]] plugin instead")
            }
        }
    }
}

/// Hermes `api_mode` values, plus a newt-only `ollama` mode so Ollama-wire
/// providers (Ollama Cloud) live in the same roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiMode {
    #[default]
    ChatCompletions,
    CodexResponses,
    AnthropicMessages,
    BedrockConverse,
    /// newt extension — Hermes has no Ollama-native mode.
    Ollama,
}

/// `api_mode` → newt wire. `None` = newt has no transport for it.
///
/// `chat_completions` deliberately leaves `api: None` (probe-at-connect,
/// which starts at chat/completions and adopts responses only when the
/// server demands it — the same shape today's wizard writes);
/// `codex_responses` pins `Some(Responses)`.
pub fn wire_for(mode: ApiMode) -> Option<(BackendKind, Option<OpenAiApi>)> {
    match mode {
        ApiMode::ChatCompletions => Some((BackendKind::Openai, None)),
        ApiMode::CodexResponses => Some((BackendKind::Openai, Some(OpenAiApi::Responses))),
        ApiMode::AnthropicMessages => Some((BackendKind::Anthropic, None)),
        ApiMode::Ollama => Some((BackendKind::Ollama, None)),
        ApiMode::BedrockConverse => None,
    }
}

/// Map a Hermes-style `base_url` onto a `BackendConfig.endpoint`.
///
/// newt's OpenAI transports append `/v1/…` themselves, so an OpenAI-mode
/// base must have an empty path or one ending in `/v1` — the `/v1` is
/// stripped (path prefixes before it survive: `…/api/v1` → `…/api`). A
/// non-`/v1`-shaped path (Gemini's `/v1beta/openai`) is a typed error.
/// Anthropic strips a trailing `/v1` too (its wire appends `/v1/messages`);
/// Ollama just trims the trailing slash.
pub fn endpoint_from_base_url(base_url: &str, mode: ApiMode) -> Result<String, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("base_url is empty".to_string());
    }
    match mode {
        ApiMode::Ollama => Ok(base.to_string()),
        ApiMode::AnthropicMessages => Ok(base
            .strip_suffix("/v1")
            .unwrap_or(base)
            .trim_end_matches('/')
            .to_string()),
        ApiMode::ChatCompletions | ApiMode::CodexResponses => {
            if let Some(stripped) = base.strip_suffix("/v1") {
                return Ok(stripped.trim_end_matches('/').to_string());
            }
            // A bare scheme://host[:port] with no path is fine as-is.
            let path_start = base.find("://").map(|i| i + 3).unwrap_or(0);
            match base[path_start..].find('/') {
                None => Ok(base.to_string()),
                Some(rel) => {
                    let path = &base[path_start + rel..];
                    Err(format!(
                        "base_url path `{path}` is not /v1-shaped; newt's openai transport \
                         appends /v1/… to the endpoint"
                    ))
                }
            }
        }
        ApiMode::BedrockConverse => {
            Err("api_mode bedrock_converse has no newt transport".to_string())
        }
    }
}

/// Refuse to transmit setup credentials to an implicit or remote plaintext
/// destination. HTTPS is accepted everywhere; HTTP is accepted only for a
/// loopback host used by local development servers.
pub fn validate_authenticated_url(target: &str) -> anyhow::Result<()> {
    let target = target.trim();
    if !target.contains("://") {
        anyhow::bail!(
            "authenticated setup needs an explicit URL including its scheme; use https:// so \
             the bearer token is not sent to inferred ports or plaintext transport"
        );
    }
    let url = reqwest::Url::parse(target)
        .map_err(|error| anyhow::anyhow!("invalid authenticated setup URL `{target}`: {error}"))?;
    if url.scheme() == "https" {
        return Ok(());
    }
    let loopback = url.host_str().is_some_and(|host| {
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() == "http" && loopback {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to send a bearer token to `{target}` over plaintext transport; use an https:// \
         URL (http:// is allowed only for loopback)"
    )
}

/// Whether a preset can be turned into a working backend today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetSupport {
    Supported {
        kind: BackendKind,
        api: Option<OpenAiApi>,
        endpoint: String,
    },
    Unsupported {
        reason: String,
    },
}

/// Auth + wire + base-URL shape, folded into one verdict. Unsupported
/// presets stay visible in pickers with the reason.
pub fn preset_support(p: &ProviderPreset) -> PresetSupport {
    if let Some(reason) = p.auth_type.unsupported_reason() {
        return PresetSupport::Unsupported {
            reason: reason.to_string(),
        };
    }
    let Some((kind, api)) = wire_for(p.api_mode) else {
        return PresetSupport::Unsupported {
            reason: "api_mode bedrock_converse has no newt transport".to_string(),
        };
    };
    match endpoint_from_base_url(&p.base_url, p.api_mode) {
        Ok(endpoint) => PresetSupport::Supported {
            kind,
            api,
            endpoint,
        },
        Err(reason) => PresetSupport::Unsupported { reason },
    }
}

/// Synthesize the `BackendConfig` a selected preset becomes. Mirrors the
/// wizard's `build_backend_pair` shape: all four tiers, multiplexer
/// serving, self-describing provenance. Model + credential references come
/// from the wizard's interactive steps.
pub fn backend_from_preset(
    p: &ProviderPreset,
    model: &str,
    api_key_env: Option<String>,
    api_key_file: Option<String>,
    setup_version: &str,
) -> Result<BackendConfig, String> {
    let PresetSupport::Supported {
        kind,
        api,
        endpoint,
    } = preset_support(p)
    else {
        let PresetSupport::Unsupported { reason } = preset_support(p) else {
            unreachable!()
        };
        return Err(reason);
    };
    Ok(BackendConfig {
        name: p.name.clone(),
        endpoint,
        // A hint, not authority — session start adopts served reality.
        model: Some(model.to_string()),
        tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
        kind: Some(kind),
        api,
        api_key_env,
        api_key_file,
        serving: Some(crate::config::Serving::Multiplexer),
        provenance: Some(crate::config::BackendProvenance {
            source: Some(format!("newt setup v{setup_version} (preset {})", p.name)),
            probed: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
            derived_serving: Some(true),
        }),
        ..Default::default()
    })
}

/// List a preset's models, honoring a custom `models_url` for OpenAI-shaped
/// presets (bearer GET, OpenAI `{"data":[{"id":…}]}` shape); otherwise the
/// wire's own catalog via [`crate::backend_probe::api_for`]. A `models_url`
/// on an ollama/anthropic preset is ignored with a warning (their catalogs
/// are not OpenAI-shaped).
pub async fn list_models_for_preset(
    client: &reqwest::Client,
    p: &ProviderPreset,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let PresetSupport::Supported { kind, endpoint, .. } = preset_support(p) else {
        anyhow::bail!("preset {} is not usable on this build", p.name);
    };
    let has_key = api_key.is_some_and(|key| !key.trim().is_empty());
    if let Some(models_url) = p.models_url.as_deref().filter(|u| !u.trim().is_empty()) {
        if kind == BackendKind::Openai {
            if has_key {
                validate_authenticated_url(models_url)?;
            }
            let mut req = client.get(models_url);
            if let Some(key) = api_key.filter(|k| !k.trim().is_empty()) {
                req = req.bearer_auth(key);
            }
            let resp = req.send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("HTTP {}", resp.status());
            }
            let json: serde_json::Value = resp.json().await?;
            let models = json["data"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("invalid model-list response: missing `data`"))?;
            return Ok(models
                .iter()
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect());
        }
        tracing::warn!(
            preset = %p.name,
            "models_url is only honored for openai-mode presets; using the wire's own catalog"
        );
    }
    if has_key {
        validate_authenticated_url(&endpoint)?;
    }
    crate::backend_probe::api_for(kind)
        .list_models(client, &endpoint, api_key)
        .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCheck {
    Accepted,
    Rejected(u16),
    Unverified(String),
}

/// Test a pasted key against the preset's selected model before setup writes.
pub async fn verify_key_for_preset(
    client: &reqwest::Client,
    p: &ProviderPreset,
    key: &str,
    model: &str,
) -> KeyCheck {
    let PresetSupport::Supported {
        kind,
        api,
        endpoint,
    } = preset_support(p)
    else {
        return KeyCheck::Unverified("preset is not usable on this build".into());
    };
    if let Err(error) = validate_authenticated_url(&endpoint) {
        return KeyCheck::Unverified(error.to_string());
    }
    match crate::backend_probe::verify_generation(client, kind, api, &endpoint, model, Some(key))
        .await
    {
        crate::backend_probe::GenerationCheck::Accepted(_) => KeyCheck::Accepted,
        crate::backend_probe::GenerationCheck::Rejected(status) => KeyCheck::Rejected(status),
        crate::backend_probe::GenerationCheck::Unverified(reason) => KeyCheck::Unverified(reason),
    }
}

// ---------------------------------------------------------------------------
// Builtin roster
// ---------------------------------------------------------------------------

/// One builtin-roster row — named fields keep the table readable (and the
/// constructor under clippy's argument limit honestly).
struct Row {
    name: &'static str,
    display: &'static str,
    blurb: &'static str,
    base_url: &'static str,
    mode: ApiMode,
    env: &'static [&'static str],
    models: &'static [&'static str],
    keys_at: &'static str,
}

fn preset(r: Row) -> ProviderPreset {
    ProviderPreset {
        name: r.name.to_string(),
        display_name: Some(r.display.to_string()),
        description: Some(r.blurb.to_string()),
        base_url: r.base_url.to_string(),
        api_mode: r.mode,
        env_vars: r.env.iter().map(|s| s.to_string()).collect(),
        fallback_models: r.models.iter().map(|s| s.to_string()).collect(),
        signup_url: (!r.keys_at.is_empty()).then(|| r.keys_at.to_string()),
        ..Default::default()
    }
}

/// The built-in roster — pure data, overridable per-name by drop-ins.
/// Every row speaks a wire newt has; excluded providers are listed in the
/// module docs with reasons.
pub fn builtin_presets() -> Vec<ProviderPreset> {
    vec![
        preset(Row {
            name: "openai",
            display: "OpenAI",
            blurb: "Hosted GPT family — OpenAI's own API",
            base_url: "https://api.openai.com/v1",
            mode: ApiMode::ChatCompletions,
            env: &["OPENAI_API_KEY"],
            models: &["gpt-5.2"],
            keys_at: "https://platform.openai.com/api-keys",
        }),
        preset(Row {
            name: "anthropic",
            display: "Anthropic",
            blurb: "Claude family — native /v1/messages wire",
            base_url: "https://api.anthropic.com",
            mode: ApiMode::AnthropicMessages,
            env: &["ANTHROPIC_API_KEY"],
            models: &["claude-sonnet-4-5"],
            keys_at: "https://console.anthropic.com/settings/keys",
        }),
        preset(Row {
            name: "ollama-cloud",
            display: "Ollama Cloud",
            blurb: "Hosted open-weights models on the Ollama wire",
            base_url: "https://ollama.com",
            mode: ApiMode::Ollama,
            env: &["OLLAMA_API_KEY"],
            models: &["gpt-oss:120b"],
            keys_at: "https://ollama.com/settings/keys",
        }),
        preset(Row {
            name: "openrouter",
            display: "OpenRouter",
            blurb: "One key, 200+ models across providers",
            base_url: "https://openrouter.ai/api/v1",
            mode: ApiMode::ChatCompletions,
            env: &["OPENROUTER_API_KEY"],
            models: &["openrouter/auto"],
            keys_at: "https://openrouter.ai/settings/keys",
        }),
        preset(Row {
            name: "nvidia-nim",
            display: "NVIDIA NIM",
            blurb: "NVIDIA-hosted inference; free developer credits",
            base_url: "https://integrate.api.nvidia.com/v1",
            mode: ApiMode::ChatCompletions,
            env: &["NVIDIA_API_KEY", "NIM_API_KEY"],
            models: &["meta/llama-3.3-70b-instruct"],
            keys_at: "https://build.nvidia.com",
        }),
        preset(Row {
            name: "huggingface",
            display: "Hugging Face",
            blurb: "HF inference router across many backends",
            base_url: "https://router.huggingface.co/v1",
            mode: ApiMode::ChatCompletions,
            env: &["HF_TOKEN", "HUGGING_FACE_HUB_TOKEN"],
            models: &["openai/gpt-oss-120b"],
            keys_at: "https://huggingface.co/settings/tokens",
        }),
        preset(Row {
            name: "moonshot",
            display: "Moonshot (Kimi)",
            blurb: "Long-context Kimi models",
            base_url: "https://api.moonshot.ai/v1",
            mode: ApiMode::ChatCompletions,
            env: &["MOONSHOT_API_KEY"],
            models: &["kimi-latest"],
            keys_at: "https://platform.moonshot.ai/console/api-keys",
        }),
        preset(Row {
            name: "lmstudio",
            display: "LM Studio (local)",
            blurb: "Local LM Studio server — no API key needed",
            base_url: "http://localhost:1234/v1",
            mode: ApiMode::ChatCompletions,
            env: &[],
            models: &[],
            keys_at: "https://lmstudio.ai",
        }),
        preset(Row {
            name: "venice",
            display: "Venice.ai",
            blurb: "Privacy-focused hosted inference",
            base_url: "https://api.venice.ai/api/v1",
            mode: ApiMode::ChatCompletions,
            env: &["VENICE_API_KEY"],
            models: &["llama-3.3-70b"],
            keys_at: "https://venice.ai/settings/api",
        }),
    ]
}

// ---------------------------------------------------------------------------
// Drop-in loading (TOML + Hermes YAML) and merging
// ---------------------------------------------------------------------------

/// One provider entry from a Hermes `config.yaml` — the field set Hermes'
/// own `_normalize_custom_provider_entry` accepts (verified against the
/// hermes-agent source), including its camelCase and alias spellings.
/// Shared with the `newt providers import-hermes` CLI. Unknown fields are
/// tolerated (Hermes carries many runtime knobs newt doesn't need).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HermesProviderEntry {
    /// Legacy list entries carry the name inline; map entries use the key.
    pub name: Option<String>,
    #[serde(alias = "url", alias = "baseUrl")]
    pub base_url: String,
    /// NEVER loaded into newt config — see [`expand_hermes_config`].
    #[serde(alias = "apiKey")]
    pub api_key: Option<String>,
    /// The env var holding the key — maps directly onto `env_vars`.
    #[serde(alias = "api_key_env", alias = "keyEnv", alias = "apiKeyEnv")]
    pub key_env: Option<String>,
    #[serde(alias = "apiMode")]
    pub api_mode: Option<ApiMode>,
    /// A single pinned model.
    #[serde(alias = "defaultModel", alias = "default_model")]
    pub model: Option<String>,
    /// A model whitelist (`models` in Hermes' schema; `allowed_models` in
    /// some documentation) — becomes `fallback_models`.
    #[serde(alias = "allowed_models")]
    pub models: Vec<String>,
    /// Carried onto `default_headers` (limitation L1 — not yet sent).
    pub extra_headers: BTreeMap<String, String>,
}

/// Both container shapes Hermes accepts: the legacy `custom_providers:`
/// LIST of entries, and the v12+ `providers:` keyed MAP (some docs also
/// show `custom_providers` as a map — accepted too).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum HermesProviderBlock {
    List(Vec<HermesProviderEntry>),
    Map(BTreeMap<String, HermesProviderEntry>),
}

impl Default for HermesProviderBlock {
    fn default() -> Self {
        Self::List(Vec::new())
    }
}

impl HermesProviderBlock {
    /// Normalize to (name, entry) pairs; a map key wins over an inline name,
    /// mirroring Hermes' own `providers_dict_to_custom_providers`.
    pub fn entries(&self) -> Vec<(String, HermesProviderEntry)> {
        match self {
            Self::List(list) => list
                .iter()
                .filter_map(|e| {
                    let name = e.name.as_deref()?.trim().to_string();
                    (!name.is_empty()).then(|| (name, e.clone()))
                })
                .collect(),
            Self::Map(map) => map.iter().map(|(k, e)| (k.clone(), e.clone())).collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::List(l) => l.is_empty(),
            Self::Map(m) => m.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct HermesConfigYaml {
    /// The legacy on-disk block.
    custom_providers: HermesProviderBlock,
    /// The v12+ keyed schema.
    providers: HermesProviderBlock,
}

/// Synthesize the conventional env-var name for a provider id
/// (`my-provider` → `MY_PROVIDER_API_KEY`).
pub fn synthesized_env_var(name: &str) -> String {
    let mut var: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    var.push_str("_API_KEY");
    var
}

/// Expand Hermes provider entries into presets. Hermes custom providers
/// default to the OpenAI-compatible wire. Inline `api_key` values are
/// reported back (so callers can print the export-or-paste instruction) and
/// NEVER placed anywhere in the output; a `key_env` reference maps straight
/// onto `env_vars`.
pub fn expand_hermes_config(
    entries: &[(String, HermesProviderEntry)],
) -> (
    Vec<ProviderPreset>,
    Vec<String /* names with inline keys */>,
) {
    let mut presets = Vec::new();
    let mut keyed = Vec::new();
    for (id, cp) in entries {
        if cp.base_url.trim().is_empty() {
            tracing::warn!(provider = %id, "hermes provider entry has no base_url — skipped");
            continue;
        }
        if cp.api_key.as_deref().is_some_and(|k| !k.trim().is_empty()) {
            keyed.push(id.clone());
        }
        let env_var = cp
            .key_env
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| synthesized_env_var(id));
        // `model` (a single pin) leads; `models` (whitelist) follows.
        let mut fallback_models: Vec<String> = Vec::new();
        if let Some(model) = cp.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            fallback_models.push(model.to_string());
        }
        for m in &cp.models {
            if !fallback_models.contains(m) {
                fallback_models.push(m.clone());
            }
        }
        presets.push(ProviderPreset {
            name: id.clone(),
            display_name: Some(id.clone()),
            description: Some("imported from Hermes providers config".to_string()),
            base_url: cp.base_url.trim().to_string(),
            api_mode: cp.api_mode.unwrap_or_default(),
            env_vars: vec![env_var],
            fallback_models,
            default_headers: cp.extra_headers.clone(),
            ..Default::default()
        });
    }
    (presets, keyed)
}

/// Parse one drop-in file body into presets. `stem` (the filename stem) is
/// authoritative for a single-preset file's `name`; a Hermes config.yaml
/// (multi-provider) keeps its map keys as names.
pub fn parse_preset_file(stem: &str, ext: &str, body: &str) -> Result<Vec<ProviderPreset>, String> {
    match ext {
        "toml" => {
            let mut p: ProviderPreset = toml::from_str(body).map_err(|e| e.to_string())?;
            if p.base_url.trim().is_empty() {
                // Also what a [[providers]] subprocess-plugin body looks like
                // from here (its `command` key is just an unknown field).
                return Err("no base_url — not a provider preset".to_string());
            }
            p.name = stem.to_string();
            Ok(vec![p])
        }
        "yaml" | "yml" => {
            // A full copied Hermes config.yaml first (detected by its
            // custom_providers key), else a bare ProviderPreset mapping.
            if let Ok(hermes) = serde_yaml::from_str::<HermesConfigYaml>(body) {
                if !hermes.custom_providers.is_empty() || !hermes.providers.is_empty() {
                    let mut entries = hermes.custom_providers.entries();
                    entries.extend(hermes.providers.entries());
                    let (presets, keyed) = expand_hermes_config(&entries);
                    for name in keyed {
                        tracing::warn!(
                            provider = %name,
                            "ignoring inline api_key from copied Hermes config — newt never \
                             stores plaintext keys; export {} or paste the key when `newt \
                             setup` asks (stored encrypted)",
                            synthesized_env_var(&name)
                        );
                    }
                    return Ok(presets);
                }
            }
            let mut p: ProviderPreset = serde_yaml::from_str(body).map_err(|e| e.to_string())?;
            if p.base_url.trim().is_empty() {
                return Err("no base_url (and no custom_providers block)".to_string());
            }
            if p.name.trim().is_empty() {
                p.name = stem.to_string();
            }
            Ok(vec![p])
        }
        other => Err(format!("unsupported extension `.{other}`")),
    }
}

/// Load `<dir>/*.{toml,yaml,yml}` as presets. Tolerant like every drop-in
/// loader: a malformed file is warned about and skipped, never fatal. A
/// TOML body that looks like a subprocess `[[providers]]` plugin (has a
/// `command` key) gets a targeted warning.
pub fn load_presets_from_dir(dir: &Path) -> Vec<ProviderPreset> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new(); // no providers dir — fine
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| matches!(x, "toml" | "yaml" | "yml"))
        })
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let (Some(stem), Some(ext)) = (
            path.file_stem().and_then(|s| s.to_str()),
            path.extension().and_then(|s| s.to_str()),
        ) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse_preset_file(stem, ext, &body) {
            Ok(presets) => out.extend(presets),
            Err(e) => {
                if ext == "toml"
                    && toml::from_str::<toml::Value>(&body)
                        .is_ok_and(|v| v.get("command").is_some())
                {
                    tracing::warn!(
                        path = %path.display(),
                        "this looks like a [[providers]] subprocess plugin, not a provider \
                         preset — presets have base_url, plugins have command"
                    );
                } else {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed provider preset");
                }
            }
        }
    }
    out
}

/// Merge preset layers by `name` — later layers win, first-seen order is
/// stable (mirror of `api_surface::merge_packs`).
pub fn merge_presets(layers: Vec<Vec<ProviderPreset>>) -> Vec<ProviderPreset> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: BTreeMap<String, ProviderPreset> = BTreeMap::new();
    for layer in layers {
        for preset in layer {
            if !by_name.contains_key(&preset.name) {
                order.push(preset.name.clone());
            }
            by_name.insert(preset.name.clone(), preset);
        }
    }
    order
        .into_iter()
        .filter_map(|name| by_name.remove(&name))
        .collect()
}

/// The resolved roster: builtin < `~/.newt/providers/` < project
/// `.newt/providers/`.
pub fn resolve_presets(workspace: Option<&Path>) -> Vec<ProviderPreset> {
    let mut layers = vec![builtin_presets()];
    if let Some(dir) = Config::user_config_dir() {
        layers.push(load_presets_from_dir(&dir.join("providers")));
    }
    if let Some(ws) = workspace {
        layers.push(load_presets_from_dir(&ws.join(".newt").join("providers")));
    } else if let Some(proj) = Config::project_config_path() {
        if let Some(parent) = proj.parent() {
            layers.push(load_presets_from_dir(&parent.join("providers")));
        }
    }
    merge_presets(layers)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- roster invariants ---

    #[test]
    fn builtin_roster_is_coherent() {
        let roster = builtin_presets();
        let mut names: Vec<&str> = roster.iter().map(|p| p.name.as_str()).collect();
        let len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), len, "unique names");
        for p in &roster {
            match preset_support(p) {
                PresetSupport::Supported { endpoint, .. } => {
                    assert!(
                        endpoint.starts_with("http"),
                        "{}: endpoint {endpoint}",
                        p.name
                    );
                    assert!(!endpoint.ends_with('/'), "{}: no trailing slash", p.name);
                }
                PresetSupport::Unsupported { reason } => {
                    panic!("builtin {} must be supported: {reason}", p.name)
                }
            }
            // Every builtin except lmstudio names at least one key var.
            if p.name != "lmstudio" {
                assert!(!p.env_vars.is_empty(), "{} has env_vars", p.name);
            }
        }
        // The original three keep their exact endpoints (compat with the
        // pre-roster wizard output).
        let by_name = |n: &str| roster.iter().find(|p| p.name == n).unwrap();
        assert!(matches!(
            preset_support(by_name("openai")),
            PresetSupport::Supported { ref endpoint, .. } if endpoint == "https://api.openai.com"
        ));
        assert!(matches!(
            preset_support(by_name("anthropic")),
            PresetSupport::Supported { kind: BackendKind::Anthropic, ref endpoint, .. }
                if endpoint == "https://api.anthropic.com"
        ));
        assert!(matches!(
            preset_support(by_name("ollama-cloud")),
            PresetSupport::Supported { kind: BackendKind::Ollama, ref endpoint, .. }
                if endpoint == "https://ollama.com"
        ));
    }

    // --- endpoint mapping ---

    #[test]
    fn endpoint_from_base_url_table() {
        use ApiMode::*;
        // /v1 strip, prefix survives, trailing slash tolerated.
        assert_eq!(
            endpoint_from_base_url("https://api.acme.com/v1", ChatCompletions).unwrap(),
            "https://api.acme.com"
        );
        assert_eq!(
            endpoint_from_base_url("https://openrouter.ai/api/v1/", ChatCompletions).unwrap(),
            "https://openrouter.ai/api"
        );
        // Bare host is fine.
        assert_eq!(
            endpoint_from_base_url("http://localhost:8080", CodexResponses).unwrap(),
            "http://localhost:8080"
        );
        // Non-/v1 path → typed error (the Gemini case).
        let err = endpoint_from_base_url(
            "https://generativelanguage.googleapis.com/v1beta/openai/",
            ChatCompletions,
        )
        .unwrap_err();
        assert!(err.contains("not /v1-shaped"), "{err}");
        // Anthropic strips /v1 too; ollama passes through.
        assert_eq!(
            endpoint_from_base_url("https://api.anthropic.com/v1", AnthropicMessages).unwrap(),
            "https://api.anthropic.com"
        );
        assert_eq!(
            endpoint_from_base_url("https://ollama.com/", Ollama).unwrap(),
            "https://ollama.com"
        );
        assert!(endpoint_from_base_url("", ChatCompletions).is_err());
        assert!(endpoint_from_base_url("https://x.com/v1", BedrockConverse).is_err());
    }

    // --- wire + support matrices ---

    #[test]
    fn wire_for_full_matrix() {
        assert_eq!(
            wire_for(ApiMode::ChatCompletions),
            Some((BackendKind::Openai, None))
        );
        assert_eq!(
            wire_for(ApiMode::CodexResponses),
            Some((BackendKind::Openai, Some(OpenAiApi::Responses)))
        );
        assert_eq!(
            wire_for(ApiMode::AnthropicMessages),
            Some((BackendKind::Anthropic, None))
        );
        assert_eq!(wire_for(ApiMode::Ollama), Some((BackendKind::Ollama, None)));
        assert_eq!(wire_for(ApiMode::BedrockConverse), None);
    }

    #[test]
    fn unsupported_auth_types_stay_visible_with_reasons() {
        for (auth, needle) in [
            (AuthType::OauthDeviceCode, "oauth_device_code"),
            (AuthType::OauthExternal, "oauth_external"),
            (AuthType::Copilot, "copilot"),
            (AuthType::AwsSdk, "aws_sdk"),
            (AuthType::ExternalProcess, "external_process"),
        ] {
            let p = ProviderPreset {
                name: "x".into(),
                base_url: "https://api.x.com/v1".into(),
                auth_type: auth,
                ..Default::default()
            };
            match preset_support(&p) {
                PresetSupport::Unsupported { reason } => {
                    assert!(reason.contains(needle), "{reason}");
                }
                other => panic!("{auth:?} must be unsupported, got {other:?}"),
            }
        }
    }

    // --- backend synthesis ---

    #[test]
    fn backend_from_preset_mirrors_the_wizard_shape() {
        let p = ProviderPreset {
            name: "acme".into(),
            base_url: "https://api.acme.com/v1".into(),
            api_mode: ApiMode::CodexResponses,
            ..Default::default()
        };
        let b = backend_from_preset(&p, "acme-large", Some("ACME_API_KEY".into()), None, "9.9.9")
            .unwrap();
        assert_eq!(b.name, "acme");
        assert_eq!(b.endpoint, "https://api.acme.com");
        assert_eq!(b.effective_model(), Some("acme-large"));
        assert_eq!(b.kind, Some(BackendKind::Openai));
        assert_eq!(
            b.api,
            Some(OpenAiApi::Responses),
            "codex_responses pins api"
        );
        assert_eq!(b.tiers.len(), 4);
        assert_eq!(b.serving, Some(crate::config::Serving::Multiplexer));
        assert_eq!(b.api_key_env.as_deref(), Some("ACME_API_KEY"));
        let prov = b.provenance.unwrap();
        assert!(prov.source.unwrap().contains("preset acme"));

        // chat_completions leaves api unset (probe-at-connect).
        let p2 = ProviderPreset {
            api_mode: ApiMode::ChatCompletions,
            ..p.clone()
        };
        let b2 = backend_from_preset(&p2, "m", None, None, "9.9.9").unwrap();
        assert_eq!(b2.api, None);

        // Unsupported → Err with the reason.
        let p3 = ProviderPreset {
            auth_type: AuthType::Copilot,
            ..p
        };
        assert!(backend_from_preset(&p3, "m", None, None, "9.9.9")
            .unwrap_err()
            .contains("copilot"));
    }

    // --- serde tolerance (the Hermes-transposition contract) ---

    #[test]
    fn hermes_transposed_toml_with_unknown_fields_loads() {
        let body = r#"
name = "ignored-stem-wins"
display_name = "Acme Inference"
description = "Acme — OpenAI-compatible direct API"
signup_url = "https://acme.example.com/keys"
env_vars = ["ACME_API_KEY", "ACME_BASE_URL"]
base_url = "https://api.acme.example.com/v1"
auth_type = "api_key"
api_mode = "chat_completions"
fallback_models = ["acme-large-v3", "acme-small-fast"]
fixed_temperature = 0.6
default_aux_model = "acme-small-fast"
"#;
        let presets = parse_preset_file("acme", "toml", body).unwrap();
        assert_eq!(presets.len(), 1);
        let p = &presets[0];
        assert_eq!(p.name, "acme", "filename stem is authoritative");
        assert_eq!(p.env_vars, vec!["ACME_API_KEY", "ACME_BASE_URL"]);
        assert_eq!(p.fallback_models[0], "acme-large-v3");
        assert!(matches!(preset_support(p), PresetSupport::Supported { .. }));
    }

    #[test]
    fn bare_yaml_preset_loads_with_stem_name() {
        let body = r#"
display_name: Acme YAML
base_url: https://api.acme.example.com/v1
env_vars: [ACME_API_KEY]
fallback_models: [acme-large-v3]
"#;
        let presets = parse_preset_file("acme-y", "yaml", body).unwrap();
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].name, "acme-y");
        assert_eq!(presets[0].label(), "Acme YAML");
    }

    #[test]
    fn copied_hermes_config_yaml_expands_custom_providers_without_keys() {
        let body = r#"
model:
  default: "hermes-3-llama-3.1-8b"
  provider: "custom"
custom_providers:
  custom:
    base_url: "http://localhost:8000/v1"
    api_key: "sk-live-SHOULD-NEVER-SURFACE"
    allowed_models:
      - "hermes-3-llama-3.1-8b"
      - "meta-llama/Llama-3-8b-Instruct"
  other:
    base_url: "https://api.other.ai/v1"
"#;
        let presets = parse_preset_file("config", "yaml", body).unwrap();
        assert_eq!(presets.len(), 2);
        let custom = presets.iter().find(|p| p.name == "custom").unwrap();
        assert_eq!(custom.base_url, "http://localhost:8000/v1");
        assert_eq!(custom.env_vars, vec!["CUSTOM_API_KEY"]);
        assert_eq!(
            custom.fallback_models,
            vec!["hermes-3-llama-3.1-8b", "meta-llama/Llama-3-8b-Instruct"]
        );
        // The key never lands anywhere in the parsed output.
        let dumped = format!("{presets:?}");
        assert!(!dumped.contains("SHOULD-NEVER-SURFACE"));
    }

    #[test]
    fn hermes_legacy_list_and_v12_map_shapes_both_expand() {
        // Verified against hermes-agent's own config normalizer: legacy
        // `custom_providers` is a LIST of entries (inline `name`), v12+
        // `providers` is a keyed MAP with `key_env`/`extra_headers`; both
        // (plus camelCase aliases) must expand.
        let body = r#"
custom_providers:
  - name: "legacy-one"
    base_url: "http://box:8000/v1"
    key_env: "LEGACY_ONE_KEY"
    model: "pinned-model"
    models: ["pinned-model", "second-model"]
providers:
  my-proxy:
    baseUrl: "https://llm.internal.example.com/v1"
    apiKeyEnv: "MY_PROXY_API_KEY"
    extra_headers:
      CF-Access-Client-Id: "xxxx.access"
"#;
        let presets = parse_preset_file("config", "yaml", body).unwrap();
        assert_eq!(presets.len(), 2);
        let legacy = presets.iter().find(|p| p.name == "legacy-one").unwrap();
        assert_eq!(legacy.env_vars, vec!["LEGACY_ONE_KEY"], "key_env maps");
        assert_eq!(
            legacy.fallback_models,
            vec!["pinned-model", "second-model"],
            "model pin leads, whitelist follows, deduped"
        );
        let proxy = presets.iter().find(|p| p.name == "my-proxy").unwrap();
        assert_eq!(proxy.base_url, "https://llm.internal.example.com/v1");
        assert_eq!(proxy.env_vars, vec!["MY_PROXY_API_KEY"], "camelCase alias");
        assert_eq!(
            proxy.default_headers.get("CF-Access-Client-Id").unwrap(),
            "xxxx.access",
            "extra_headers carried (L1)"
        );
    }

    #[test]
    fn malformed_bodies_error_not_panic() {
        assert!(parse_preset_file("x", "toml", "{{{{").is_err());
        assert!(parse_preset_file("x", "yaml", ": : :").is_err());
        assert!(parse_preset_file("x", "yaml", "just: scalar-noise").is_err());
        assert!(parse_preset_file("x", "json", "{}").is_err());
    }

    #[test]
    fn synthesized_env_var_slugs() {
        assert_eq!(synthesized_env_var("custom"), "CUSTOM_API_KEY");
        assert_eq!(
            synthesized_env_var("my-provider.2"),
            "MY_PROVIDER_2_API_KEY"
        );
    }

    // --- merge / precedence ---

    #[test]
    fn merge_presets_later_layers_win_order_stable() {
        let a = ProviderPreset {
            name: "a".into(),
            base_url: "https://a/v1".into(),
            ..Default::default()
        };
        let b = ProviderPreset {
            name: "b".into(),
            base_url: "https://b/v1".into(),
            ..Default::default()
        };
        let a2 = ProviderPreset {
            name: "a".into(),
            base_url: "https://a-OVERRIDE/v1".into(),
            ..Default::default()
        };
        let merged = merge_presets(vec![vec![a, b], vec![a2]]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "a", "first-seen order stable");
        assert_eq!(merged[0].base_url, "https://a-OVERRIDE/v1", "later wins");
        assert_eq!(merged[1].name, "b");
    }

    // --- loader (real fs — grounds the parse fns above) ---

    #[serial_test::serial(real_fs)]
    #[test]
    fn dropin_dir_loads_toml_and_yaml_and_skips_garbage() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("acme.toml"),
            "base_url = \"https://api.acme.com/v1\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("hermes-copy.yaml"),
            "custom_providers:\n  copied:\n    base_url: \"http://box:8000/v1\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("broken.toml"), "{{{{").unwrap();
        // A subprocess-plugin body: targeted warn, skipped.
        std::fs::write(
            dir.path().join("plugin.toml"),
            "name = \"p\"\ncommand = \"/usr/bin/foo\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "not a preset").unwrap();

        let presets = load_presets_from_dir(dir.path());
        let names: Vec<&str> = presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["acme", "copied"]);
    }

    // --- list_models_for_preset (wiremock) ---

    #[tokio::test]
    async fn custom_models_url_is_honored_with_bearer() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/catalog/models"))
            .and(header("authorization", "Bearer sk-x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "weird-catalog-model"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let p = ProviderPreset {
            name: "weird".into(),
            base_url: format!("{}/v1", server.uri()),
            models_url: Some(format!("{}/catalog/models", server.uri())),
            ..Default::default()
        };
        let models = list_models_for_preset(&reqwest::Client::new(), &p, Some("sk-x"))
            .await
            .unwrap();
        assert_eq!(models, vec!["weird-catalog-model"]);
        server.verify().await;
    }

    #[tokio::test]
    async fn custom_models_url_refuses_a_bearer_over_remote_plaintext() {
        let p = ProviderPreset {
            name: "unsafe-catalog".into(),
            base_url: "https://inference.example.test/v1".into(),
            models_url: Some("http://192.0.2.10/catalog/models".into()),
            ..Default::default()
        };
        let error = list_models_for_preset(&reqwest::Client::new(), &p, Some("sk-secret"))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("refusing to send a bearer token"), "{error}");
    }

    #[test]
    fn authenticated_urls_require_https_except_for_loopback() {
        assert!(validate_authenticated_url("host.example:8000").is_err());
        assert!(validate_authenticated_url("http://host.example:8000").is_err());
        assert!(validate_authenticated_url("https://host.example:8000").is_ok());
        assert!(validate_authenticated_url("http://127.0.0.1:8000").is_ok());
        assert!(validate_authenticated_url("http://[::1]:8000").is_ok());
    }

    #[tokio::test]
    async fn default_catalog_goes_through_the_wire_api() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "normal-model"}]
            })))
            .mount(&server)
            .await;
        let p = ProviderPreset {
            name: "n".into(),
            base_url: format!("{}/v1", server.uri()),
            ..Default::default()
        };
        let models = list_models_for_preset(&reqwest::Client::new(), &p, None)
            .await
            .unwrap();
        assert_eq!(models, vec!["normal-model"]);
    }

    // --- verify_key_for_preset (wiremock) ---

    #[tokio::test]
    async fn verify_key_ollama_wire_uses_one_token_chat_and_classifies_auth() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        // ollama.com serves /api/tags to anyone and gates only generation, so
        // the check MUST exercise /api/chat — a good key passes, a bad one 401s.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(header("authorization", "Bearer good-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "hi"}, "done": true
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "Unauthorized"
            })))
            .mount(&server)
            .await;
        let p = ProviderPreset {
            name: "ol".into(),
            base_url: server.uri(),
            api_mode: ApiMode::Ollama,
            ..Default::default()
        };
        let client = reqwest::Client::new();
        assert_eq!(
            verify_key_for_preset(&client, &p, "good-key", "m").await,
            KeyCheck::Accepted
        );
        assert_eq!(
            verify_key_for_preset(&client, &p, "typo-key", "m").await,
            KeyCheck::Rejected(401)
        );
        // The chat body is a 1-token probe against the chosen model.
        let reqs = server.received_requests().await.expect("journal");
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["model"], serde_json::json!("m"));
        assert_eq!(body["options"]["num_predict"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn verify_key_openai_wire_uses_selected_model_chat() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-good"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "hi"}}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let p = ProviderPreset {
            name: "oa".into(),
            base_url: format!("{}/v1", server.uri()),
            ..Default::default()
        };
        let client = reqwest::Client::new();
        assert_eq!(
            verify_key_for_preset(&client, &p, "sk-good", "m").await,
            KeyCheck::Accepted
        );
        assert_eq!(
            verify_key_for_preset(&client, &p, "sk-bad", "m").await,
            KeyCheck::Rejected(403)
        );
        let requests = server.received_requests().await.expect("journal");
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], serde_json::json!("m"));
        assert_eq!(body["max_tokens"], serde_json::json!(8));
    }

    #[tokio::test]
    async fn verify_chat_does_not_treat_a_public_model_catalog_as_auth() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "listed-but-gated"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        assert_eq!(
            crate::backend_probe::verify_generation(
                &reqwest::Client::new(),
                BackendKind::Openai,
                None,
                &server.uri(),
                "listed-but-gated",
                None,
            )
            .await,
            crate::backend_probe::GenerationCheck::Rejected(401)
        );
    }

    #[tokio::test]
    async fn verify_key_unreachable_is_unverified_not_rejected() {
        // A down endpoint is distinct from a rejected key, but setup still
        // fails closed because no generation was verified.
        let p = ProviderPreset {
            name: "down".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            ..Default::default()
        };
        match verify_key_for_preset(&reqwest::Client::new(), &p, "k", "m").await {
            KeyCheck::Unverified(_) => {}
            other => panic!("expected Unverified, got {other:?}"),
        }
    }
}
