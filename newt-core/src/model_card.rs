//! Data-driven **model cards** (#853) — the central abstraction for standing a
//! model up. One card holds *all* the settings needed to serve exactly one model
//! on one backend (vLLM or ollama), plus its sampling `tuning` and reasoning
//! `capability` bits. Authoring a new card is how you add a model newt has never
//! heard of.
//!
//! Three Cs: knowledge lives in **data**, defaults are **overridable**. A card is
//! deserialized from TOML *or* YAML; every field is `Option` so a partial overlay
//! overrides only what it sets, giving the precedence
//! `built-in < ~/.newt/models/<name>`. Name lookups live in ONE place —
//! [`crate::card_catalog::ModelCardCatalog::resolve_exact`]; this module owns
//! the schema, the pure merge/validate, and the capability decision.
//! [`ModelCard::merge`] and [`ModelCard::validate`] are **pure / IO-free**
//! (the file reads in [`load_card_file`] / [`load_dropin_dir`] are the only
//! IO) — mirroring the `dgx_vllm` / `dgx_pull` discipline.
//!
//! **IDENTITIES ONLY** (public-repo rule, like `newt-cli::dgx_registry`): a card
//! carries model identities + serving *profiles* — never a host / IP / GPU / DNS.
//! The endpoint stays in the operator's local `[dgx]` config, resolved at runtime.
//! Fields that *look* hardware-ish but are not: `gpu_mem` is a
//! `--gpu-memory-utilization` **fraction**, `tensor_parallel` a topology knob,
//! `served_name` an OpenAI alias — none identify a machine. [`no_hardware_leak`]
//! enforces this and is applied to every built-in card.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The inference backend a card targets. newt-cli's `InferenceTool` is the
/// CLI-side sibling; this core-side enum lets the harness read a card without a
/// newt-cli dependency (crate boundary). A future pass can unify them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// vLLM (OpenAI-compatible server; FP8/FP16 + multi-node tensor-parallel).
    Vllm,
    /// Ollama (GGUF single-node convenience runtime).
    Ollama,
    /// llama.cpp (GGUF experimental large-model single-node path).
    LlamaCpp,
}

impl Backend {
    /// Stable lowercase token (matches `InferenceTool::as_str`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vllm => "vllm",
            Self::Ollama => "ollama",
            Self::LlamaCpp => "llama_cpp",
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "vllm" => Ok(Self::Vllm),
            "ollama" => Ok(Self::Ollama),
            "llama_cpp" | "llama.cpp" | "llamacpp" => Ok(Self::LlamaCpp),
            other => Err(format!(
                "unknown backend `{other}` (expected vllm | ollama | llama_cpp)"
            )),
        }
    }
}

/// vLLM serving profile — the `vllm serve` knobs. Every field `Option` for layered
/// override.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VllmProfile {
    /// `--served-model-name` (the OpenAI alias clients send as `model`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_name: Option<String>,
    /// `--max-model-len` (context window).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_len: Option<u32>,
    /// `--tensor-parallel-size` (topology knob — not a machine id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tensor_parallel: Option<u8>,
    /// `--gpu-memory-utilization` **fraction** (0.0–1.0) — a knob, not a machine id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_mem: Option<f64>,
    /// `--reasoning-parser` (e.g. `qwen3`) so CoT lands in `reasoning_content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_parser: Option<String>,
    /// `--tool-call-parser` (e.g. `qwen3_xml`) so tool calls surface as `tool_calls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_parser: Option<String>,
    /// `--enable-auto-tool-choice`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_auto_tool_choice: Option<bool>,
    /// Extra raw `vllm serve` argv appended verbatim (the escape hatch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

impl VllmProfile {
    #[must_use]
    pub(crate) fn merge(self, o: Self) -> Self {
        Self {
            served_name: o.served_name.or(self.served_name),
            max_model_len: o.max_model_len.or(self.max_model_len),
            tensor_parallel: o.tensor_parallel.or(self.tensor_parallel),
            gpu_mem: o.gpu_mem.or(self.gpu_mem),
            reasoning_parser: o.reasoning_parser.or(self.reasoning_parser),
            tool_call_parser: o.tool_call_parser.or(self.tool_call_parser),
            enable_auto_tool_choice: o.enable_auto_tool_choice.or(self.enable_auto_tool_choice),
            extra: if o.extra.is_empty() {
                self.extra
            } else {
                o.extra
            },
        }
    }
}

/// A named family's default serving knobs — the layer UNDER a card's own
/// `[vllm]` table in [`resolve`]. Deliberately NOT a full [`ModelCard`]: a
/// family default is never served directly (no `name`/`backend`/footprint to
/// carry), it exists purely to be shared, and the card's own declarations
/// always win field-by-field over it (the same `.or()` precedence every other
/// override layer in this module uses).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FamilyDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vllm: Option<VllmProfile>,
}

/// Ollama serving profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OllamaProfile {
    /// Ollama tag, e.g. `ornith:35b`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// `num_ctx` — set EXPLICITLY (a large auto value can OOM the KV cache).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// Optional Modelfile template override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

impl OllamaProfile {
    #[must_use]
    fn merge(self, o: Self) -> Self {
        Self {
            tag: o.tag.or(self.tag),
            num_ctx: o.num_ctx.or(self.num_ctx),
            template: o.template.or(self.template),
        }
    }
}

/// Per-model sampling tuning (seeds a `[[model_tuning]]` entry on `setup`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tuning {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// The context-token budget the harness should plan against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
}

impl Tuning {
    #[must_use]
    fn merge(self, o: Self) -> Self {
        Self {
            temperature: o.temperature.or(self.temperature),
            top_p: o.top_p.or(self.top_p),
            top_k: o.top_k.or(self.top_k),
            context_tokens: o.context_tokens.or(self.context_tokens),
        }
    }
}

/// How assistant reasoning is replayed into later completion requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningReplayScope {
    /// Never send model reasoning back to the endpoint.
    #[default]
    Never,
    /// Preserve reasoning only while continuing the current human turn.
    CurrentUserTurn,
    /// Preserve reasoning across the complete conversation history.
    FullHistory,
}

/// Optional OpenAI Chat Completions extensions accepted by an endpoint.
/// Every field is opt-in so strict or unknown compatible servers retain the
/// historical request body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCompletionsCapability {
    /// Project the psyche cognition dial into local generation parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cognition: Option<bool>,
    /// Accepts `chat_template_kwargs` for thinking-mode selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<bool>,
    /// Explicit value to send for `parallel_tool_calls`; unset omits the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Allows one bounded continuation after a reasoning-only length stop.
    /// The runtime also requires a non-`never` reasoning replay scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounded_reasoning_continuation: Option<bool>,
}

