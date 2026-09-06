//! Backend declarations and their composition from operator, probe, and CLI layers.
//!
//! Whole-config loading, publication, and winner selection remain in the parent.
//! Assembly shares that selector for unnamed CLI edits so targeting and routing
//! continue to agree; `ResolvedConfig` keeps the aggregate config and receipts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{NewtError, Result};
use crate::router::Tier;

use super::dropin::{
    classify_untagged_dropin, disk_record_tag, parse_probe_record, DropinOwner, ProbeObservation,
};
use super::{
    backend_is_routable, expand_tilde, pin_requested_selection, select_backend_slot, Config,
    SlotSelection,
};

/// The wire protocol an inference backend speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Ollama's native `POST /api/chat` API (the historical default).
    #[default]
    Ollama,
    /// An OpenAI-compatible HTTP API (`POST /v1/chat/completions`,
    /// `GET /v1/models`): vLLM, llama.cpp's server, or any hosted
    /// OpenAI-compatible endpoint. Optionally authenticated with a
    /// bearer token (see [`BackendConfig::api_key_file`] /
    /// [`BackendConfig::api_key_env`]).
    #[serde(alias = "vllm", alias = "openai-compatible")]
    Openai,
    /// An **in-process** inference backend — no HTTP, no external server. Loads a
    /// small quantized (GGUF) model and runs it in-tree (Metal-accelerated on
    /// Apple Silicon). Opt-in behind the `embedded` cargo feature (default-off);
    /// when the feature is absent, selecting it is a clear build-time-off error,
    /// never a silent fallback. Intended for the summarizer + small auxiliary
    /// calls so they never contend with the primary model (#639).
    Embedded,
    /// Anthropic's native Messages API (`POST /v1/messages`, `GET /v1/models`),
    /// authenticated with `x-api-key` + `anthropic-version` headers (NOT a
    /// bearer token). A genuinely distinct wire: top-level `system`, required
    /// `max_tokens`, content-block responses. Unlike llama.cpp/vLLM (which
    /// share the OpenAI wire and are told apart by [`Engine`] metadata),
    /// Anthropic earns its own kind because the protocol differs.
    #[serde(alias = "claude")]
    Anthropic,
}

impl BackendKind {
    /// Short human label for the wire protocol — shown in the ready preamble and
    /// the `/backends` list. Note newt models the *protocol*, so vLLM, llama.cpp,
    /// and hosted OpenAI all read as `openai` (vLLM has no distinct wire form).
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Openai => "openai",
            Self::Embedded => "embedded",
            Self::Anthropic => "anthropic",
        }
    }
}

/// The inference ENGINE behind an endpoint — pure metadata, orthogonal to
/// [`BackendKind`] (the wire protocol). llama.cpp's server and vLLM both
/// speak the OpenAI wire, so `kind` alone cannot tell them apart; a
/// fingerprint probe (`backend_probe::detect_engine`) can. The engine never
/// gates a transport — it drives only which warm-model probe applies, display
/// labels, and future model-card hints. `None` = undetected/unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// Ollama (`/api/version`, `/api/tags`, `/api/ps`).
    Ollama,
    /// llama.cpp's `llama-server` (`/props`, non-`/v1` `/models` with load
    /// states).
    #[serde(alias = "llama-cpp", alias = "llama.cpp")]
    LlamaCpp,
    /// vLLM (`/version`, single served model per instance).
    Vllm,
}

impl Engine {
    /// Short human label — shown beside probe results and in `/backends`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::LlamaCpp => "llama.cpp",
            Self::Vllm => "vllm",
        }
    }
}

/// Which OpenAI HTTP surface a `kind = "openai"` backend speaks.
///
/// `chat_completions` (the default) is the classic `POST /v1/chat/completions`.
/// `responses` is the newer `POST /v1/responses` — required by models that
/// OpenAI serves *only* there (e.g. `gpt-5-codex`, which 404s on
/// chat/completions with "only supported in v1/responses"). Ignored for
/// `kind = "ollama"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApi {
    /// `POST /v1/chat/completions` (the historical default).
    #[default]
    #[serde(alias = "chat", alias = "completions")]
    ChatCompletions,
    /// `POST /v1/responses` (the newer Responses API).
    Responses,
}

impl OpenAiApi {
    /// Short human label for the HTTP surface.
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }
}

/// A single inference backend entry.
///
/// Two ways to define one: an inline `[[backends]]` array element in
/// `config.toml`, or a per-file drop-in `~/.newt/backends/<name>.toml` (the
/// How a backend SERVES models — orthogonal to [`BackendKind`] (the wire
/// protocol). The out-of-the-box epic's (#1126) second axis:
///
/// - **Multiplexer** (Ollama; also an OpenAI-compatible gateway fronting many
///   models): many models, the client picks per request (`/model` swaps
///   freely), capabilities are learned **per model**.
/// - **Instance** (vLLM; the embedded engine): bound to ONE base model at
///   startup — `/v1/models` exists only to *declare* it. newt ADOPTS the
///   served model; capabilities attach to the **backend**; `/model` reports
///   "fixed — restart the server or `/backends` to switch".
///
/// Usually left unset in the file and DERIVED by probing (see
/// [`derive_serving`]), then cached back as provenance by `newt setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Serving {
    Multiplexer,
    Instance,
}

/// Whether newt actively **tends** this backend's host — a shared, model-
/// swapping box (e.g. a llama.cpp router) — rather than merely consuming a
/// dedicated endpoint. Orthogonal to [`BackendKind`] (the wire) and
/// [`Serving`] (how the box serves). See ADR `docs/decisions/managed_backend.md`.
///
/// - **`Shared`** — cooperative guest: the box may serve other consumers
///   (including other newt-agents), so the default is to **adopt whatever model
///   is warm** rather than force a swap (see [`crate::backend_probe::adopt`]).
///   This is the clash-avoidance primitive — two agents on one box don't thrash
///   the single-model swap.
/// - **`Dedicated`** — "I own this box": newt may force its configured model
///   (force-load + keep-warm are later slices). No adopt-warm.
///
/// Unset on a backend = an ordinary consumed endpoint (no swap-awareness).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedMode {
    Shared,
    Dedicated,
}

/// Derive the serving axis from what a probe saw: the wire kind plus how many
/// models the endpoint reported. Pure — Phase B's probe/adopt calls this; kept
/// here so the rule lives beside the type. `served_count` = models listed by
/// `/api/tags` (ollama) or `/v1/models` (openai).
pub fn derive_serving(kind: BackendKind, served_count: usize) -> Serving {
    match kind {
        // Ollama loads models on demand — always a multiplexer, even if only
        // one model happens to be pulled today.
        BackendKind::Ollama => Serving::Multiplexer,
        // A vLLM instance declares exactly one model; an OpenAI-compatible
        // gateway fronting a fleet lists many.
        BackendKind::Openai => {
            if served_count == 1 {
                Serving::Instance
            } else {
                Serving::Multiplexer
            }
        }
        // The in-process engine runs one GGUF.
        BackendKind::Embedded => Serving::Instance,
        // A hosted API fronting the whole Claude family — always many models.
        BackendKind::Anthropic => Serving::Multiplexer,
    }
}

/// Explicit ownership of a backend drop-in record — who may rewrite the file
/// and how the loader merges it. This is the discriminator the loader
/// BRANCHES on; [`BackendProvenance`] below stays purely informational.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RecordTag {
    /// Operator-owned: the loader replaces the same-name backend WHOLESALE,
    /// so omissions deliberately clear/rebind. The runtime writeback never
    /// touches such a file. Untagged files are operator-owned too — except
    /// the legacy ambiguity the backend assembly's drop-in merge refuses to guess
    /// about (see there).
    OperatorV1,
    /// Probe-owned overlay: associated by exact name + endpoint, whitelist-
    /// merged (observed `kind`/`api`/`serving`, plus `model` only for an
    /// Instance observation) onto the same-name backend. Never touches card,
    /// capability, auth, tiers, managed, host, or operator provenance.
    ProbeV1,
}

/// Where a backend file came from — written by `newt setup`, hand-authored,
/// or probe-derived. Pure data; nothing branches on it (ownership branches
/// on [`RecordTag`]). Makes a generated file self-describing and lets
/// `doctor` show declared-vs-derived drift.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackendProvenance {
    /// Who wrote the file (e.g. `newt setup v0.7.3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// When the endpoint was last probed (ISO 8601 date or datetime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed: Option<String>,
    /// True when `serving` was derived by the probe rather than hand-declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_serving: Option<bool>,
}

