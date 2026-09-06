//! Context-management presets, feature overrides, and compaction budgets.

use serde::{Deserialize, Serialize};

use super::{ApiSurfaceConfig, BackendKind, Config, SemanticConfig};

/// Context-management strategy — the `[context] manager` key and the
/// `/context manager <name>` command (Step 24.8, #559). `standard` is the
/// current prune → summary → static-marker pipeline. `progressive` and
/// `distributed` are the retrievable-card managers **owned by #546** and not
/// yet available — selecting them reports that and stays on `standard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
// kebab-case, NOT lowercase: serde's `lowercase` rule is a plain
// `to_ascii_lowercase()` of the identifier, which would name `AppendOnly`
// "appendonly" while `keyword()`, the `/context manager` selector and the docs
// all say "append-only" — making the documented spelling a hard config-load
// failure. `standard` / `progressive` / `distributed` are byte-identical under
// either rule, so this is not a compatibility break for existing configs.
#[serde(rename_all = "kebab-case")]
pub enum ContextManager {
    /// Prune → summarize → static-marker (today's behavior). The only one
    /// implemented; the selector seam for the others.
    #[default]
    Standard,
    /// **Never rewrite history.** No summarization, no structural pruning of
    /// prior messages: this preset removes compaction as a source of
    /// prompt-prefix churn, so whatever prefix the harness emits stays stable
    /// turn over turn and provider caching can hold. Oversized material is
    /// capped where it is *produced* (tool-result caps, paginated reads,
    /// offload). A request that will not fit an **authoritative** ceiling is
    /// refused rather than rewritten; softer triggers dispatch as-is, since
    /// dispatching rewrites nothing either.
    ///
    /// The preset governs compaction, not every byte of the request: a caller
    /// that regenerates a volatile system prompt each turn still invalidates the
    /// cache on its own, and that is the caller's to fix — not something this
    /// preset can promise away.
    ///
    /// This is the strategy `mini-swe-agent` uses, and it is a real answer, not
    /// a degenerate one: it scores >74% on SWE-bench Verified with no context
    /// management at all. It trades recall for fidelity — nothing is silently
    /// altered, because nothing is altered.
    AppendOnly,
    /// Leave a lookup marker; retrieve cards on demand (ephemeral → local DB).
    /// Owned by #546 — not yet available.
    Progressive,
    /// Agent-mesh-shared card store across a swarm. Owned by #546 — not yet
    /// available.
    Distributed,
}

impl ContextManager {
    /// Parse a CLI/config/command keyword (case-insensitive).
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standard" => Some(Self::Standard),
            "append-only" | "append_only" | "appendonly" | "append" => Some(Self::AppendOnly),
            "progressive" => Some(Self::Progressive),
            "distributed" => Some(Self::Distributed),
            _ => None,
        }
    }

    /// The canonical keyword. Round-trips through BOTH `from_keyword` and serde
    /// — the `kebab-case` rename on the enum is what keeps the second half of
    /// that promise once a variant's name is more than one word.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::AppendOnly => "append-only",
            Self::Progressive => "progressive",
            Self::Distributed => "distributed",
        }
    }

    /// Whether this manager is implemented. Only `standard` today; the others
    /// are owned by #546 (the selector reports "not yet available").
    pub fn available(self) -> bool {
        matches!(self, Self::Standard | Self::AppendOnly)
    }

    /// Whether this preset may rewrite messages already in the transcript.
    ///
    /// `false` for [`AppendOnly`](Self::AppendOnly), which is the whole point of
    /// it: no summarization, no structural pruning of prior turns, so the prompt
    /// prefix is byte-identical turn over turn and provider caching is optimal by
    /// construction. Everything else rewrites, and pays the cache cost for recall.
    pub fn rewrites_history(self) -> bool {
        !matches!(self, Self::AppendOnly)
    }

    /// The default feature bundle this preset turns on (Phase 26, #588). A
    /// preset is a named bundle of composable [`ContextFeature`]s; config and
    /// `/context feature` overrides layer on top (see [`ContextFeatures`]).
    /// Every preset currently resolves to the all-off baseline (today's
    /// `standard` behavior) because no composable feature is implemented yet;
    /// presets gain features as 26.3–26.6 land.
    pub fn base_features(self) -> ContextFeatureSet {
        ContextFeatureSet::default()
    }
}

/// Automatic compaction trigger policy — the `[context]
/// compaction_trigger_policy` key.
///
/// The default is [`HeadroomAware`](Self::HeadroomAware): a message-count
/// threshold is a fallback when Newt does not know the usable input ceiling,
/// rather than a reason to compact a roomy, known context window. Explicit
/// token/send-budget pressure continues to trigger compaction under either
/// policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTriggerPolicy {
    /// Defer a count-only compaction when an authoritative input ceiling is
    /// known. This is the safe default for large-context models.
    #[default]
    HeadroomAware,
    /// Preserve the legacy behavior: compact whenever the message count
    /// exceeds its threshold, even if authoritative input headroom remains.
    MessageCount,
}