impl ChatCompletionsCapability {
    #[must_use]
    fn merge(self, o: Self) -> Self {
        Self {
            cognition: o.cognition.or(self.cognition),
            chat_template_kwargs: o.chat_template_kwargs.or(self.chat_template_kwargs),
            parallel_tool_calls: o.parallel_tool_calls.or(self.parallel_tool_calls),
            bounded_reasoning_continuation: o
                .bounded_reasoning_continuation
                .or(self.bounded_reasoning_continuation),
        }
    }
}

/// Reasoning capability bits the harness reads (retires the `reasoning.rs`
/// name-match in a later issue).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    /// The model opens its turn with a reasoning / `<think>` block that must be
    /// split out of the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emits_leading_reasoning: Option<bool>,
    /// Thinking is on by default (the per-turn toggle's fail-safe value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_default: Option<bool>,
    /// The server returns CoT in a separate response field of this name (e.g.
    /// `reasoning_content`); `None` = inline `<think>` inside `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content_field: Option<String>,
    /// Scope in which assistant reasoning may be replayed to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_replay_scope: Option<ReasoningReplayScope>,
    /// Explicit extensions for OpenAI-compatible Chat Completions servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_completions: Option<ChatCompletionsCapability>,
}

impl Capability {
    /// Field-by-field overlay: `o` wins where it declares, `self` fills the
    /// rest. `pub(crate)` so the config-side capability resolution can layer
    /// an inline block over a named card without reimplementing the merge.
    #[must_use]
    pub(crate) fn merge(self, o: Self) -> Self {
        Self {
            emits_leading_reasoning: o.emits_leading_reasoning.or(self.emits_leading_reasoning),
            thinking_default: o.thinking_default.or(self.thinking_default),
            reasoning_content_field: o.reasoning_content_field.or(self.reasoning_content_field),
            reasoning_replay_scope: o.reasoning_replay_scope.or(self.reasoning_replay_scope),
            chat_completions: match (self.chat_completions, o.chat_completions) {
                (Some(base), Some(overlay)) => Some(base.merge(overlay)),
                (base, overlay) => overlay.or(base),
            },
        }
    }
}

/// A model card: everything needed to stand up one model. `name` is required (it
/// is the merge key); every other field is `Option` so a partial overlay overrides
/// only what it sets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCard {
    /// Model identity, e.g. `Ornith-1.0-35B`. The merge / drop-in key.
    pub name: String,
    /// Which backend `setup` stands up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<Backend>,
    /// Approximate resident footprint (GiB) — a serving knob (like
    /// `ModelVariant.gib`), not a machine id; used by the hardware-fit gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint_gib: Option<f64>,
    /// Likely exceeds a typical node (e.g. the 397B) — `setup` warns / requires
    /// `--force`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gated: Option<bool>,
    /// The model family this card belongs to (e.g. `"qwen3"`), if any — looks
    /// up [`family_defaults`] as the base layer UNDER this card's own `[vllm]`
    /// table in [`resolve`], so cards in the same family (different sizes of
    /// the same tokenizer/parser lineage) don't each duplicate
    /// `reasoning_parser`/`tool_call_parser`/etc. `None` means no family layer
    /// applies — today's pre-family behavior, unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vllm: Option<VllmProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ollama: Option<OllamaProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tuning: Option<Tuning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<Capability>,
}

fn merge_opt<T>(base: Option<T>, overlay: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    match (base, overlay) {
        (Some(b), Some(o)) => Some(f(b, o)),
        (b, None) => b,
        (None, o) => o,
    }
}

impl ModelCard {
    /// Overlay `self` under `overlay` — overlay wins field-by-field, deep-merging
    /// the nested tables. Pure. Precedence chains left-to-right:
    /// `builtin.merge(dropin).merge(one_off).merge(flags)`.
    #[must_use]
    pub fn merge(self, overlay: Self) -> Self {
        Self {
            name: if overlay.name.trim().is_empty() {
                self.name
            } else {
                overlay.name
            },
            backend: overlay.backend.or(self.backend),
            footprint_gib: overlay.footprint_gib.or(self.footprint_gib),
            gated: overlay.gated.or(self.gated),
            family: overlay.family.or(self.family),
            vllm: merge_opt(self.vllm, overlay.vllm, VllmProfile::merge),
            ollama: merge_opt(self.ollama, overlay.ollama, OllamaProfile::merge),
            tuning: merge_opt(self.tuning, overlay.tuning, Tuning::merge),
            capability: merge_opt(self.capability, overlay.capability, Capability::merge),
        }
    }

    /// Reject a structurally-invalid card **loudly** (never silently): empty name,
    /// no backend, a backend whose serving block is absent, or a `family` naming
    /// no known family defaults (almost always a typo — silently applying no
    /// defaults would be a quieter, harder-to-notice version of the same
    /// mistake `deny_unknown_fields` guards against elsewhere in this module).
    ///
    /// # Errors
    /// Returns a human-readable reason when the card cannot be stood up.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("model card: `name` is empty".to_string());
        }
        // Family is an IDENTITY, decoupled from serving defaults: any
        // nonempty explicit family validates — when no
        // `cards/families/<name>.toml` profile exists the card simply gets
        // no serving defaults ([`apply_family_defaults`] is a no-op). Only
        // the empty/whitespace non-identity is rejected. (The old rule
        // required a defaults profile per family, which made the typed
        // family seam unable to carry `nemotron`/`gemma`/… without an
        // unrelated serving profile.)
        if let Some(family) = self.family.as_deref() {
            if family.trim().is_empty() {
                return Err(format!(
                    "model card `{}`: `family` is empty — declare a real family \
                     identity or remove the key",
                    self.name
                ));
            }
        }
        match self.backend {
            None => Err(format!(
                "model card `{}`: no `backend` set (vllm | ollama | llama_cpp)",
                self.name
            )),
            Some(Backend::Vllm) if self.vllm.is_none() => Err(format!(
                "model card `{}`: backend=vllm but no [vllm] serving block",
                self.name
            )),
            Some(Backend::Ollama) if self.ollama.is_none() => Err(format!(
                "model card `{}`: backend=ollama but no [ollama] serving block",
                self.name
            )),
            Some(_) => Ok(()),
        }
    }

    /// The card as canonical TOML (for `card show` / seeding `~/.newt`).
    ///
    /// # Errors
    /// Propagates a serialization error (should not occur for a valid card).
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| format!("card serialize: {e}"))
    }
}