/// filename stem is the `name`, so a drop-in omits it). `name` and `tiers`
/// therefore default — a minimal drop-in is just `endpoint` + `model`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Backend name. For a per-file drop-in this is overwritten by the filename
    /// stem, so the file body may omit it.
    #[serde(default)]
    pub name: String,
    /// HTTP endpoint URL (Ollama / OpenAI). Defaulted so a `kind = "embedded"`
    /// backend — which runs in-process and has no URL — can omit it.
    #[serde(default)]
    pub endpoint: String,
    /// Maximum concurrent inference requests issued to this named endpoint.
    /// One is the safe default for single-slot local servers; operators may
    /// opt into parallel dispatch when the backend is configured for it.
    #[serde(
        default = "default_backend_slots",
        skip_serializing_if = "backend_slots_are_default"
    )]
    pub slots: BackendSlots,
    /// The model this backend serves. OPTIONAL (#1128, epic #1126): an unset
    /// model means "the server dictates" — Phase B's probe/adopt fills it in at
    /// session start. Configs that set it keep exactly today's behavior; read
    /// through [`effective_model`](Self::effective_model), never directly, so a
    /// `None` can never misroute a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// For `kind = "embedded"`: the local GGUF model file (the in-process engine
    /// has no `endpoint`). `~/` is expanded at use. Ignored for HTTP backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_path: Option<String>,
    #[serde(default)]
    // INERT-CODE-RATCHET: F15 WIRE: empty backend tiers are the default and match every requested tier.
    pub tiers: Vec<Tier>,
    /// Which wire protocol this backend speaks. OPTIONAL (#backend-kind-probe):
    /// unset means "probe at connect" via [`crate::backend_probe::detect_endpoint`]
    /// (race `/api/tags` vs `/v1/models`). Explicit `kind = "ollama"|"openai"|…`
    /// keeps today's pinned behavior. Auth stays explicit (`api_key_*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BackendKind>,
    /// For `kind = "openai"`: which OpenAI HTTP surface to use
    /// (`chat_completions` or `responses`). OPTIONAL: unset means probe at
    /// connect (try chat/completions; adopt `responses` when the server says
    /// the model is responses-only). Explicit values stay pinned. Ignored for
    /// Ollama. Serialized only when set so a minimal drop-in stays minimal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<OpenAiApi>,
    /// Optional path to a file whose first non-empty line is a bearer
    /// token, sent as `Authorization: Bearer <token>` by
    /// OpenAI-compatible backends. A leading `~/` is expanded to the
    /// home directory. Keeping the secret in a file (rather than inline
    /// in the config) keeps tokens out of version control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
    /// Optional environment variable name holding a bearer token. Takes
    /// precedence over [`api_key_file`](Self::api_key_file) when both
    /// resolve to a non-empty value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// The serving axis (multiplexer | instance) — see [`Serving`]. Unset =
    /// derive by probing (Phase B); `newt setup` caches the derivation here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving: Option<Serving>,
    /// When set, newt actively **tends** this backend's host rather than merely
    /// consuming it — see [`ManagedMode`] and ADR
    /// `docs/decisions/managed_backend.md`. `Shared` makes
    /// [`crate::backend_probe::adopt`] prefer a warm model over forcing a swap
    /// (clash-avoidance for several agents on one box); unset = an ordinary
    /// consumed endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedMode>,
    /// The detected inference engine (ollama | llama.cpp | vllm) — see
    /// [`Engine`]. Pure metadata, orthogonal to `kind`: never gates a
    /// transport, only refines warm-model probing and display. Unset =
    /// undetected; `newt setup` caches the fingerprint result here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
    /// The physical host this endpoint lives on, for same-host reasoning (the
    /// vLLM-starves-ollama rule, crew spread). Unset = derived from the
    /// endpoint URL's host part; set it only to group endpoints the URL
    /// doesn't reveal as co-located.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Big-box escape hatch: `true` asserts this host has room to run this
    /// backend ALONGSIDE others (e.g. a huge-RAM ollama next to a small vLLM),
    /// suppressing the default "vLLM resident ⇒ same-host ollama is starved"
    /// rule. Unset = the conservative default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coexist: Option<bool>,
    /// Host memory available for serving (GiB), for the crew fit-gate
    /// (Σ model `footprint_gib` ≤ `ram_gib`). Unset = unknown (fit-gate
    /// falls back to the conservative one-model law).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_gib: Option<f64>,
    /// Model-card pointer: the card whose serving/tuning/capability blocks
    /// apply to this backend's model (instance backends especially).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// Inline capability overrides for THIS backend — same shape as a model
    /// card's `[capability]` (reused type). On an instance backend this is
    /// where adopted capabilities live; a multiplexer keeps per-model
    /// capabilities in the probe cache instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<crate::model_card::Capability>,
    /// Self-description of how this file came to be — see
    /// [`BackendProvenance`]. Written by `newt setup`; never read at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<BackendProvenance>,
}

/// A positive backend concurrency limit. The wrapper keeps both Serde input
/// and programmatic [`BackendConfig::default`] from producing a zero-slot
/// endpoint that can never make progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackendSlots(std::num::NonZeroUsize);

impl BackendSlots {
    /// The configured positive slot count.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for BackendSlots {
    fn default() -> Self {
        Self(std::num::NonZeroUsize::MIN)
    }
}

fn default_backend_slots() -> BackendSlots {
    BackendSlots::default()
}

fn backend_slots_are_default(slots: &BackendSlots) -> bool {
    *slots == default_backend_slots()
}