impl CompactionTriggerPolicy {
    /// Parse a CLI/config/command keyword (case-insensitive).
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "headroom_aware" => Some(Self::HeadroomAware),
            "message_count" => Some(Self::MessageCount),
            _ => None,
        }
    }

    /// The canonical snake-case keyword (round-trips `from_keyword` + serde).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeadroomAware => "headroom_aware",
            Self::MessageCount => "message_count",
        }
    }

    /// Alias for [`Self::as_str`], matching the existing context selector API.
    pub const fn keyword(self) -> &'static str {
        self.as_str()
    }
}

/// The compaction trigger policy this session resolves to: the operator's
/// session override, else `[context] compaction_trigger_policy`, else the
/// default.
///
/// # Why this exists (#2009 PR7)
///
/// The override was `compaction_trigger_policy_override`, a `run_chat` LOCAL —
/// §5's precondition, the same one `/markdown` and `/mode` hit: **a receipt
/// writer cannot read a local.** `settings_form::apply` is a pure function and
/// has no view into the session loop.
///
/// Deliberately the same shape as `session_markdown_mode` and
/// `session_operating_mode`, under the same #1850 lock. A third spelling of
/// "session override, else config, else default" is how the three come to
/// disagree.
#[must_use]
pub fn session_compaction_trigger_policy() -> CompactionTriggerPolicy {
    if let Some(policy) = std::env::var("NEWT_COMPACTION_TRIGGER")
        .ok()
        .as_deref()
        .and_then(CompactionTriggerPolicy::from_keyword)
    {
        return policy;
    }
    Config::resolve()
        .ok()
        .and_then(|c| c.context)
        .map(|c| c.compaction_trigger_policy)
        .unwrap_or_default()
}

/// Whether the operator pinned the policy this session, as opposed to
/// inheriting it. `/context` reports which, and a receipt's from→to is
/// meaningless without it.
#[must_use]
pub fn compaction_trigger_is_session_pinned() -> bool {
    std::env::var("NEWT_COMPACTION_TRIGGER")
        .ok()
        .as_deref()
        .and_then(CompactionTriggerPolicy::from_keyword)
        .is_some()
}

/// A composable context-management feature (Phase 26, #588) — an independent
/// on/off technique under `[context.features]` and the `/context feature <name>
/// on|off` command. None are implemented yet (`available()` is false for all);
/// they land in 26.3–26.6 and report "not yet available" until then (same
/// pattern as the `ContextManager` presets under #546).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextFeature {
    /// Cap oversized tool outputs; spill the full payload to a re-readable store (#584).
    ToolOffload,
    /// Structured `<state>` store mutated via tools, kept out of the log (#583).
    Scratchpad,
    /// Structure-aligned retrieval of repo evidence (#582).
    Semantic,
    /// Provenance-preserving compaction with retrievable handles (#584).
    Provenance,
    /// Write-gated cross-task experience memory (#585).
    Experiential,
    /// Per-step compiled context view instead of a rolling buffer (#586).
    Scheduled,
}

impl ContextFeature {
    /// Every feature, in display order.
    pub const ALL: [Self; 6] = [
        Self::ToolOffload,
        Self::Scratchpad,
        Self::Semantic,
        Self::Provenance,
        Self::Experiential,
        Self::Scheduled,
    ];

    /// Parse a keyword (case-insensitive; `-`/`_` interchangeable; short aliases).
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "tool_offload" | "tooloffload" | "offload" => Some(Self::ToolOffload),
            "scratchpad" | "state" => Some(Self::Scratchpad),
            "semantic" | "retrieval" => Some(Self::Semantic),
            "provenance" | "handles" => Some(Self::Provenance),
            "experiential" | "experience" => Some(Self::Experiential),
            "scheduled" | "compiled" => Some(Self::Scheduled),
            _ => None,
        }
    }

    /// The canonical lowercase keyword (matches the `[context.features]` key).
    pub fn keyword(self) -> &'static str {
        match self {
            Self::ToolOffload => "tool_offload",
            Self::Scratchpad => "scratchpad",
            Self::Semantic => "semantic",
            Self::Provenance => "provenance",
            Self::Experiential => "experiential",
            Self::Scheduled => "scheduled",
        }
    }

    /// Whether this feature is implemented yet. Flips true per feature as it
    /// lands (26.3–26.6). `tool_offload` 26.3 (#584); `scratchpad` 26.4 (#583);
    /// `semantic` 26.5 (#582); `experiential` 26.6a (#585); `scheduled` 26.6b
    /// (#586). Only `provenance` (#584, a later compaction-handle feature) is
    /// still pending.
    pub fn available(self) -> bool {
        match self {
            Self::ToolOffload
            | Self::Scratchpad
            | Self::Semantic
            | Self::Experiential
            | Self::Scheduled => true,
            Self::Provenance => false,
        }
    }

    /// The tracking issue for this feature (cited in "not yet available").
    pub fn issue(self) -> u32 {
        match self {
            Self::ToolOffload | Self::Provenance => 584,
            Self::Scratchpad => 583,
            Self::Semantic => 582,
            Self::Experiential => 585,
            Self::Scheduled => 586,
        }
    }
}