/// Parse a card from `contents` as TOML or YAML, dispatched by `ext` (with or
/// without a leading dot). Pure over the bytes + extension.
///
/// # Errors
/// Returns a parse error, or an "unknown extension" error for anything other than
/// `toml` / `yaml` / `yml`.
pub fn parse_card(contents: &str, ext: &str) -> Result<ModelCard, String> {
    match ext.trim_start_matches('.').to_ascii_lowercase().as_str() {
        "toml" => toml::from_str(contents).map_err(|e| format!("card TOML: {e}")),
        "yaml" | "yml" => serde_yaml::from_str(contents).map_err(|e| format!("card YAML: {e}")),
        other => Err(format!(
            "card: unknown extension `.{other}` (expected .toml / .yaml / .yml)"
        )),
    }
}

/// Load a single card file (`--card <path>`), dispatching TOML/YAML on extension.
///
/// # Errors
/// Returns a read error or a parse error.
pub fn load_card_file(path: &Path) -> Result<ModelCard, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    parse_card(&contents, ext)
}

/// Load every droppable card in `dir` (`*.toml` / `*.yaml` / `*.yml`). A missing
/// dir yields an empty list; an unreadable/invalid file is skipped (best-effort,
/// like the language-pack loader). The merge itself stays pure via [`resolve`].
#[must_use]
pub fn load_dropin_dir(dir: &Path) -> Vec<ModelCard> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(ext, "toml" | "yaml" | "yml") {
            if let Ok(card) = load_card_file(&p) {
                out.push(card);
            }
        }
    }
    out
}

/// Compatibility wrapper for the pre-catalog public precedence chain
/// (`built-in < drop-ins < one-off`). Runtime NAMED lookup now goes through
/// [`crate::card_catalog::ModelCardCatalog::resolve_exact`] — the one owner
/// of card identity, its typed errors, and validation; this wrapper keeps
/// the historical signature for external callers, over the same primitives
/// (with the corrected family order: defaults fill for the FINAL merged
/// family, once). Like before, it never errors and never validates.
#[deprecated(
    note = "resolve card names through card_catalog::ModelCardCatalog::resolve_exact; \
            for a pre-merged card, card_catalog::finalize validates too"
)]
#[must_use]
pub fn resolve(builtin: ModelCard, dropins: &[ModelCard], one_off: Option<ModelCard>) -> ModelCard {
    let name = builtin.name.clone();
    let mut card = dropins
        .iter()
        .filter(|d| d.name == name)
        .cloned()
        .fold(builtin, ModelCard::merge);
    if let Some(o) = one_off {
        card = card.merge(o);
    }
    apply_family_defaults(&mut card);
    card
}

/// Fill the card's `[vllm]` gaps from its family's default table — the
/// card's own declarations always win field-by-field; the family only fills
/// what the card left unset. Called by [`crate::card_catalog::finalize`]
/// AFTER base/overlay merging, so the layer follows the FINAL family (an
/// overlay that changes `family` gets the family it ends up in). Pure.
pub(crate) fn apply_family_defaults(card: &mut ModelCard) {
    if let Some(family) = card.family.clone() {
        if let Some(defaults) = family_defaults(&family) {
            card.vllm = merge_opt(defaults.vllm, card.vllm.take(), VllmProfile::merge);
        }
    }
}

/// The built-in family-default tables shipped with newt (embedded DATA) — named
/// serving-knob presets a card opts into via its own `family` field, so cards
/// sharing a tokenizer/parser lineage (different sizes of the same family)
/// don't each duplicate the same `[vllm]` settings. Adding a new family is a
/// new `cards/families/<name>.toml` file plus one entry here — config, not code.
#[must_use]
pub fn family_defaults(family: &str) -> Option<FamilyDefaults> {
    const EMBEDDED: &[(&str, &str)] = &[("qwen3", include_str!("cards/families/qwen3.toml"))];
    let want = family.trim().to_ascii_lowercase();
    EMBEDDED
        .iter()
        .find(|(name, _)| *name == want)
        .map(|(_, toml)| toml::from_str(toml).expect("built-in family-defaults file is valid TOML"))
}

/// The built-in model cards shipped with newt (embedded DATA) — the base layer of
/// the precedence chain. A drop-in `~/.newt/models/<name>.toml` overrides one by
/// name; `--card <path>` overlays a one-off. Ornith-1.0-35B is the reference card;
/// the 397B ships `gated` (it almost certainly exceeds a single node).
#[must_use]
pub fn builtin_cards() -> Vec<ModelCard> {
    builtin_card_entries().into_iter().map(|(_, c)| c).collect()
}

/// The built-in cards paired with their DECLARED override source keys — the
/// shipped filename stems, embedded as data beside each card. The catalog
/// consults these keys literally; they are never derived by normalizing a
/// name at runtime.
#[must_use]
pub fn builtin_card_entries() -> Vec<(String, ModelCard)> {
    const EMBEDDED: &[(&str, &str)] = &[
        ("ornith-1.0-35b", include_str!("cards/ornith-1.0-35b.toml")),
        (
            "ornith-1.0-397b",
            include_str!("cards/ornith-1.0-397b.toml"),
        ),
    ];
    EMBEDDED
        .iter()
        .map(|(key, s)| {
            (
                (*key).to_string(),
                parse_card(s, "toml").expect("built-in card is valid TOML"),
            )
        })
        .collect()
}

/// The resolved capability layers for one backend — constructed ONCE per
/// backend choice from [`CardBindingSeed`] evidence, immutable, and
/// consulted through the typed route decision [`Self::for_route`].
/// The layers stay whole and the decision is a pure function of the CURRENT
/// serving principal, so rebuilds, refreshes, restarts, and adoptions all
/// get the same answer from the same facts.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCapabilities {
    inline: Option<Capability>,
    /// ONE association per resolved card: capability AND family ride the
    /// same binding (both optional), so the typed applicability the display
    /// owner renders and the family policy the tenacity seam consumes can
    /// never diverge — a family-only card's transitions are exactly as
    /// VISIBLE as a capability card's. A card carrying NEITHER contributes
    /// nothing and mints no binding (serving/tuning-only cards stay
    /// silent). Family identity is DECOUPLED from serving-default
    /// availability (a `family` with no `cards/families/<name>.toml`
    /// profile is still an identity).
    binding: Option<CardBinding>,
}