impl BackendConfig {
    /// Overlay `edits` onto a per-file backend drop-in's TOML `text`,
    /// **preserving comments, key order, and every key newt does not model** —
    /// unlike a serde round-trip (`toml::from_str` → mutate → `toml::to_string`),
    /// which silently destroys both. Pure: the caller owns the read/write, the
    /// same contract as [`Config::with_default_backend`], which exists for
    /// exactly this reason on the config side.
    ///
    /// Each edit is `(key, value)`: `Some` sets that top-level key to the string
    /// (creating it when absent, keeping the existing line's decor when
    /// present), `None` removes it. Only string scalars are settable, which
    /// covers every field the backend panel's form manages (`kind`, `endpoint`,
    /// `model`, `api_key_env`, `api_key_file`, `name`); an edit list that omits
    /// a key leaves it byte-for-byte alone.
    ///
    /// # Errors
    /// Returns [`NewtError::Config`] when `text` is not valid TOML.
    pub fn with_dropin_edits(text: &str, edits: &[(&str, Option<String>)]) -> Result<String> {
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| NewtError::Config(format!("backend drop-in is not valid TOML: {e}")))?;
        let root = doc.as_table_mut();
        for (key, value) in edits {
            match value {
                Some(new) => match root.get_mut(key) {
                    Some(item) => {
                        // Keep the operator's trailing comment / spacing on a
                        // key that already exists.
                        let decor = item.as_value().map(|value| value.decor().clone());
                        *item = toml_edit::value(new.as_str());
                        if let (Some(decor), Some(value)) = (decor, item.as_value_mut()) {
                            *value.decor_mut() = decor;
                        }
                    }
                    None => {
                        root.insert(key, toml_edit::value(new.as_str()));
                    }
                },
                None => {
                    root.remove(key);
                }
            }
        }
        Ok(doc.to_string())
    }

    /// Resolve explicitly accepted Chat Completions request extensions —
    /// from the INLINE block only. The card-aware answer is
    /// [`crate::model_card::ResolvedCapabilities`], constructed once per
    /// backend choice; these inline accessors stay for the card-less callers
    /// and as the conservative floor.
    #[must_use]
    pub fn chat_completions_capability(&self) -> crate::model_card::ChatCompletionsCapability {
        self.capability
            .as_ref()
            .and_then(|capability| capability.chat_completions)
            .unwrap_or_default()
    }

    /// Whether this model streams its chain-of-thought as a lone leading
    /// closer (`reasoning</think>answer`, no opening tag) and therefore needs
    /// the stream filter to start INSIDE the reasoning block.
    ///
    /// Reads THIS backend's inline [`Capability`] — never the model name.
    /// Display names are labels: an operator may serve any artifact under any
    /// alias, so `contains("qwen3")` is wrong in both directions (it
    /// suppresses output from things that are not Qwen, and prints raw
    /// reasoning from things that are). Replaces the #384 name-list stopgap.
    ///
    /// **Scope:** like its two siblings above, this reads the inline
    /// `capability` field only — the conservative floor. The card-aware
    /// surface is [`crate::model_card::ResolvedCapabilities`], which resolves
    /// the named `card =` binding once per backend choice and decides per
    /// serving principal; every runtime lane consumes that, not this.
    ///
    /// **Unknown defaults to `false` — do not suppress.** The two failure
    /// modes are not symmetric: filtering when we should not DROPS real answer
    /// text silently, while not filtering when we should shows reasoning the
    /// operator can see and correct. Fail toward the visible one.
    #[must_use]
    pub fn emits_leading_reasoning(&self) -> bool {
        self.capability
            .as_ref()
            .and_then(|capability| capability.emits_leading_reasoning)
            .unwrap_or(false)
    }

    /// Resolve the backend's reasoning replay contract. Unknown or legacy
    /// endpoints remain conservative and never receive replayed reasoning.
    #[must_use]
    pub fn reasoning_replay_scope(&self) -> crate::model_card::ReasoningReplayScope {
        self.capability
            .as_ref()
            .and_then(|capability| capability.reasoning_replay_scope)
            .unwrap_or_default()
    }

    /// The declared model, if any — empty strings count as unset. This is the
    /// ONLY sanctioned way to read `model`; when it returns `None` the backend
    /// expects the served model to be adopted from the endpoint (Phase B).
    pub fn effective_model(&self) -> Option<&str> {
        self.model.as_deref().filter(|m| !m.trim().is_empty())
    }

    /// True when `kind` was omitted — session start / doctor must run
    /// [`crate::backend_probe::detect_endpoint`] before speaking the wire.
    pub fn needs_kind_probe(&self) -> bool {
        self.kind.is_none()
    }

    /// Human label for lists/preambles: the pinned protocol, or `"auto"` when
    /// unset (probe fills it in at connect).
    pub fn kind_label(&self) -> &'static str {
        self.kind.map(BackendKind::label).unwrap_or("auto")
    }

    /// Resolve this backend's bearer token, if any.
    ///
    /// Checks [`api_key_env`](Self::api_key_env) first (environment
    /// variable), then [`api_key_file`](Self::api_key_file) — plaintext
    /// (first non-empty line, trimmed) or age-encrypted (`.token.age`,
    /// decrypted through [`crate::secrets`]). Returns `None` when nothing
    /// resolves; a LOCKED/broken encrypted token additionally warns once per
    /// path so it is never a silent `None` (use
    /// [`resolve_api_key_detailed`](Self::resolve_api_key_detailed) for the
    /// typed reason).
    pub fn resolve_api_key(&self) -> Option<String> {
        match self.resolve_api_key_detailed() {
            Ok(v) => v,
            Err(e) => {
                crate::secrets::warn_once(self.api_key_file.as_deref().unwrap_or(&self.name), &e);
                None
            }
        }
    }

    /// [`resolve_api_key`](Self::resolve_api_key) with the typed failure —
    /// doctor and worker startup lines surface the actionable reason
    /// (passphrase required / wrong passphrase / corrupt file).
    pub fn resolve_api_key_detailed(
        &self,
    ) -> std::result::Result<Option<String>, crate::secrets::SecretsError> {
        resolve_api_key_common(self.api_key_env.as_deref(), self.api_key_file.as_deref())
    }
}

/// A side call's backend, expressed as an **override of the session backend**.
///
/// Every field is optional and an absent field inherits the session's value, so
/// an empty table means "run this exactly where the session runs". This is the
/// same inherit-or-override shape `[summarizer]` already uses; it is factored
/// out here so a second consumer does not hand-roll a third spelling of
/// *(endpoint, model, kind, key)*.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackendRef {
    /// `None` ⇒ reuse the session endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// `None` ⇒ reuse the session model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `None` ⇒ reuse the session wire protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<BackendKind>,
    /// Bearer-token environment variable (checked before `api_key_file`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Bearer-token file (first non-empty line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
}

impl BackendRef {
    /// Does this override point somewhere other than the session endpoint?
    #[must_use]
    pub fn pins_a_different_host(&self) -> bool {
        self.endpoint.is_some()
    }

    /// Resolve against the session's `(endpoint, model, kind, key)`.
    ///
    /// The api-key rule is the one that matters and mirrors the summarizer's
    /// (`resolve_summarizer_backend`): **a bearer token authenticates a
    /// specific host**, so the session key is inherited only when this call
    /// reuses the session endpoint. Pinning a different host and inheriting the
    /// session's credential would leak it.
    #[must_use]
    pub fn resolve(
        &self,
        session_endpoint: &str,
        session_model: &str,
        session_kind: BackendKind,
        session_key: &Option<String>,
    ) -> (String, String, BackendKind, Option<String>) {
        let own_key =
            resolve_api_key_common(self.api_key_env.as_deref(), self.api_key_file.as_deref())
                .unwrap_or_default();
        let key = if self.pins_a_different_host() {
            own_key
        } else {
            own_key.or_else(|| session_key.clone())
        };
        (
            self.endpoint
                .clone()
                .unwrap_or_else(|| session_endpoint.to_string()),
            self.model
                .clone()
                .unwrap_or_else(|| session_model.to_string()),
            self.kind.unwrap_or(session_kind),
            key,
        )
    }
}

/// The ONE env-then-file credential rule shared by [`BackendConfig`] and
/// [`SummarizerConfig`](super::SummarizerConfig). Env wins when set and non-empty; the file path goes
/// through `secrets::resolve_token_file` (plaintext and encrypted alike).
pub(crate) fn resolve_api_key_common(
    api_key_env: Option<&str>,
    api_key_file: Option<&str>,
) -> std::result::Result<Option<String>, crate::secrets::SecretsError> {
    if let Some(var) = api_key_env {
        if let Ok(val) = std::env::var(var) {
            let val = val.trim();
            if !val.is_empty() {
                return Ok(Some(val.to_string()));
            }
        }
    }
    if let Some(path) = api_key_file {
        let expanded = expand_tilde(path);
        return crate::secrets::resolve_token_file(&expanded);
    }
    Ok(None)
}

/// A backend's EXACT destination — where the session's bytes go: an HTTP
/// `endpoint`, or (`kind = "embedded"`) a local `model_path`. The ONLY
/// normalization anywhere is empty-string-to-`None`; comparison is exact
/// string equality, never URL parsing or trimming (a near-collision must
/// compare unequal, not get "helpfully" unified).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendDestination {
    /// The HTTP endpoint, when one is declared/requested (empty ⇒ `None`).
    pub endpoint: Option<String>,
    /// The local artifact path for an embedded backend.
    pub model_path: Option<String>,
}

impl BackendDestination {
    /// Empty-to-`None` construction — the one normalization.
    #[must_use]
    pub fn new(endpoint: Option<String>, model_path: Option<String>) -> Self {
        Self {
            endpoint: endpoint.filter(|e| !e.is_empty()),
            model_path: model_path.filter(|p| !p.is_empty()),
        }
    }

    /// The destination a backend declaration names.
    #[must_use]
    pub fn of(backend: &BackendConfig) -> Self {
        Self::new(Some(backend.endpoint.clone()), backend.model_path.clone())
    }

    /// A CONCRETE destination: exactly one NONEMPTY axis (endpoint XOR
    /// model_path). A hollow destination (neither) routes nowhere and a
    /// composite one (both) is two identities — neither may anchor an exact
    /// association ([`crate::model_card::ResolvedCapabilities::for_route`]
    /// refuses to activate a card across a non-concrete destination). The
    /// fields are public, so a hand-built literal can hold `Some("")` that
    /// [`BackendDestination::new`] would have normalized away — concreteness
    /// therefore checks CONTENT, not `Option` presence: two empty-string
    /// endpoints agreeing are two absences, not an identity.
    #[must_use]
    pub fn is_concrete(&self) -> bool {
        let endpoint = self.endpoint.as_deref().is_some_and(|e| !e.is_empty());
        let model_path = self.model_path.as_deref().is_some_and(|p| !p.is_empty());
        endpoint ^ model_path
    }
}