/// The resolved on/off state of every context feature (Phase 26, #588) — the
/// effective set after a `manager` preset's defaults and config/session
/// overrides are applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextFeatureSet {
    pub tool_offload: bool,
    pub scratchpad: bool,
    pub semantic: bool,
    pub provenance: bool,
    pub experiential: bool,
    pub scheduled: bool,
}

impl ContextFeatureSet {
    /// Read one feature's resolved state.
    pub fn get(&self, f: ContextFeature) -> bool {
        match f {
            ContextFeature::ToolOffload => self.tool_offload,
            ContextFeature::Scratchpad => self.scratchpad,
            ContextFeature::Semantic => self.semantic,
            ContextFeature::Provenance => self.provenance,
            ContextFeature::Experiential => self.experiential,
            ContextFeature::Scheduled => self.scheduled,
        }
    }

    /// Set one feature's resolved state.
    pub fn set(&mut self, f: ContextFeature, on: bool) {
        match f {
            ContextFeature::ToolOffload => self.tool_offload = on,
            ContextFeature::Scratchpad => self.scratchpad = on,
            ContextFeature::Semantic => self.semantic = on,
            ContextFeature::Provenance => self.provenance = on,
            ContextFeature::Experiential => self.experiential = on,
            ContextFeature::Scheduled => self.scheduled = on,
        }
    }

    /// The features currently on, in display order.
    pub fn enabled(self) -> Vec<ContextFeature> {
        ContextFeature::ALL
            .into_iter()
            .filter(|&f| self.get(f))
            .collect()
    }

    /// The base feature set *before* `[context.features]` / session overrides:
    /// the `manager` preset's bundle, with **every available context-management
    /// feature defaulted ON** (#727). The one exception is `provenance`, which
    /// is not yet implemented (`ContextFeature::available()` is `false`) and so
    /// stays OFF until it lands. This makes the full context toolkit — tool
    /// offload, the `<state>`/`<plan>` scratchpad ledger, semantic retrieval,
    /// experiential memory, and the scheduled per-step view — the default on
    /// EVERY backend rather than only local (`Ollama`) ones. Cloud endpoints
    /// (hosted `Openai`-protocol endpoints generally) can't
    /// auto-discover a context window, so they benefit most from the ledger and
    /// retrieval being on by default. Explicit overrides still win — they layer
    /// on top via [`ContextFeatures::apply_to`], so a user can turn any feature
    /// back off in `[context.features]` or with `/context feature <name> off`.
    ///
    /// `kind` is retained for signature stability and future backend-specific
    /// tuning; defaults no longer branch on it.
    pub fn base_for(manager: ContextManager, _kind: BackendKind) -> Self {
        let mut base = manager.base_features();
        for f in ContextFeature::ALL {
            if f != ContextFeature::Provenance && f.available() {
                base.set(f, true);
            }
        }
        base
    }
}

/// Per-feature overrides under `[context.features]` (config) and `/context
/// feature` (session) (Phase 26, #588). `None` inherits the `manager` preset's
/// default; `Some(b)` forces the feature on/off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFeatures {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_offload: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratchpad: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiential: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<bool>,
}

impl ContextFeatures {
    /// Read one feature's override (`None` = inherit the preset).
    pub fn get(&self, f: ContextFeature) -> Option<bool> {
        match f {
            ContextFeature::ToolOffload => self.tool_offload,
            ContextFeature::Scratchpad => self.scratchpad,
            ContextFeature::Semantic => self.semantic,
            ContextFeature::Provenance => self.provenance,
            ContextFeature::Experiential => self.experiential,
            ContextFeature::Scheduled => self.scheduled,
        }
    }

    /// Set one feature's override.
    pub fn set(&mut self, f: ContextFeature, v: Option<bool>) {
        match f {
            ContextFeature::ToolOffload => self.tool_offload = v,
            ContextFeature::Scratchpad => self.scratchpad = v,
            ContextFeature::Semantic => self.semantic = v,
            ContextFeature::Provenance => self.provenance = v,
            ContextFeature::Experiential => self.experiential = v,
            ContextFeature::Scheduled => self.scheduled = v,
        }
    }