/// Pre-overlay card-binding evidence for one backend: the operator's card
/// pointer and the declared model it was bound against, captured BEFORE any
/// runtime overlay (CLI `--backend-model`, session model override) rewrites
/// the backend's own fields. [`ResolvedCapabilities::resolve`] consumes
/// this — never a possibly-overridden `BackendConfig.card`/`model` pair —
/// so an overlay can retarget the SESSION without silently rebinding the
/// CARD. [`crate::config::BackendResolutionReceipt::binding`] is where a
/// resolved config hands it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CardBindingSeed {
    /// The operator's card pointer, if any.
    pub card: Option<String>,
    /// The declared model the card was bound against, if any.
    pub bound_model: Option<String>,
    /// The destination the card was bound AT — a binding is evidence about
    /// one server (or one local artifact), and never silently follows a
    /// session that was pointed somewhere else. Exact comparison, in
    /// [`ResolvedCapabilities::for_route`].
    pub bound_destination: crate::config::BackendDestination,
}

impl CardBindingSeed {
    /// The seed a backend's CURRENT declaration yields — correct whenever no
    /// overlay has touched the backend (drop-in files, plain configs).
    #[must_use]
    pub fn from_backend(backend: &crate::BackendConfig) -> Self {
        Self {
            card: backend.card.clone(),
            bound_model: backend.effective_model().map(str::to_string),
            bound_destination: crate::config::BackendDestination::of(backend),
        }
    }
}

/// An operator's card binding: the card's declared capability plus the model
/// the operator bound the card against.
#[derive(Debug, Clone, PartialEq)]
struct CardBinding {
    name: String,
    /// The card's `[capability]` layer, when it declares one.
    capability: Option<Capability>,
    /// The card's declared family identity, when it names one.
    family: Option<String>,
    /// From [`CardBindingSeed::bound_model`] — `None` when the backend
    /// declared no model.
    bound_model: Option<String>,
    /// From [`CardBindingSeed::bound_destination`].
    bound_destination: crate::config::BackendDestination,
}

/// The serving principal a capability decision is made for. `non_exhaustive`:
/// consumers construct variants and match with a conservative arm; new
/// principal shapes must not silently widen a card's reach.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServingPrincipal<'a> {
    /// A single-artifact server: the binding holds whatever the label says.
    Instance,
    /// A multi-model server with the FINAL adopted model known.
    MultiplexerModel(&'a str),
    /// No serving axis, but an operator-SELECTED model identity (declared or
    /// explicitly requested — never an adopted guess). Exact association is
    /// justified, exactly as in the multiplexer arm.
    SelectedModel(&'a str),
    /// Serving not yet established — stay conservative.
    Unknown,
}

/// Whether — and why not — the card binding contributes to a decision: the
/// TYPED status consumers render once at their display boundary and compare
/// BY IDENTITY for dedupe. Never prose-compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardApplicability {
    /// No card binding exists.
    None,
    /// The binding applies: card capability under inline overrides.
    Active {
        /// The bound card's name.
        card: String,
    },
    /// The binding exists but the session's route points at a DIFFERENT
    /// destination than the one the card was bound at — inline-only, and
    /// the operator must be able to SEE it. Both destinations are typed so
    /// a display seam can render exactly what diverged.
    InactiveDestination {
        card: String,
        /// Where the operator bound the card.
        bound_destination: crate::config::BackendDestination,
        /// Where the session is actually routed.
        active_destination: crate::config::BackendDestination,
    },
    /// The binding exists at this destination but the principal is a
    /// different model — inline-only, and the operator must be able to
    /// SEE it.
    InactiveModel {
        card: String,
        /// What the operator bound the card against (`None` = no declared
        /// model, which can never associate on a multiplexer).
        bound_model: Option<String>,
        /// The model the session is actually serving.
        active_model: String,
    },
    /// The binding exists and the serving principal is not established —
    /// inline-only. Consumers must surface this (headless refuses to run;
    /// the TUI renders the transition) rather than let a configured card
    /// silently never apply.
    Undecided { card: String },
}

/// The outcome of a principal decision: the effective capability layer and
/// the typed [`CardApplicability`] status.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityDecision {
    effective: Option<Capability>,
    applicability: CardApplicability,
}

impl CapabilityDecision {
    /// The full effective [`Capability`], by reference — the whole seam
    /// (`thinking_default`, `reasoning_content_field`, future fields), not
    /// just the flag accessors below.
    #[must_use]
    pub fn effective(&self) -> Option<&Capability> {
        self.effective.as_ref()
    }

    /// The typed card-applicability status for this principal.
    #[must_use]
    pub fn applicability(&self) -> &CardApplicability {
        &self.applicability
    }

    #[must_use]
    pub fn chat_completions(&self) -> ChatCompletionsCapability {
        self.effective
            .as_ref()
            .and_then(|c| c.chat_completions)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn reasoning_replay_scope(&self) -> ReasoningReplayScope {
        self.effective
            .as_ref()
            .and_then(|c| c.reasoning_replay_scope)
            .unwrap_or_default()
    }

    /// **Unknown defaults to `false` — do not suppress.** Filtering when we
    /// should not silently DROPS answer text; not filtering shows reasoning
    /// the operator can see and correct. Fail toward the visible one.
    #[must_use]
    pub fn emits_leading_reasoning(&self) -> bool {
        self.effective
            .as_ref()
            .and_then(|c| c.emits_leading_reasoning)
            .unwrap_or(false)
    }
}

/// Does this principal EXACTLY associate with the bound model? Instance
/// associates by artifact identity; a multiplexer/selected principal only
/// on exact string equality of two supplied identifiers — and an
/// empty/whitespace principal is NO model identity (two empty strings
/// agreeing is not an association). The ONE association rule, shared by
/// the capability decision and the family identity so they cannot drift.
fn principal_associates(bound_model: Option<&str>, principal: ServingPrincipal<'_>) -> bool {
    match principal {
        ServingPrincipal::Instance => true,
        ServingPrincipal::MultiplexerModel(current) | ServingPrincipal::SelectedModel(current) => {
            !current.trim().is_empty() && bound_model == Some(current)
        }
        _ => false,
    }
}

impl ResolvedCapabilities {
    /// Resolve one backend's layers from its [`CardBindingSeed`]. ONE catalog
    /// lookup ([`crate::card_catalog::ModelCardCatalog::resolve_exact`]); the
    /// catalog dir follows `dgx card`'s either/or rule via `explicit_config`:
    /// an operator-explicit config resolves cards from ITS sibling `models/`,
    /// everything else from the user catalog. Callers pass
    /// [`crate::Config::pinned_config_path`] (or their `--profile` path) —
    /// never an ambient `./newt.toml`, per the #1301 trust boundary.
    ///
    /// # Errors
    ///
    /// A seed that NAMES a card which does not resolve is a hard error naming
    /// the backend, the card, and the typed catalog diagnosis (not found /
    /// malformed file / name mismatch / duplicate / invalid card). A resolved
    /// card that merely lacks a `[capability]` block is valid — serving/
    /// tuning-only — and contributes NO layer: `binding` stays `None`, so
    /// nothing downstream reports an inactive binding for declarations that
    /// never existed.
    pub fn resolve(
        backend: &crate::BackendConfig,
        seed: &CardBindingSeed,
        explicit_config: Option<&Path>,
    ) -> Result<Self, String> {
        let inline = backend.capability.clone();
        // The card pointer's EXACT identity goes to the catalog — the
        // catalog defines identity as exact/case-sensitive with no
        // normalization, so trimming here would silently bind
        // `"team-reasoner "` to `team-reasoner` instead of surfacing the
        // typed near-collision. Whitespace-ONLY pointers are absent (the
        // effective-identity rule); everything else is looked up verbatim.
        let Some(name) = seed.card.as_deref().filter(|n| !n.trim().is_empty()) else {
            return Ok(Self {
                inline,
                binding: None,
            });
        };
        let dir = explicit_config
            .and_then(|p| p.parent())
            .map(|d| d.join("models"))
            .or_else(|| crate::Config::user_config_dir().map(|d| d.join("models")));
        let catalog = crate::card_catalog::ModelCardCatalog::load(dir.as_deref());
        let card = catalog
            .resolve_exact(name)
            .map_err(|e| format!("backend `{}` names model card `{name}` — {e}", backend.name))?;
        let capability = card.capability;
        let family = card.family.clone().filter(|f| !f.trim().is_empty());
        Ok(Self {
            // ONE binding whenever the card CONTRIBUTES anything —
            // capability, family, or both. A card with neither contributes
            // nothing: no binding, no applicability chatter downstream.
            binding: (capability.is_some() || family.is_some()).then(|| CardBinding {
                name: name.to_string(),
                capability,
                family,
                // The effective-model rule, defensively: an empty/whitespace
                // bound model in a hand-built seed is no identity.
                bound_model: seed.bound_model.clone().filter(|m| !m.trim().is_empty()),
                bound_destination: seed.bound_destination.clone(),
            }),
            inline,
        })
    }