/// The operator's DECLARED backend facts — the layer as configured (inline
/// `[[backends]]` or an `operator_v1` drop-in), before any probe overlay or
/// CLI request. Immutable intent: never probe residue, never a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredBackend {
    /// Where the operator pointed this backend.
    pub destination: BackendDestination,
    /// The declared model, if any.
    pub model: Option<String>,
    /// The declared model card, if any.
    pub card: Option<String>,
    /// The declared serving axis.
    pub serving: Option<Serving>,
    /// The declared wire protocol.
    pub kind: Option<BackendKind>,
    /// The declared OpenAI HTTP surface.
    pub api: Option<OpenAiApi>,
    /// The declared managed mode.
    pub managed: Option<ManagedMode>,
}

impl DeclaredBackend {
    /// Snapshot the declaration layer from a backend that IS pure
    /// declaration (nothing has overlaid it).
    #[must_use]
    pub fn of(backend: &BackendConfig) -> Self {
        Self {
            destination: BackendDestination::of(backend),
            // The effective-model rule: an empty/whitespace model string is
            // NO model identity — it must never become an exact identifier
            // a card binding could associate against.
            model: backend.effective_model().map(str::to_string),
            card: backend.card.clone(),
            serving: backend.serving,
            kind: backend.kind,
            api: backend.api,
            managed: backend.managed,
        }
    }
}

/// How a CLI `--backend-*` request targets the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMode {
    /// A destination (`--backend-url` / `--backend-model-path`) was given:
    /// the request defines an EXCLUSIVE backend — one slot survives.
    ExclusiveDestination,
    /// Field-only: the named (else first) backend is edited in place.
    FieldOnly,
}

/// The explicit per-invocation CLI request, recorded AS a request — typed
/// facts taken from the `--backend-*` flags themselves, never re-derived
/// from the mutated backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRequest {
    /// Exclusive-destination or field-only (see [`RequestMode`]).
    pub mode: RequestMode,
    /// The requested endpoint, if any (empty ⇒ `None`).
    pub endpoint: Option<String>,
    /// The requested embedded artifact path, if any.
    pub model_path: Option<String>,
    /// The requested model, if any.
    pub model: Option<String>,
    /// The requested card rebind, if any.
    pub card: Option<String>,
    /// The requested serving axis, if any.
    pub serving: Option<Serving>,
    /// The requested wire protocol, if any.
    pub kind: Option<BackendKind>,
    /// The requested OpenAI HTTP surface, if any.
    pub api: Option<OpenAiApi>,
}

impl BackendRequest {
    fn from_override(over: &BackendOverride) -> Self {
        let mode = if over.endpoint.is_some() || over.model_path.is_some() {
            RequestMode::ExclusiveDestination
        } else {
            RequestMode::FieldOnly
        };
        Self {
            mode,
            endpoint: over.endpoint.clone().filter(|e| !e.is_empty()),
            model_path: over.model_path.clone().filter(|p| !p.is_empty()),
            // Same effective-model rule as the declaration layer: an
            // empty/whitespace request is no model identity.
            model: over.model.clone().filter(|m| !m.trim().is_empty()),
            card: over.card.clone(),
            serving: over.serving,
            kind: over.kind,
            api: over.api,
        }
    }

    /// The destination the request lands on, given the declared one: the
    /// requested endpoint/model_path override their declared counterparts
    /// field-by-field; a request with neither lands where declared.
    #[must_use]
    pub fn destination_over(&self, declared: &BackendDestination) -> BackendDestination {
        if self.endpoint.is_none() && self.model_path.is_none() {
            return declared.clone();
        }
        // A destination request REPLACES the destination: `--backend-url`
        // points the exclusive backend at that URL (it does not inherit a
        // declared model_path, nor vice versa).
        BackendDestination {
            endpoint: self.endpoint.clone(),
            model_path: self.model_path.clone(),
        }
    }
}

/// Per-backend provenance receipt: the LAYERS a resolved backend was
/// composed from, kept distinguishable. [`Config::resolve`] flattens
/// operator declaration → cached `probe_v1` observation → per-invocation
/// CLI request into one effective [`BackendConfig`] — right for wire
/// routing, wrong for evidence: a consumer reading `backend.model` cannot
/// tell a declaration from probe residue or a request. Receipts are built
/// by the private backend assembly and ride in [`ResolvedConfig`](super::ResolvedConfig), aligned
/// 1:1 by slot with `config.backends` — never looked up by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendResolutionReceipt {
    /// The operator's declaration layer.
    pub declaration: DeclaredBackend,
    /// The explicit CLI request, if any.
    pub request: Option<BackendRequest>,
    /// The cached probe observation this resolution retained, if any. A
    /// requested destination CHANGE clears it (cached truth about one
    /// server must not ride to another); an identical destination retains.
    pub observation: Option<ProbeObservation>,
    /// The card-binding evidence this resolution justifies — see
    /// [`crate::model_card::CardBindingSeed`]:
    ///
    /// * an explicit `--backend-card` is a deliberate rebind — the requested
    ///   card binds at the post-request destination, to the requested model
    ///   (else the declared one, NEVER a probed one);
    /// * otherwise the declared card binds to the declared model at the
    ///   declared destination — a model-only or endpoint-only request
    ///   RETAINS this binding untouched, and visibility is decided
    ///   downstream by typed applicability
    ///   ([`crate::model_card::ResolvedCapabilities::for_route`]), never by
    ///   erasing evidence here.
    pub binding: crate::model_card::CardBindingSeed,
}

/// Shared backend-identity validation: nonempty and unique names. Selection
/// (`default_backend`, `$NEWT_PROVIDER`), CLI overrides, drop-in merging,
/// and the slot-aligned receipts are all name-addressed at their edges —
/// with a duplicate, different consumers can disagree about WHICH backend a
/// name means and hand backend A the card binding declared for backend B.
/// Hard, actionable error instead. Used by the assembly constructor on
/// every path (normal resolve AND profiles) and again after the CLI
/// request.
pub(super) fn validate_backend_names<'a>(
    backends: impl Iterator<Item = &'a BackendConfig>,
) -> std::result::Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for (i, b) in backends.enumerate() {
        if b.name.trim().is_empty() {
            return Err(format!(
                "backend #{} has no name — every [[backends]] entry needs a unique \
                 `name` (selection, overrides, and card bindings are name-based)",
                i + 1
            ));
        }
        if !seen.insert(b.name.clone()) {
            return Err(format!(
                "two backends share the name `{}` — backend selection is name-based \
                 everywhere (default_backend, $NEWT_PROVIDER, --backend-*, card \
                 bindings), so a duplicate can activate the wrong card; rename one",
                b.name
            ));
        }
    }
    Ok(())
}

/// A declaration with SOME destination — a nonempty endpoint or a nonempty
/// `model_path`. `model_path = ""` is NOT a destination: an empty-path
/// drop-in must not pass destination checks and strip a valid slot.
fn backend_has_destination(b: &BackendConfig) -> bool {
    !b.endpoint.is_empty() || b.model_path.as_deref().is_some_and(|p| !p.is_empty())
}

/// Shared destination-XOR validation for DECLARATIONS: a backend has ONE
/// destination — an HTTP `endpoint`, or an embedded `model_path`, never
/// both (a composite destination is two identities in one slot; every
/// consumer — routing, probe association, card bindings — would pick a
/// side silently). CLI requests get the same rule in
/// [`BackendAssembly::apply_request`].
fn validate_backend_destination(b: &BackendConfig) -> std::result::Result<(), String> {
    if !b.endpoint.is_empty() && b.model_path.as_deref().is_some_and(|p| !p.is_empty()) {
        return Err(format!(
            "backend `{}` declares BOTH an endpoint and a model_path — a backend has \
             ONE destination; remove one",
            b.name
        ));
    }
    Ok(())
}

/// A defensive name-lookup outcome — assembly operations never assume a
/// name resolves, even though the constructor validated uniqueness.
enum NameMatch {
    Missing,
    Unique(usize),
    Ambiguous,
}

/// One backend under assembly: the operator's declaration plus the layers
/// that may (or may not) apply to it. The layers stay SEPARATE until
/// [`BackendAssembly::finish`] composes the effective backend and mints the
/// receipt — so a later layer can never masquerade as an earlier one.
#[derive(Debug)]
struct AssemblySlot {
    /// The declaration: inline `[[backends]]`, replaced wholesale by an
    /// `operator_v1` drop-in.
    declaration: BackendConfig,
    /// The exact probe observation attached to this slot, if any.
    observation: Option<ProbeObservation>,
    /// The CLI `--backend-*` request targeted at this slot, if any.
    request: Option<BackendOverride>,
}