    /// Layer these overrides onto a base set (a preset's defaults).
    pub fn apply_to(&self, mut base: ContextFeatureSet) -> ContextFeatureSet {
        for f in ContextFeature::ALL {
            if let Some(v) = self.get(f) {
                base.set(f, v);
            }
        }
        base
    }
}

/// `[context]` config section (Step 24.8, #559; features added Phase 26, #588).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Context-management strategy preset. Default `standard`.
    #[serde(default)]
    pub manager: ContextManager,

    /// Policy for automatic compaction triggers. Default `headroom_aware`:
    /// message count alone only triggers when Newt lacks an authoritative
    /// input ceiling; token and send-budget pressure always remain active.
    #[serde(default)]
    pub compaction_trigger_policy: CompactionTriggerPolicy,

    /// Per-feature overrides under `[context.features]` — each inherits the
    /// `manager` preset's default unless explicitly set (Phase 26, #588).
    #[serde(default)]
    pub features: ContextFeatures,

    /// `[context.semantic]` — settings for the `semantic` RAG feature (Step
    /// 26.5, #582).
    #[serde(default)]
    pub semantic: SemanticConfig,

    /// `[context.estimation]` — the cheap token-estimation heuristic
    /// (`chars_per_token`, default 4). Threaded through the estimators; the
    /// per-model calibration ratio scales the result on top.
    #[serde(default)]
    pub estimation: crate::tokens::TokenEstimation,

    /// Floor (chars) for the whole-middle summarizer input cap. The cap is
    /// normally the compression budget converted to chars, but a tight budget
    /// would starve the summarizer of material — never give it less than this.
    #[serde(default = "default_summary_input_cap_floor_chars")]
    pub summary_input_cap_floor_chars: usize,

    /// `[context.api_surface]` — the workspace-API-surface knowledge_base
    /// technique + its pluggable language packs (#669).
    #[serde(default)]
    pub api_surface: ApiSurfaceConfig,

    /// Percentage bound on how much of a request's `num_ctx` window may be
    /// input before the pre-send gate trims. The effective input ceiling is
    /// the tighter of this bound and the room left by the active maximum
    /// output. The historical hardcoded value was 80. Large-window models can
    /// safely run this higher to pack more input when the output reserve is not
    /// already tighter. Normalized to `1..=99`; anything outside falls back to
    /// 80. See `num_ctx_input_ceiling` (#282).
    #[serde(
        default = "default_input_ceiling_pct",
        deserialize_with = "deserialize_input_ceiling_pct"
    )]
    pub input_ceiling_pct: u32,

    /// Percent-of-ceiling below which the loop emits the low-remaining-budget
    /// nudge to the model. Historical hardcoded value was 15. Raise it to be
    /// warned earlier, lower it to suppress the nudge on roomy models. Clamped
    /// to `0..=100`; `0` disables the nudge. See `agentic::budget` (#559).
    #[serde(default = "default_low_budget_pct")]
    pub low_budget_pct: usize,
}

fn default_summary_input_cap_floor_chars() -> usize {
    8_192
}

pub(super) fn default_input_ceiling_pct() -> u32 {
    80
}

/// Normalize a configured input-ceiling percentage to its documented safe
/// domain. Invalid values fall back to the historical 80% default: zero must
/// not erase an authoritative ceiling, and 100%+ must not permit over-window
/// input budgets.
#[must_use]
pub fn normalize_input_ceiling_pct(value: u32) -> u32 {
    if (1..=99).contains(&value) {
        value
    } else {
        default_input_ceiling_pct()
    }
}

/// Resolve only the configured percentage bound for a full context window.
/// Generation-policy output reserves compose with this value in the agentic
/// loop; callers that hold an already-derived input cap must not apply it again.
#[must_use]
pub fn input_percentage_ceiling(context_window: u32, input_ceiling_pct: u32) -> u32 {
    let pct = normalize_input_ceiling_pct(input_ceiling_pct);
    (u64::from(context_window) * u64::from(pct) / 100) as u32
}

fn deserialize_input_ceiling_pct<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    u32::deserialize(deserializer).map(normalize_input_ceiling_pct)
}

fn default_low_budget_pct() -> usize {
    15
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            manager: ContextManager::default(),
            compaction_trigger_policy: CompactionTriggerPolicy::default(),
            features: ContextFeatures::default(),
            semantic: SemanticConfig::default(),
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: default_summary_input_cap_floor_chars(),
            api_surface: ApiSurfaceConfig::default(),
            input_ceiling_pct: default_input_ceiling_pct(),
            low_budget_pct: default_low_budget_pct(),
        }
    }
}