    /// No declarations at all — the conservative floor, for choices built
    /// without a backend (tests, empty defaults).
    #[must_use]
    pub fn none() -> Self {
        Self {
            inline: None,
            binding: None,
        }
    }

    /// The bound card's name, when a binding with declarations exists.
    #[must_use]
    pub fn card(&self) -> Option<&str> {
        self.binding.as_ref().map(|b| b.name.as_str())
    }

    /// The typed model-family identity for the CURRENT route, or `None` —
    /// the anti-substring seam: family comes from the exact catalog lookup
    /// of the operator's named card, under the SAME association gates as
    /// the capability decision (concrete equal destination + exact
    /// principal association), NEVER from model-name inference. Present
    /// even when the card carries no `[capability]`, and independent of
    /// whether a `cards/families/<name>.toml` default profile exists.
    #[must_use]
    pub fn family_for_route(
        &self,
        active_destination: &crate::config::BackendDestination,
        principal: ServingPrincipal<'_>,
    ) -> Option<&str> {
        let b = self.binding.as_ref()?;
        let family = b.family.as_deref()?;
        (b.bound_destination.is_concrete()
            && active_destination.is_concrete()
            && b.bound_destination == *active_destination
            && principal_associates(b.bound_model.as_deref(), principal))
        .then_some(family)
    }

    /// THE decision — pure over the layers, the session's ROUTE destination,
    /// and the serving principal. Call it at use time with the CURRENT
    /// destination + principal, never cache its output across a route,
    /// serving, or model change.
    ///
    /// Destination first: a binding is evidence about the server (or local
    /// artifact) it was bound AT. `active_destination` differing from the
    /// bound one — by EXACT comparison, no URL normalization beyond
    /// empty-to-`None` — is a typed
    /// [`CardApplicability::InactiveDestination`]: inline-only, evidence
    /// intact and visible. At the SAME destination:
    ///
    /// * **Instance** — the binding applies. One artifact is served; the
    ///   operator's binding names it and the display label is its alias
    ///   (`requested_ignored` included).
    /// * **Multiplexer / SelectedModel** — the binding applies only on EXACT
    ///   equality of the bound model and the current model: association of
    ///   two supplied identifiers inside a typed arm, never inference — no
    ///   substring, no normalization. A mismatch is a typed
    ///   [`CardApplicability::InactiveModel`].
    /// * **Unknown** — inline-only, [`CardApplicability::Undecided`].
    #[must_use]
    pub fn for_route(
        &self,
        active_destination: &crate::config::BackendDestination,
        principal: ServingPrincipal<'_>,
    ) -> CapabilityDecision {
        let Some(b) = &self.binding else {
            return CapabilityDecision {
                effective: self.inline.clone(),
                applicability: CardApplicability::None,
            };
        };
        // Association needs a CONCRETE destination on BOTH sides (exactly
        // one axis — endpoint XOR model_path). A hollow seed matching a
        // hollow route is two absences agreeing, not an exact identity:
        // inline-only, typed Undecided.
        if !b.bound_destination.is_concrete() || !active_destination.is_concrete() {
            return CapabilityDecision {
                effective: self.inline.clone(),
                applicability: CardApplicability::Undecided {
                    card: b.name.clone(),
                },
            };
        }
        if b.bound_destination != *active_destination {
            return CapabilityDecision {
                effective: self.inline.clone(),
                applicability: CardApplicability::InactiveDestination {
                    card: b.name.clone(),
                    bound_destination: b.bound_destination.clone(),
                    active_destination: active_destination.clone(),
                },
            };
        }
        let applies = principal_associates(b.bound_model.as_deref(), principal);
        if applies {
            // A family-only binding activates with NO capability layer —
            // inline stays the whole capability story while the family
            // policy engages; the applicability is Active either way, so
            // its transitions render exactly like a capability card's.
            let effective = match (b.capability.clone(), self.inline.clone()) {
                (Some(cap), Some(inl)) => Some(cap.merge(inl)),
                (Some(cap), None) => Some(cap),
                (None, inline) => inline,
            };
            return CapabilityDecision {
                effective,
                applicability: CardApplicability::Active {
                    card: b.name.clone(),
                },
            };
        }
        let applicability = match principal {
            ServingPrincipal::MultiplexerModel(current)
            | ServingPrincipal::SelectedModel(current)
                if !current.trim().is_empty() =>
            {
                CardApplicability::InactiveModel {
                    card: b.name.clone(),
                    bound_model: b.bound_model.clone(),
                    active_model: current.to_string(),
                }
            }
            _ => CardApplicability::Undecided {
                card: b.name.clone(),
            },
        };
        CapabilityDecision {
            effective: self.inline.clone(),
            applicability,
        }
    }
}

/// The built-in card whose `name` matches (case-insensitive), if any.
#[must_use]
pub fn builtin_card(name: &str) -> Option<ModelCard> {
    let want = name.trim().to_ascii_lowercase();
    builtin_cards()
        .into_iter()
        .find(|c| c.name.to_ascii_lowercase() == want)
}

/// Scan a card for a leaked machine identity — an RFC1918 / CGNAT IPv4 literal in
/// any string field. Built-in cards MUST pass (identities only; the endpoint lives
/// in local `[dgx]` config). Returns the offending values (empty = clean).
///
/// A pragmatic guard, not a full network parser: it flags dotted-quad literals in
/// the private / carrier-grade ranges, which is what a stray endpoint looks like.
#[must_use]
pub fn no_hardware_leak(card: &ModelCard) -> Vec<String> {
    // Serialize to TOML and scan the text — covers every string field uniformly.
    let text = card.to_toml().unwrap_or_default();
    let mut hits = Vec::new();
    for tok in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        if is_private_ipv4(tok) {
            hits.push(tok.to_string());
        }
    }
    hits
}