impl AssemblySlot {
    fn declared(declaration: BackendConfig) -> Self {
        Self {
            declaration,
            observation: None,
            request: None,
        }
    }

    /// The declaration with this slot's observation overlaid — the
    /// PROBE-INFORMED effective view (pre-request). Used both by
    /// [`BackendAssembly::finish`]'s composition and by the CLI targeting
    /// in [`BackendAssembly::apply_request`], so the backend a field-only
    /// edit lands on and the backend the final resolution selects can
    /// never diverge over probed facts.
    fn observed_view(&self) -> BackendConfig {
        let mut backend = self.declaration.clone();
        if let Some(obs) = &self.observation {
            overlay_observation(&mut backend, obs);
        }
        normalize_destination_kind(&mut backend);
        backend
    }
}

/// Destination/kind coherence normalization — the SAME rule in composition
/// ([`BackendAssembly::finish`]) and the targeting preview
/// ([`AssemblySlot::observed_view`]), so a declaration the composition
/// would accept (model_path + a stale HTTP kind, normalized to Embedded)
/// is never refused by a harmless field-only edit that previewed it
/// un-normalized. Both axes:
///
/// * **kind** — a model_path route IS embedded; an endpoint route never
///   retains Embedded (cleared to probe-at-connect);
/// * **serving** — an embedded backend serves exactly ONE artifact
///   ([`derive_serving`] makes Embedded intrinsically Instance), so a
///   model_path route never retains an inherited/declared Multiplexer —
///   Phase B's principal decision must never see
///   `kind = Embedded + serving = multiplexer`. (EXPLICITLY contradictory
///   requests are rejected in `apply_request`, not normalized away.)
fn normalize_destination_kind(backend: &mut BackendConfig) {
    if backend.endpoint.is_empty() && backend.model_path.as_deref().is_some_and(|p| !p.is_empty()) {
        backend.kind = Some(BackendKind::Embedded);
        backend.serving = Some(Serving::Instance);
    } else if !backend.endpoint.is_empty() && backend.kind == Some(BackendKind::Embedded) {
        backend.kind = None;
    }
}

/// Overlay a probe observation's facts onto a backend — only what a probe
/// observes: `kind`/`api`/`serving`, plus the model iff Instance (the typed
/// [`ProbeObservation::serving_axis`] gate). The ONE overlay, shared by
/// composition and targeting.
fn overlay_observation(backend: &mut BackendConfig, obs: &ProbeObservation) {
    if let Some(kind) = obs.kind {
        backend.kind = Some(kind);
    }
    if let Some(api) = obs.api {
        backend.api = Some(api);
    }
    let (serving, model) = obs.serving_axis();
    if let Some(serving) = serving {
        backend.serving = Some(serving);
        // Only an Instance observation carries backend-truth model; a
        // multiplexer/unknown observation leaves the declared model
        // standing.
        if let Some(model) = model {
            backend.model = Some(model);
        }
    }
}

/// A probe record staged during the directory walk, attached only after
/// EVERY directory's operator declarations have applied — so a probe in an
/// earlier directory is judged against the FINAL declaration, not against
/// whichever declaration happened to exist when its file was read.
#[derive(Debug)]
struct PendingProbe {
    path: PathBuf,
    stem: String,
    observation: ProbeObservation,
}

/// The PRIVATE backend assembly: the one place the four layers of a
/// backend meet, in order — inline/project declaration → operator drop-in
/// replacement → exact probe observation → CLI request. Owns the layering
/// rules so [`ResolvedConfig`](super::ResolvedConfig)'s receipts are correct BY CONSTRUCTION:
///
/// * the constructor validates backend identity (nonempty, unique names)
///   on every path — normal resolve and profiles alike;
/// * an operator drop-in REPLACES its slot's declaration and resets the
///   slot's observation (the file IS the backend);
/// * a probe record attaches only to the UNIQUE slot with the exact same
///   name AND destination — cached truth about one server never rides to
///   another;
/// * the CLI request is recorded as a request; an exclusive destination
///   request retains exactly one (chosen or new) slot.
#[derive(Debug)]
pub(super) struct BackendAssembly {
    slots: Vec<AssemblySlot>,
    /// Probe records staged for post-declaration attachment, in walk order
    /// (directory precedence, then path order) — attachment is last-wins,
    /// so a later directory's probe record deterministically supersedes an
    /// earlier one for the same slot.
    pending_probes: Vec<PendingProbe>,
    /// An operator drop-in merged — the config is operator-configured.
    operator_configured: bool,
    /// A nonempty CLI request was applied.
    requested: bool,
    /// #1984: every skip/degrade decision this assembly made, as VALUES —
    /// the primary record. `warn` (below) is the ONE place that both
    /// appends here and emits the `tracing::warn!` a human `RUST_LOG=warn`
    /// session still sees; every other call site in this impl block goes
    /// through it rather than calling `tracing::warn!` directly, so there
    /// is exactly one emission point to keep in sync. Tests assert on
    /// `warnings()`, not on a scraped log — see `config_tests/tests.rs`'s
    /// module doc for why the log-scraping shape was flaky (a per-test
    /// `tracing::subscriber::with_default` capture races tracing's
    /// process-wide callsite interest cache against sibling tests doing
    /// the same, #1984).
    warnings: Vec<String>,
}

impl BackendAssembly {
    /// Stage `backends` (pure declarations) for assembly, validating
    /// backend identity first — see [`validate_backend_names`].
    pub(super) fn new(backends: Vec<BackendConfig>) -> std::result::Result<Self, String> {
        validate_backend_names(backends.iter())?;
        for b in &backends {
            validate_backend_destination(b)?;
        }
        Ok(Self {
            slots: backends.into_iter().map(AssemblySlot::declared).collect(),
            pending_probes: Vec::new(),
            operator_configured: false,
            requested: false,
            warnings: Vec::new(),
        })
    }

    /// The ONE place this impl block records a skip/degrade decision:
    /// appends `message` to the returned-value record (#1984's fix) and
    /// emits it as a `tracing::warn!` so an operator with `RUST_LOG=warn`
    /// (the default — see #1951) still sees it live. `message` should read
    /// the same whether it reaches a human via the log or a test via
    /// [`Self::warnings`].
    fn warn(&mut self, message: String) {
        tracing::warn!("{message}");
        self.warnings.push(message);
    }

    /// Every skip/degrade decision recorded so far, in the order they
    /// happened — the returned-value record `warn` builds. Callable any
    /// time before [`Self::finish`] consumes `self`.
    ///
    /// `#[cfg(test)]`: `warn` (above) is the sole PRODUCTION consumer of
    /// `self.warnings` today (it feeds `tracing::warn!`) — nothing in
    /// production reads the accumulated Vec back out yet. `newt doctor`'s
    /// drop-in diagnostics (#1951/#1962) were checked as a candidate
    /// consumer and are NOT: that scan deliberately does not call
    /// `merge_dir` at all, because it must keep reporting file-by-file even
    /// when `merge_dir` hard-errors (the ambiguous-legacy-marker case) —
    /// exactly the failure this accessor's caller would already be past.
    /// Un-gate this the day a production caller needs it; until then,
    /// `#[cfg(test)]` is the honest signal that it is a value the TESTS
    /// rely on, not a currently-dead production API surface.
    #[cfg(test)]
    pub(super) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(super) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub(super) fn operator_configured(&self) -> bool {
        self.operator_configured
    }

    pub(super) fn requested(&self) -> bool {
        self.requested
    }

    /// The compiled-in localhost fallback, staged as a declaration.
    pub(super) fn push_fallback(&mut self, backend: BackendConfig) {
        self.slots.push(AssemblySlot::declared(backend));
    }

    fn find(&self, name: &str) -> NameMatch {
        let mut hits = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.declaration.name == name)
            .map(|(i, _)| i);
        match (hits.next(), hits.next()) {
            (None, _) => NameMatch::Missing,
            (Some(i), None) => NameMatch::Unique(i),
            (Some(_), Some(_)) => NameMatch::Ambiguous,
        }
    }

    /// Merge `<dir>/*.toml` drop-ins (filename stem = name), branching on
    /// the file's raw `record` header:
    ///
    /// * **Operator records** (`record = "operator_v1"`, or untagged and
    ///   classified operator) — REPLACE the same-name slot's declaration
    ///   wholesale (resetting its observation), else append a new slot.
    ///   Omissions deliberately clear/rebind; the file IS the backend.
    /// * **Probe records** (`record = "probe_v1"`, or an unambiguous
    ///   legacy probe cache) — parsed through the STRICT machine schema
    ///   and STAGED; they attach as slot observations only after every
    ///   directory's declarations have applied (see
    ///   [`Self::attach_pending_probes`]), so a home-dir probe survives to
    ///   be judged against a project-dir declaration. Never card,
    ///   capability, auth, tiers, managed, host, or operator provenance;
    ///   an invalid record is skipped with a visible warning.
    ///
    /// A malformed file is skipped with a warning. The one HARD ERROR is
    /// the legacy ambiguity: a file carrying the exact old newt-adopt probe
    /// marker AND binding/operator evidence cannot be attributed (operator
    /// declaration, or probe residue?) — refuse to guess, name the path and
    /// the remediations.
    pub(super) fn merge_dir(&mut self, dir: &Path) -> std::result::Result<(), String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(()); // no backends dir — fine
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let tag = match disk_record_tag(&text) {
                Ok(tag) => tag,
                Err(e) => {
                    self.warn(format!(
                        "{}: skipping malformed backend file: {e}",
                        path.display()
                    ));
                    continue;
                }
            };
            match tag {
                Some(RecordTag::ProbeV1) => self.stage_probe(&path, stem, &text),
                Some(RecordTag::OperatorV1) => self.merge_operator(&path, stem, &text),
                None => {
                    let backend = match toml::from_str::<BackendConfig>(&text) {
                        Ok(backend) => backend,
                        Err(e) => {
                            // The header parse is laxer than the full parse
                            // (it reads one key) — a body-malformed file
                            // lands here, same visible skip as everywhere.
                            self.warn(format!(
                                "{}: skipping malformed backend file: {e}",
                                path.display()
                            ));
                            continue;
                        }
                    };
                    match classify_untagged_dropin(&backend, &text) {
                        Ok(DropinOwner::Operator) => self.merge_operator(&path, stem, &text),
                        Ok(DropinOwner::Probe) => self.stage_probe(&path, stem, &text),
                        Err(reason) => return Err(format!("{}: {reason}", path.display())),
                    }
                }
            }
        }
        Ok(())
    }

    /// An operator record: the file IS the backend — its declaration
    /// replaces the slot wholesale and resets the slot's observation. It
    /// needs a destination — an HTTP `endpoint`, or (`kind = "embedded"`)
    /// a local `model_path`; a record with neither is skipped TOUCHING
    /// NOTHING (a skipped record must not strip what an earlier layer
    /// established).
    fn merge_operator(&mut self, path: &Path, stem: &str, text: &str) {
        let mut backend = match toml::from_str::<BackendConfig>(text) {
            Ok(backend) => backend,
            Err(e) => {
                self.warn(format!(
                    "{}: skipping malformed backend file: {e}",
                    path.display()
                ));
                return;
            }
        };
        // The filename is authoritative for the name (collision-free).
        backend.name = stem.to_string();
        if !backend_has_destination(&backend) {
            self.warn(format!(
                "{}: skipping backend with neither endpoint nor model_path",
                path.display()
            ));
            return;
        }
        if let Err(reason) = validate_backend_destination(&backend) {
            self.warn(format!(
                "{}: skipping backend drop-in: {reason}",
                path.display()
            ));
            return;
        }
        self.operator_configured = true;
        match self.find(stem) {
            NameMatch::Unique(i) => self.slots[i] = AssemblySlot::declared(backend),
            NameMatch::Missing => self.slots.push(AssemblySlot::declared(backend)),
            NameMatch::Ambiguous => {
                // Unreachable (the constructor validated uniqueness) — but
                // never guess which duplicate a file means.
                self.warn(format!(
                    "{}: several staged backends share this name — drop-in not merged",
                    path.display()
                ));
            }
        }
    }

    /// A probe record: parse through the STRICT machine schema and stage it
    /// for attachment after all declarations are in.
    fn stage_probe(&mut self, path: &Path, stem: &str, text: &str) {
        let record = match parse_probe_record(text) {
            Ok(record) => record,
            Err(reason) => {
                self.warn(format!(
                    "{}: invalid probe record — not overlaid (delete the file to re-probe): {reason}",
                    path.display()
                ));
                return;
            }
        };
        self.pending_probes.push(PendingProbe {
            path: path.to_path_buf(),
            stem: stem.to_string(),
            observation: record.to_observation(stem),
        });
    }

    /// Attach every staged probe record against the FINAL declarations:
    /// the unique slot with the exact same name AND destination. Walk
    /// order, last-wins — a later directory's record deterministically
    /// supersedes an earlier one. A name or destination that no final
    /// declaration matches is skipped with a visible warning.
    fn attach_pending_probes(&mut self) {
        for pending in std::mem::take(&mut self.pending_probes) {
            let PendingProbe {
                path,
                stem,
                observation,
            } = pending;
            let slot = match self.find(&stem) {
                NameMatch::Unique(i) => &mut self.slots[i],
                NameMatch::Missing => {
                    self.warn(format!(
                        "{}: probe record names an unconfigured backend — ignored (delete the file)",
                        path.display()
                    ));
                    continue;
                }
                NameMatch::Ambiguous => {
                    self.warn(format!(
                        "{}: several staged backends share this name — probe record not attached",
                        path.display()
                    ));
                    continue;
                }
            };
            // Association is the exact declared destination — an endpoint-less
            // (embedded) backend is never overlaid, and a near-collision is a
            // different destination, not a match.
            let observed_at = BackendDestination::new(Some(observation.endpoint.clone()), None);
            let declared_at = BackendDestination::of(&slot.declaration);
            if declared_at != observed_at {
                let configured = slot.declaration.endpoint.clone();
                let probed = observation.endpoint.clone();
                self.warn(format!(
                    "{}: probe record's destination does not match the configured backend \
                     (configured={configured}, probed={probed}) — not overlaid",
                    path.display()
                ));
                continue;
            }
            slot.observation = Some(observation);
        }
    }

    /// Record the CLI `--backend-*` request.
    ///
    /// A destination request (`--backend-url` XOR `--backend-model-path` —
    /// exactly one, nonempty) defines an EXCLUSIVE backend: exactly one
    /// slot survives — the uniquely named existing one (its declaration and
    /// observation intact; whether the observation still applies is decided
    /// in [`Self::finish`]) or a brand-new slot with no declaration layer.
    ///
    /// A field-only request targets ONE slot in place: the named one (a
    /// name matching nothing is a hard, actionable error — `--backend-name`
    /// is both the edit target and this invocation's selection, never a
    /// silent no-op), else the slot the shared [`select_backend_slot`]
    /// picks — the SAME selector every consumer uses, so the edited backend
    /// IS the selected backend, never "index 0".
    ///
    /// Names are validated AGAIN afterwards — a request-created slot's name
    /// enters here.
    /// Returns the SLOT INDEX the request landed on (`None` when there was
    /// no request) so composing callers can align config-level selection
    /// with the target.
    pub(super) fn apply_request(
        &mut self,
        over: Option<BackendOverride>,
        default_backend: Option<&str>,
    ) -> std::result::Result<Option<usize>, String> {
        // Probe attachment resolves against the FINAL directory
        // declarations BEFORE any exclusive pruning — a valid cache for a
        // disk-declared backend must not look "unconfigured" (and emit the
        // destructive delete/re-probe warning) merely because THIS
        // invocation selected another backend.
        self.attach_pending_probes();
        let Some(over) = over.filter(|o| !o.is_empty()) else {
            return Ok(None);
        };
        // Destination invariants: empty strings are malformed requests, and
        // a request cannot point two places at once.
        if over.endpoint.as_deref().is_some_and(str::is_empty) {
            return Err("--backend-url is empty — give a URL or omit the flag".into());
        }
        if over.model_path.as_deref().is_some_and(str::is_empty) {
            return Err("--backend-model-path is empty — give a path or omit the flag".into());
        }
        if over.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
            return Err(
                "--backend-model is empty — give a model or omit the flag (there is \
                 no implicit clear: the flattened route would serve \
                 server-decides while the receipt fell back to the stale \
                 declared model)"
                    .into(),
            );
        }
        if over.endpoint.is_some() && over.model_path.is_some() {
            return Err(
                "--backend-url and --backend-model-path are mutually exclusive — a \
                 backend has ONE destination (an HTTP endpoint, or an embedded \
                 artifact path)"
                    .into(),
            );
        }
        // Destination/kind coherence: an explicitly contradictory pair is an
        // operator error, not something to silently normalize away.
        if over.endpoint.is_some() && over.kind == Some(BackendKind::Embedded) {
            return Err(
                "--backend-url with --backend-kind embedded is contradictory — an \
                 embedded backend has no endpoint; use --backend-model-path"
                    .into(),
            );
        }
        if over.model_path.is_some() && over.kind.is_some_and(|k| k != BackendKind::Embedded) {
            return Err(format!(
                "--backend-model-path with --backend-kind {:?} is contradictory — a \
                 model_path destination is an embedded backend",
                over.kind.unwrap()
            ));
        }
        if over.model_path.is_some() && over.serving == Some(Serving::Multiplexer) {
            return Err(
                "--backend-model-path with --backend-serving multiplexer is \
                 contradictory — an embedded backend serves exactly one artifact \
                 (instance)"
                    .into(),
            );
        }
        self.requested = true;
        let has_destination = over.endpoint.is_some() || over.model_path.is_some();
        if has_destination {
            let name = over.name.clone().unwrap_or_else(|| "cli".to_string());
            let kept = match self.find(&name) {
                NameMatch::Unique(i) => self.slots.swap_remove(i),
                NameMatch::Missing => AssemblySlot::declared(BackendConfig {
                    name: name.clone(),
                    ..Default::default()
                }),
                NameMatch::Ambiguous => {
                    return Err(format!(
                        "--backend-* targets `{name}`, which several backends share — \
                         rename one"
                    ));
                }
            };
            self.slots = vec![kept];
            self.slots[0].request = Some(over);
            validate_backend_names(self.slots.iter().map(|s| &s.declaration))?;
            return Ok(Some(0));
        }
        {
            // Field-only targeting runs over the PROBE-INFORMED effective
            // view ([`AssemblySlot::observed_view`]) — the same facts the
            // final resolution selects on — so the slot the edit lands on
            // and the slot the session then selects cannot diverge over a
            // probed kind.
            let effective: Vec<BackendConfig> =
                self.slots.iter().map(AssemblySlot::observed_view).collect();
            let idx = match over.name.as_deref() {
                Some(n) => match self.find(n) {
                    NameMatch::Unique(i) => {
                        if !backend_is_routable(&effective[i]) {
                            return Err(format!(
                                "--backend-name `{n}` names a backend with neither an \
                                 endpoint nor a model_path — a field-only --backend-* \
                                 cannot route it; give it a destination \
                                 (--backend-url / --backend-model-path) or fix the \
                                 backend"
                            ));
                        }
                        i
                    }
                    NameMatch::Missing => {
                        let configured: Vec<&str> = self
                            .slots
                            .iter()
                            .map(|s| s.declaration.name.as_str())
                            .collect();
                        return Err(format!(
                            "--backend-name `{n}` matches no configured backend \
                             (configured: {configured:?}) — a field-only --backend-* \
                             edits an existing backend; add --backend-url to define \
                             a new one"
                        ));
                    }
                    NameMatch::Ambiguous => {
                        return Err(format!(
                            "--backend-* targets `{n}`, which several backends share — \
                             rename one"
                        ));
                    }
                },
                None => {
                    let declarations: Vec<&BackendConfig> = effective.iter().collect();
                    match select_backend_slot(&declarations, default_backend) {
                        SlotSelection::Slot(i) => i,
                        // A field-only request supplies no destination, so
                        // editing the explicitly selected but destination-less
                        // backend could not make it routable — and editing any
                        // OTHER backend would desert the explicit selection.
                        SlotSelection::ExplicitlyUnroutable { name } => {
                            return Err(format!(
                                "--backend-* targets `{name}` (named by $NEWT_PROVIDER or \
                                 default_backend), which has neither an endpoint nor a \
                                 model_path — a field-only --backend-* cannot route it; \
                                 give it a destination (--backend-url / \
                                 --backend-model-path) or fix the backend"
                            ));
                        }
                        SlotSelection::ExplicitlyUnmatched { name } => {
                            return Err(format!(
                                "--backend-* would apply to the selected backend, but \
                                 $NEWT_PROVIDER/default_backend names `{name}`, which \
                                 matches no configured backend (it may name a provider, \
                                 which --backend-* cannot edit) — fix the selector or \
                                 name a backend with --backend-name"
                            ));
                        }
                        SlotSelection::None => {
                            return Err("--backend-* has no backend to apply to — nothing \
                                 configured is routable; name one with --backend-name \
                                 or define one with --backend-url"
                                .into());
                        }
                    }
                }
            };
            // A field-only kind change must agree with the destination the
            // target already has — refused ATOMICALLY here, never recorded
            // and then silently normalized away in composition.
            if let Some(kind) = over.kind {
                let target = &effective[idx];
                if kind == BackendKind::Embedded && !target.endpoint.is_empty() {
                    return Err(format!(
                        "--backend-kind embedded on `{}` is contradictory — its \
                         destination is an HTTP endpoint; retarget with \
                         --backend-model-path or pick an HTTP kind",
                        target.name
                    ));
                }
                if kind != BackendKind::Embedded
                    && target.endpoint.is_empty()
                    && target.model_path.as_deref().is_some_and(|p| !p.is_empty())
                {
                    return Err(format!(
                        "--backend-kind {kind:?} on `{}` is contradictory — its \
                         destination is an embedded model_path; retarget with \
                         --backend-url or keep kind embedded",
                        target.name
                    ));
                }
            }
            // A field-only serving change must agree with the target's
            // destination, exactly like kind: an embedded (model_path)
            // backend serves one artifact — refused ATOMICALLY, never
            // recorded and then silently normalized away.
            if over.serving == Some(Serving::Multiplexer) {
                let target = &effective[idx];
                if target.endpoint.is_empty()
                    && target.model_path.as_deref().is_some_and(|p| !p.is_empty())
                {
                    return Err(format!(
                        "--backend-serving multiplexer on `{}` is contradictory — an \
                         embedded (model_path) backend serves exactly one artifact \
                         (instance); retarget with --backend-url for a multiplexer",
                        target.name
                    ));
                }
            }
            // Selection PARITY for the unnamed edit: the request itself can
            // reorder the shared precedence (a kind edit adds/removes the
            // prefer-OpenAI property), so re-run the selector over the
            // POST-request view and require it to still pick the edited
            // slot — otherwise the backend the edit landed on and the
            // backend the session then selects would diverge. A
            // destabilizing edit must name its target.
            if over.name.is_none() {
                let mut post: Vec<BackendConfig> = effective.clone();
                over.overlay(&mut post[idx]);
                let post_refs: Vec<&BackendConfig> = post.iter().collect();
                match select_backend_slot(&post_refs, default_backend) {
                    SlotSelection::Slot(i) if i == idx => {}
                    _ => {
                        return Err(format!(
                            "--backend-* would edit `{}` (the currently selected \
                             backend), but the edit changes which backend the shared \
                             precedence selects — name the target explicitly with \
                             --backend-name",
                            self.slots[idx].declaration.name
                        ));
                    }
                }
            }
            self.slots[idx].request = Some(over);
            validate_backend_names(self.slots.iter().map(|s| &s.declaration))?;
            Ok(Some(idx))
        }
    }

    /// Compose the layers: per slot, the effective [`BackendConfig`]
    /// (declaration → retained observation → request) and the
    /// [`BackendResolutionReceipt`], aligned 1:1 by index.
    ///
    /// * A requested destination CHANGE clears the cached observation —
    ///   truth observed at one destination never rides to another; an
    ///   identical requested destination retains it.
    /// * The binding: an explicit `--backend-card` rebinds at the
    ///   post-request destination to the requested-or-DECLARED model (never
    ///   a probed one); otherwise the declared binding stands untouched —
    ///   including under a model-only or endpoint-only request, whose
    ///   visibility is a typed downstream decision, not an erasure here.
    pub(super) fn finish(mut self) -> (Vec<BackendConfig>, Vec<BackendResolutionReceipt>) {
        self.attach_pending_probes();
        let mut backends = Vec::with_capacity(self.slots.len());
        let mut receipts = Vec::with_capacity(self.slots.len());
        for slot in self.slots {
            let declaration = DeclaredBackend::of(&slot.declaration);
            let request = slot.request.as_ref().map(BackendRequest::from_override);
            let destination = request
                .as_ref()
                .map(|r| r.destination_over(&declaration.destination))
                .unwrap_or_else(|| declaration.destination.clone());
            let observation = slot
                .observation
                .filter(|_| destination == declaration.destination);

            let mut backend = slot.declaration;
            if let Some(obs) = &observation {
                overlay_observation(&mut backend, obs);
            }
            if let Some(over) = &slot.request {
                over.overlay(&mut backend);
                // Tier defaulting belongs to the EXCLUSIVE destination
                // request only (a fresh/retargeted backend must actually
                // serve). A field-only edit never invents tiers: an
                // intentionally empty `tiers = []` declaration stays empty.
                let exclusive = over.endpoint.is_some() || over.model_path.is_some();
                if exclusive && backend.tiers.is_empty() {
                    backend.tiers = vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review];
                }
            }
            // Destination/kind coherence — the SAME normalization the
            // targeting preview applies ([`normalize_destination_kind`]).
            // Explicitly CONTRADICTORY requests were rejected in
            // `apply_request`; this normalizes residual declared/probed kind
            // after a destination changed around it.
            normalize_destination_kind(&mut backend);

            let binding = match &request {
                Some(req) if req.card.is_some() => crate::model_card::CardBindingSeed {
                    card: req.card.clone(),
                    bound_model: req.model.clone().or_else(|| declaration.model.clone()),
                    bound_destination: destination.clone(),
                },
                _ => crate::model_card::CardBindingSeed {
                    card: declaration.card.clone(),
                    bound_model: declaration.model.clone(),
                    bound_destination: declaration.destination.clone(),
                },
            };
            receipts.push(BackendResolutionReceipt {
                declaration,
                request,
                observation,
                binding,
            });
            backends.push(backend);
        }
        (backends, receipts)
    }
}