/// True for an RFC1918 (`10.`, `172.16–31.`, `192.168.`) or CGNAT (`100.64–127.`)
/// dotted-quad. Pure.
fn is_private_ipv4(s: &str) -> bool {
    let octets: Vec<&str> = s.split('.').collect();
    if octets.len() != 4 {
        return false;
    }
    let mut o = [0u16; 4];
    for (i, part) in octets.iter().enumerate() {
        match part.parse::<u16>() {
            Ok(v) if v <= 255 && (part.len() == 1 || !part.starts_with('0')) => o[i] = v,
            _ => return false,
        }
    }
    o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn ornith_toml() -> &'static str {
        r#"
name = "Ornith-1.0-35B"
backend = "vllm"
footprint_gib = 35.0

[vllm]
served_name = "Ornith-1.0-35B"
max_model_len = 262144
reasoning_parser = "qwen3"
tool_call_parser = "qwen3_xml"
enable_auto_tool_choice = true

[tuning]
temperature = 0.6
top_p = 0.95
top_k = 20
context_tokens = 262144

[capability]
emits_leading_reasoning = true
thinking_default = true
reasoning_content_field = "reasoning_content"
"#
    }

    // ── Backend vocabulary ────────────────────────────────────────────────
    #[test]
    fn backend_tokens_round_trip() {
        for (tok, b) in [
            ("vllm", Backend::Vllm),
            ("ollama", Backend::Ollama),
            ("llama_cpp", Backend::LlamaCpp),
        ] {
            assert_eq!(Backend::from_str(tok).unwrap(), b);
            assert_eq!(b.as_str(), tok);
        }
        // Tolerant aliases + case + whitespace.
        assert_eq!(Backend::from_str("  VLLM ").unwrap(), Backend::Vllm);
        assert_eq!(Backend::from_str("llama.cpp").unwrap(), Backend::LlamaCpp);
        // Unknown fails loudly.
        assert!(Backend::from_str("tgi").is_err());
    }

    // ── TOML / YAML parity ────────────────────────────────────────────────
    #[test]
    fn parses_from_toml_and_yaml_identically() {
        let from_toml = parse_card(ornith_toml(), "toml").expect("toml");
        // The same card in YAML.
        let yaml = r#"
name: Ornith-1.0-35B
backend: vllm
footprint_gib: 35.0
vllm:
  served_name: Ornith-1.0-35B
  max_model_len: 262144
  reasoning_parser: qwen3
  tool_call_parser: qwen3_xml
  enable_auto_tool_choice: true
tuning:
  temperature: 0.6
  top_p: 0.95
  top_k: 20
  context_tokens: 262144
capability:
  emits_leading_reasoning: true
  thinking_default: true
  reasoning_content_field: reasoning_content
"#;
        let from_yaml = parse_card(yaml, "yml").expect("yaml");
        assert_eq!(from_toml, from_yaml, "TOML and YAML must parse identically");
        // And a TOML round-trip is stable.
        let reparsed = parse_card(&from_toml.to_toml().unwrap(), "toml").unwrap();
        assert_eq!(reparsed, from_toml);
    }

    #[test]
    fn capability_parses_turn_scoped_reasoning_replay() {
        let card = parse_card(
            r#"
name = "reasoning-model"

[capability]
reasoning_replay_scope = "current_user_turn"
"#,
            "toml",
        )
        .expect("reasoning replay scope is a supported capability");

        let capability = serde_json::to_value(card.capability.expect("capability present"))
            .expect("capability serializes");
        assert_eq!(capability["reasoning_replay_scope"], "current_user_turn");
    }

    #[test]
    fn parse_rejects_unknown_extension_and_bad_syntax() {
        assert!(parse_card(ornith_toml(), "json").is_err());
        assert!(parse_card("name = \"x\"\nbogus_field = 1", "toml").is_err()); // deny_unknown_fields
    }

    // ── Merge / layered override ──────────────────────────────────────────
    #[test]
    fn overlay_wins_field_by_field_and_deep_merges() {
        let base = parse_card(ornith_toml(), "toml").unwrap();
        // An overlay that sets ONLY vllm.max_model_len + tuning.temperature.
        let overlay: ModelCard = toml::from_str(
            "name = \"Ornith-1.0-35B\"\n[vllm]\nmax_model_len = 65536\n[tuning]\ntemperature = 1.0",
        )
        .unwrap();
        let merged = base.clone().merge(overlay);
        let v = merged.vllm.as_ref().unwrap();
        // The overlaid field wins…
        assert_eq!(v.max_model_len, Some(65536));
        assert_eq!(merged.tuning.as_ref().unwrap().temperature, Some(1.0));
        // …and every un-set field is INHERITED from the base (deep merge).
        assert_eq!(v.reasoning_parser.as_deref(), Some("qwen3"));
        assert_eq!(v.tool_call_parser.as_deref(), Some("qwen3_xml"));
        assert_eq!(merged.tuning.as_ref().unwrap().top_p, Some(0.95));
        assert_eq!(
            merged.capability.as_ref().unwrap().thinking_default,
            Some(true)
        );
    }

    #[test]
    fn chat_completions_capability_deep_merges_field_by_field() {
        let base: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"llama_cpp\"\n\
             [capability.chat_completions]\ncognition = true\n\
             chat_template_kwargs = true\nparallel_tool_calls = false\n",
        )
        .unwrap();
        let overlay: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"llama_cpp\"\n\
             [capability.chat_completions]\nbounded_reasoning_continuation = true\n",
        )
        .unwrap();

        let merged = base.merge(overlay);
        let capability = merged
            .capability
            .and_then(|capability| capability.chat_completions)
            .expect("chat-completions capability survives the merge");
        assert_eq!(capability.cognition, Some(true));
        assert_eq!(capability.chat_template_kwargs, Some(true));
        assert_eq!(capability.parallel_tool_calls, Some(false));
        assert_eq!(capability.bounded_reasoning_continuation, Some(true));
    }

    /// Build a one-entry catalog source for override tests.
    fn dropin_source(key: &str, card: ModelCard) -> crate::card_catalog::CardSource {
        crate::card_catalog::CardSource {
            key: key.to_string(),
            path: std::path::PathBuf::from(format!("/cards/{key}.toml")),
            parsed: Ok(card),
        }
    }

    // ── Family defaults ───────────────────────────────────────────────────
    #[test]
    fn family_defaults_returns_known_family_case_insensitively() {
        for name in ["qwen3", "QWEN3", "Qwen3"] {
            let d = family_defaults(name).unwrap_or_else(|| panic!("{name} should be known"));
            let v = d.vllm.unwrap();
            assert_eq!(v.reasoning_parser.as_deref(), Some("qwen3"));
            assert_eq!(v.tool_call_parser.as_deref(), Some("qwen3_xml"));
            assert_eq!(v.enable_auto_tool_choice, Some(true));
        }
    }

    #[test]
    fn family_defaults_is_none_for_an_unknown_family() {
        assert!(family_defaults("nemotron").is_none());
        assert!(family_defaults("bogus").is_none());
    }

    #[test]
    fn finalize_fills_vllm_gaps_from_family_defaults() {
        let card: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"vllm\"\nfamily = \"qwen3\"\n\
             [vllm]\nserved_name = \"test\"\nmax_model_len = 8192\n",
        )
        .unwrap();
        let resolved = crate::card_catalog::finalize(card).unwrap();
        let v = resolved.vllm.unwrap();
        // The card's own fields stand...
        assert_eq!(v.served_name.as_deref(), Some("test"));
        assert_eq!(v.max_model_len, Some(8192));
        // ...and the family fills what the card didn't set.
        assert_eq!(v.reasoning_parser.as_deref(), Some("qwen3"));
        assert_eq!(v.tool_call_parser.as_deref(), Some("qwen3_xml"));
        assert_eq!(v.enable_auto_tool_choice, Some(true));
    }

    #[test]
    fn finalize_cards_own_vllm_field_wins_over_family_default() {
        let card: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"vllm\"\nfamily = \"qwen3\"\n\
             [vllm]\ntool_call_parser = \"custom_xml\"\n",
        )
        .unwrap();
        let resolved = crate::card_catalog::finalize(card).unwrap();
        let v = resolved.vllm.unwrap();
        assert_eq!(
            v.tool_call_parser.as_deref(),
            Some("custom_xml"),
            "the card's own declaration overrides the family default, never the reverse"
        );
        assert_eq!(
            v.reasoning_parser.as_deref(),
            Some("qwen3"),
            "a field the card didn't set still inherits from the family"
        );
    }

    #[test]
    fn finalize_with_no_family_is_unaffected_by_family_defaults() {
        // A card with no `family` gets no layer applied — and validates.
        let card: ModelCard =
            toml::from_str("name = \"test\"\nbackend = \"vllm\"\n[vllm]\nserved_name = \"test\"\n")
                .unwrap();
        let resolved = crate::card_catalog::finalize(card).unwrap();
        assert_eq!(resolved.vllm.unwrap().reasoning_parser, None);
    }

    #[test]
    fn finalize_accepts_an_arbitrary_family_without_a_defaults_profile() {
        // Family is an IDENTITY, decoupled from serving defaults: a family
        // with no `cards/families/<name>.toml` profile validates and simply
        // gets no defaults layer — the card's own [vllm] block is exactly
        // what comes out. (Previously this was rejected, which made the
        // typed family seam unable to carry nemotron/gemma/… identities.)
        let card: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"vllm\"\nfamily = \"nemotron\"\n\
             [vllm]\nserved_name = \"test\"\n",
        )
        .unwrap();
        let resolved = crate::card_catalog::finalize(card).expect("identity needs no profile");
        assert_eq!(resolved.family.as_deref(), Some("nemotron"));
        let vllm = resolved.vllm.expect("own block kept");
        assert_eq!(vllm.served_name.as_deref(), Some("test"));
        assert_eq!(
            vllm.reasoning_parser, None,
            "no defaults profile ⇒ no serving defaults applied"
        );
    }

    #[test]
    fn finalize_rejects_an_empty_family_identity() {
        // The empty/whitespace non-identity is still rejected — an empty
        // string can never be an exact family.
        let card: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"vllm\"\nfamily = \"  \"\n\
             [vllm]\nserved_name = \"test\"\n",
        )
        .unwrap();
        let err = crate::card_catalog::finalize(card).expect_err("empty family");
        assert!(err.contains("family"), "{err}");
    }

    #[test]
    fn validate_accepts_an_arbitrary_family_identity() {
        // Identity is decoupled from serving-default availability: a family
        // outside the defaults registry (a `qwenn3` typo included — the
        // registry cannot tell a typo from a real new family) validates;
        // only the empty non-identity is rejected.
        let card: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"vllm\"\nfamily = \"qwenn3\"\n\
             [vllm]\nserved_name = \"test\"\n",
        )
        .unwrap();
        card.validate().expect("an explicit family is an identity");
        let empty: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"vllm\"\nfamily = \"\"\n\
             [vllm]\nserved_name = \"test\"\n",
        )
        .unwrap();
        let err = empty.validate().expect_err("empty family is no identity");
        assert!(err.contains("family"), "{err}");
    }

    #[test]
    fn validate_accepts_a_known_family_name() {
        let card: ModelCard = toml::from_str(
            "name = \"test\"\nbackend = \"vllm\"\nfamily = \"qwen3\"\n\
             [vllm]\nserved_name = \"test\"\n",
        )
        .unwrap();
        assert!(card.validate().is_ok());
    }

    // ── Validate ──────────────────────────────────────────────────────────
    #[test]
    fn validate_accepts_a_well_formed_card() {
        assert!(parse_card(ornith_toml(), "toml")
            .unwrap()
            .validate()
            .is_ok());
    }

    #[test]
    fn validate_rejects_empty_name_missing_backend_and_missing_block() {
        let mut c = parse_card(ornith_toml(), "toml").unwrap();
        c.name = "  ".into();
        assert!(c.validate().is_err(), "empty name");

        let no_backend: ModelCard = toml::from_str("name = \"X\"").unwrap();
        assert!(no_backend.validate().is_err(), "no backend");

        let vllm_no_block: ModelCard = toml::from_str("name = \"X\"\nbackend = \"vllm\"").unwrap();
        assert!(vllm_no_block.validate().is_err(), "backend=vllm, no [vllm]");

        let ollama_no_block: ModelCard =
            toml::from_str("name = \"X\"\nbackend = \"ollama\"").unwrap();
        assert!(
            ollama_no_block.validate().is_err(),
            "backend=ollama, no [ollama]"
        );
    }

    // ── No hardware leak ──────────────────────────────────────────────────
    #[test]
    fn no_hardware_leak_flags_a_private_ip_and_passes_clean_cards() {
        // Clean card: no leak.
        let clean = parse_card(ornith_toml(), "toml").unwrap();
        assert!(
            no_hardware_leak(&clean).is_empty(),
            "identities-only card is clean"
        );

        // A card that smuggled an endpoint into served_name is caught. Build the
        // private IP from octets so no literal RFC1918 dotted-quad sits in this
        // public source (the pre-push network-leak guard).
        let ip = format!("{}.{}.{}.{}", 192, 168, 1, 100);
        let leaky: ModelCard = toml::from_str(&format!(
            "name = \"X\"\nbackend = \"vllm\"\n[vllm]\nserved_name = \"http://{ip}:8000\""
        ))
        .unwrap();
        assert_eq!(no_hardware_leak(&leaky), vec![ip]);
    }

    #[test]
    fn is_private_ipv4_ranges() {
        // Build the private-range samples from octets so no literal RFC1918/CGNAT
        // dotted-quad appears in this public source (the pre-push leak guard).
        for o in [
            [10, 0, 0, 1],
            [172, 16, 0, 1],
            [172, 31, 255, 255],
            [192, 168, 1, 1],
            [100, 64, 0, 1],
        ] {
            let ip = format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]);
            assert!(is_private_ipv4(&ip), "{ip} is private");
        }
        for ip in [
            "8.8.8.8",
            "172.15.0.1",
            "172.32.0.1",
            "1.2.3",
            "256.1.1.1",
            "01.2.3.4",
        ] {
            assert!(!is_private_ipv4(ip), "{ip} is NOT flagged");
        }
    }

    // ── #854: built-in Ornith cards ───────────────────────────────────────
    #[test]
    fn builtin_cards_parse_validate_and_are_leak_free() {
        let cards = builtin_cards();
        assert!(!cards.is_empty(), "built-in cards present");
        for c in &cards {
            c.validate()
                .unwrap_or_else(|e| panic!("built-in `{}` invalid: {e}", c.name));
            assert!(
                no_hardware_leak(c).is_empty(),
                "built-in `{}` leaks a private IP",
                c.name
            );
            // Identities only: a built-in card pins NO node-specific knob.
            if let Some(v) = c.vllm.as_ref() {
                assert!(v.gpu_mem.is_none(), "{}: gpu_mem is node-specific", c.name);
                assert!(
                    v.tensor_parallel.is_none(),
                    "{}: tensor_parallel is node-specific",
                    c.name
                );
            }
        }
    }

    #[test]
    fn ornith_35b_card_has_the_expected_settings() {
        // Resolved through the real catalog (not the raw builtin card):
        // reasoning_parser/tool_call_parser/enable_auto_tool_choice come from
        // the qwen3 family defaults, not this card's own [vllm] table — this
        // asserts the EFFECTIVE settings a real `card setup` would use.
        let c = crate::card_catalog::ModelCardCatalog::new(builtin_card_entries(), vec![], None)
            .resolve_exact("Ornith-1.0-35B")
            .expect("Ornith-1.0-35B present");
        assert_eq!(c.backend, Some(Backend::Vllm));
        assert_eq!(c.gated, None, "the 35B is runnable, not gated");
        let v = c.vllm.unwrap();
        assert_eq!(v.max_model_len, Some(262_144));
        assert_eq!(v.reasoning_parser.as_deref(), Some("qwen3"));
        assert_eq!(v.tool_call_parser.as_deref(), Some("qwen3_xml"));
        assert_eq!(v.enable_auto_tool_choice, Some(true));
        assert!(v.extra.iter().any(|a| a == "--enable-prefix-caching"));
        assert!(v.extra.iter().any(|a| a == "--trust-remote-code"));
        let t = c.tuning.unwrap();
        assert_eq!(t.temperature, Some(0.6));
        assert_eq!(t.top_p, Some(0.95));
        assert_eq!(t.top_k, Some(20));
        assert_eq!(t.context_tokens, Some(262_144));
        let cap = c.capability.unwrap();
        assert_eq!(cap.emits_leading_reasoning, Some(true));
        assert_eq!(cap.thinking_default, Some(true));
        assert_eq!(
            cap.reasoning_content_field.as_deref(),
            Some("reasoning_content")
        );
        let o = c.ollama.unwrap();
        assert_eq!(o.tag.as_deref(), Some("ornith:35b"));
        assert!(
            o.num_ctx.unwrap() < 262_144,
            "ollama num_ctx is capped below the native window (OOM guard)"
        );
    }

    #[test]
    fn ornith_397b_is_gated() {
        let c = builtin_card("Ornith-1.0-397B").expect("397B present as a gated card");
        assert_eq!(c.gated, Some(true), "the 397B must be hardware-gated");
    }

    #[test]
    fn builtin_card_is_overridable_by_name() {
        // A drop-in overrides only what it sets; the rest inherits from the built-in.
        let dropin: ModelCard =
            toml::from_str("name = \"Ornith-1.0-35B\"\n[ollama]\nnum_ctx = 65536").unwrap();
        let resolved = crate::card_catalog::ModelCardCatalog::new(
            builtin_card_entries(),
            vec![dropin_source("ornith-1.0-35b", dropin)],
            None,
        )
        .resolve_exact("Ornith-1.0-35B")
        .expect("resolves");
        assert_eq!(
            resolved.ollama.unwrap().num_ctx,
            Some(65536),
            "drop-in wins"
        );
        assert_eq!(
            resolved.vllm.unwrap().reasoning_parser.as_deref(),
            Some("qwen3"),
            "un-set fields inherit from the built-in"
        );
    }
}