/// CLI-supplied backend override (`newt --backend-*` flags). Each field mirrors
/// an operator-settable [`BackendConfig`] field; `None` means "not set on the
/// command line". Applied LAST in [`Config::resolve`] so it wins over disk
/// drop-ins and localhost discovery — the explicit, per-invocation escape hatch
/// for "use EXACTLY this backend", which no probe write-back or auto-discovery
/// can then override. Set once from the CLI via [`set_cli_backend_override`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BackendOverride {
    /// Backend name (default `"cli"`). Names the exclusive backend, or selects
    /// which existing backend a field-only override targets.
    pub name: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub model_path: Option<String>,
    pub tiers: Option<Vec<Tier>>,
    pub kind: Option<BackendKind>,
    pub api: Option<OpenAiApi>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    pub serving: Option<Serving>,
    pub engine: Option<Engine>,
    pub host: Option<String>,
    pub coexist: Option<bool>,
    pub ram_gib: Option<f64>,
    pub card: Option<String>,
}

impl BackendOverride {
    /// True when no `--backend-*` flag was set (the common case) — [`apply`] is
    /// then a no-op.
    ///
    /// [`apply`]: Self::apply
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Apply to a resolved config — the INFALLIBLE compatibility surface:
    /// delegates to [`BackendOverride::try_apply`] (the one invariant-owning
    /// composer) and, when the request is refused (both/empty destinations,
    /// a contradictory kind, a named backend that does not exist, duplicate
    /// names), warns and leaves the config untouched. It can no longer
    /// violate the XOR/nonempty/shared-selector/named-miss semantics the
    /// assembly enforces.
    pub fn apply(&self, cfg: &mut Config) {
        if let Err(e) = self.try_apply(cfg) {
            tracing::warn!(error = %e, "--backend-* override not applied");
        }
    }

    /// Apply to a resolved config through the SAME backend-assembly path
    /// `Config::resolve_runtime` uses — one composer, one set of
    /// invariants:
    ///
    /// * a destination request (`--backend-url` XOR `--backend-model-path`,
    ///   nonempty, kind-coherent) defines an **exclusive** backend that
    ///   REPLACES all others;
    /// * a field-only request edits the NAMED backend (a name matching
    ///   nothing is an error, never a silent no-op) or, unnamed, the
    ///   backend the shared selection precedence picks — never "index 0";
    /// * destination/kind coherence is normalized exactly as in
    ///   `resolve_runtime` (a model_path route is Embedded; an endpoint
    ///   route never retains Embedded).
    ///
    /// On error the config is byte-for-byte untouched.
    ///
    /// # Errors
    /// Duplicate/empty backend names; both or empty destinations; a
    /// contradictory destination/kind pair; a named or explicitly selected
    /// target that does not exist or cannot be routed.
    pub fn try_apply(&self, cfg: &mut Config) -> std::result::Result<(), String> {
        if self.is_empty() {
            return Ok(());
        }
        let original = cfg.backends.clone();
        let mut assembly = match BackendAssembly::new(std::mem::take(&mut cfg.backends)) {
            Ok(assembly) => assembly,
            Err(e) => {
                cfg.backends = original;
                return Err(e);
            }
        };
        let default_backend = cfg.default_backend.clone();
        let applied = assembly.apply_request(Some(self.clone()), default_backend.as_deref());
        let (backends, _receipts) = assembly.finish();
        match applied {
            Ok(target) => {
                cfg.backends = backends;
                // An explicit `--backend-*` flag is operator configuration —
                // the session is no longer on the bare compiled-in fallback.
                cfg.backend_fallback = false;
                // The one selection-follows-the-request rule, shared with
                // the runtime composers (the binary additionally sets
                // $NEWT_PROVIDER).
                pin_requested_selection(cfg, Some(self), target);
                Ok(())
            }
            Err(e) => {
                cfg.backends = original;
                Err(e)
            }
        }
    }

    /// Copy every set field onto `backend` (leaving unset fields untouched).
    /// A requested destination REPLACES the destination axis whole: both
    /// effective fields are cleared before the requested one installs, so an
    /// HTTP→embedded (or embedded→HTTP) retarget cannot retain the opposite
    /// field and leave the backend pointing two places at once.
    fn overlay(&self, backend: &mut BackendConfig) {
        if self.endpoint.is_some() || self.model_path.is_some() {
            backend.endpoint = String::new();
            backend.model_path = None;
        }
        if let Some(v) = &self.endpoint {
            backend.endpoint = v.clone();
        }
        if let Some(v) = &self.model {
            backend.model = Some(v.clone());
        }
        if let Some(v) = &self.model_path {
            backend.model_path = Some(v.clone());
        }
        if let Some(v) = &self.tiers {
            backend.tiers = v.clone();
        }
        if let Some(v) = self.kind {
            backend.kind = Some(v);
        }
        if let Some(v) = self.api {
            backend.api = Some(v);
        }
        if let Some(v) = &self.api_key_env {
            backend.api_key_env = Some(v.clone());
        }
        if let Some(v) = &self.api_key_file {
            backend.api_key_file = Some(v.clone());
        }
        if let Some(v) = self.serving {
            backend.serving = Some(v);
        }
        if let Some(v) = self.engine {
            backend.engine = Some(v);
        }
        if let Some(v) = &self.host {
            backend.host = Some(v.clone());
        }
        if let Some(v) = self.coexist {
            backend.coexist = Some(v);
        }
        if let Some(v) = self.ram_gib {
            backend.ram_gib = Some(v);
        }
        if let Some(v) = &self.card {
            backend.card = Some(v.clone());
        }
    }
}

/// Process-global CLI backend override, set once from the CLI before any config
/// application. Mirrors the other publishes in
/// [`Config::apply_runtime_settings`] (max_output_tokens, scratch dir): the CLI
/// can't thread a value through every runtime consumer, so it stashes it here
/// and the canonical apply operation installs it last.
static CLI_BACKEND_OVERRIDE: std::sync::Mutex<Option<BackendOverride>> =
    std::sync::Mutex::new(None);

/// Install the CLI backend override (see [`BackendOverride`]). Call once, before
/// the first [`Config::apply_runtime_settings`] call.
pub fn set_cli_backend_override(over: BackendOverride) {
    if let Ok(mut slot) = CLI_BACKEND_OVERRIDE.lock() {
        *slot = Some(over);
    }
}

pub(super) fn cli_backend_override() -> Option<BackendOverride> {
    CLI_BACKEND_OVERRIDE.lock().ok().and_then(|s| s.clone())
}
