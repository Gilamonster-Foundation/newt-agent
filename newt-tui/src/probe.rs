//! Tool-conformance probing and context-window discovery for Ollama models.
//!
//! Sends a minimal test request to classify how a model handles tool calls:
//! - `Native`   — uses Ollama's `tool_calls` field correctly
//! - `TextMode` — embeds tool-call JSON in the `content` field as text
//! - `NoTools`  — ignores tools and answers with plain text
//!
//! Also queries `/api/show` to discover each model's declared context window,
//! and records empirical success/overflow data so the harness can self-tune
//! `num_ctx` without human intervention.
//!
//! Results are cached in `~/.newt/model-capabilities.json` so probing is
//! opt-in and never automatic. The cache is a stable JSON format that
//! downstream tools (e.g. gilamonster-agent) can read for model routing.

use std::collections::HashMap;
use std::path::PathBuf;

use newt_core::TokenEstimation;

pub use newt_core::parse_context_window_error;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// How a model handles tool-call requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConformance {
    /// Model uses Ollama's `tool_calls` wire format correctly.
    Native,
    /// Model puts tool-call JSON in the `content` field as text.
    /// The newt harness cannot dispatch these calls.
    TextMode,
    /// Model ignores tool definitions and answers with plain text.
    NoTools,
}

impl ToolConformance {
    /// Short display symbol for the capabilities table.
    pub fn symbol(&self) -> &'static str {
        match self {
            Self::Native => "✓ native",
            Self::TextMode => "~ text  ",
            Self::NoTools => "✗ none  ",
        }
    }
}

/// Confidence in the empirically-derived `safe_context` value.
/// Ratchets up with consecutive successes, resets to Low on overflow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuneConfidence {
    #[default]
    None,
    Low,
    Medium,
    High,
}

impl TuneConfidence {
    /// Promote one level (stops at High).
    pub fn promote(&self) -> Self {
        match self {
            Self::None => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium | Self::High => Self::High,
        }
    }
}

/// One row in the capability cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityEntry {
    pub conformance: ToolConformance,
    /// ISO-8601 date (YYYY-MM-DD) the probe was last run.
    pub tested_date: String,

    // --- Context window tuning (all optional for backward compat) ---
    /// Model's declared maximum context length from Ollama `/api/show`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,

    /// Smallest full context window reported by a numbered hard rejection.
    /// Kept distinct from an ordinary declaration/probe so explicit overrides
    /// remain experimental until the server actually refuses one. Once known,
    /// this ceiling only tightens and survives process restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_context_window: Option<u32>,

    /// Empirically confirmed safe `num_ctx` to send to Ollama.
    /// Starts at 80 % of `context_window`; ratchets down on overflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_context: Option<u32>,

    /// Input token count at which an empty response (overflow) was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_at: Option<u32>,

    /// Highest input token count that produced a successful response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ok_input: Option<u32>,

    /// Consecutive successes since the last overflow (used to promote confidence).
    #[serde(default)]
    pub consecutive_ok: u32,

    /// Confidence level in the current `safe_context` value.
    #[serde(default)]
    pub tune_confidence: TuneConfidence,

    /// ISO-8601 date the tuning was last updated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tune_date: Option<String>,

    /// Observed/estimated prompt-token ratio for this model (Phase 20,
    /// `docs/design/model-self-tuning.md` §2.1/§2.3): an EMA of per-round
    /// `prompt_eval_count / chars-4-estimate` samples, clamped [0.5, 3.0].
    /// Converts estimate-space figures into honest token space wherever the
    /// two currencies meet (compression triggers and targets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_ratio: Option<f32>,

    /// Model returned thinking-only responses (empty content, non-empty
    /// `thinking`/`reasoning` field) at least once (Phase 20 §2.1). Observed
    /// once, persisted so the quirk isn't re-discovered — at the cost of a
    /// prompt-inflating corrective retry — every session. Manual reset only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emits_thinking: Option<bool>,

    /// Token-accounting regime this entry's tuning values were recorded
    /// under. `0` (the serde default for entries that predate the field)
    /// means the pre-18.1 double-counting regime, whose per-turn "input"
    /// summed `prompt_eval_count` across every round of a turn — the B3
    /// baseline caught `max_ok_input: 25602` persisted at High confidence
    /// when the largest prompt the backend ever evaluated was 4,748 tokens
    /// (5.4×). [`migrate_accounting`] invalidates such entries once on load.
    /// NOTE: serde's missing-field default (0 = legacy) is deliberately
    /// different from `CapabilityEntry::default()` (current version), so
    /// entries created in-process never get migrated away.
    #[serde(default)]
    pub accounting_version: u32,
}

/// The token-accounting regime of the current build (Step 18.1:
/// prompt-tokens-preferred; turn input = largest single prompt evaluated).
pub const ACCOUNTING_VERSION: u32 = 1;

impl Default for CapabilityEntry {
    fn default() -> Self {
        Self {
            conformance: ToolConformance::NoTools,
            tested_date: String::new(),
            context_window: None,
            hard_context_window: None,
            safe_context: None,
            overflow_at: None,
            max_ok_input: None,
            consecutive_ok: 0,
            tune_confidence: TuneConfidence::None,
            tune_date: None,
            estimate_ratio: None,
            emits_thinking: None,
            // New entries are recorded under the current (truthful) regime.
            accounting_version: ACCOUNTING_VERSION,
        }
    }
}

impl CapabilityEntry {
    /// Record a successful inference turn.  Promotes confidence every 5 runs.
    /// Returns `true` if `safe_context` or confidence changed (caller should save cache).
    pub fn record_success(&mut self, input_tokens: u32, today: &str) -> bool {
        let mut changed = false;
        if self.max_ok_input.map(|m| input_tokens > m).unwrap_or(true) {
            self.max_ok_input = Some(input_tokens);
            changed = true;
        }
        self.consecutive_ok = self.consecutive_ok.saturating_add(1);
        if self.consecutive_ok >= 5 && self.tune_confidence != TuneConfidence::High {
            self.tune_confidence = self.tune_confidence.promote();
            self.tune_date = Some(today.to_string());
            self.consecutive_ok = 0;
            changed = true;
        }
        changed
    }

    /// Record a hard context-window rejection (HTTP 400 /
    /// `ContextWindowExceededError`) where the endpoint reported its real
    /// full context limit as `hard_limit` tokens.
    ///
    /// Persists the reported full window and sets `max_ok_input` to the default
    /// 80% input ceiling. Runtime callers with a configured percentage use
    /// [`Self::record_context_window_400_with_pct`] instead, so later requests
    /// reserve their actual generation allowance against the same effective
    /// ceiling. Confidence drops to `Low` because the previous tuning clearly
    /// overshot. See issue #223.
    ///
    /// Returns `true` if state changed (caller should save cache).
    pub fn record_context_window_400(&mut self, hard_limit: u32, today: &str) -> bool {
        self.record_context_window_400_with_pct(hard_limit, 80, today)
    }

    /// Runtime variant of [`Self::record_context_window_400`] that persists the
    /// active normalized input-ceiling percentage.
    pub fn record_context_window_400_with_pct(
        &mut self,
        hard_limit: u32,
        input_ceiling_pct: u32,
        today: &str,
    ) -> bool {
        // The reported `hard_limit` is authoritative about the model's true
        // ceiling. Keep that full-window fact separate from the conservative
        // input cap so cognition output can be reserved on future sessions.
        let hard_limit = self
            .hard_context_window
            .map_or(hard_limit, |known| known.min(hard_limit));
        self.hard_context_window = Some(hard_limit);
        self.context_window = Some(hard_limit);
        // Set the pre-send gate from the normalized configured percentage —
        // even when that raises a previously-low `max_ok_input` (issue #223 saw
        // a stale 251_640 while the real max was 1_000_000, so the gate must
        // move up to the authoritative configured cap).
        let new_cap = newt_core::config::input_percentage_ceiling(hard_limit, input_ceiling_pct);
        self.max_ok_input = Some(new_cap);
        self.consecutive_ok = 0;
        self.tune_confidence = TuneConfidence::Low;
        self.tune_date = Some(today.to_string());
        // Rein in `safe_context` (Ollama num_ctx KV allocation) only when it was
        // set higher — never raise it, to avoid VRAM surprises.
        if self.safe_context.map(|s| new_cap < s).unwrap_or(true) {
            self.safe_context = Some(new_cap);
        }
        true // always dirty after a 400
    }

    /// Record an overflow (empty response at `input_tokens` tokens).
    /// Reduces `safe_context` to 75 % of the overflow point.
    /// Returns `true` if state changed (caller should save cache).
    ///
    /// Phase 20 (`docs/design/model-self-tuning.md` §2.1): ALSO reins
    /// `max_ok_input` down to the same cap when it sits higher — both budget
    /// resolvers prefer the larger of the two figures, so lowering only
    /// `safe_context` left overflow learning inert (the audit's compounding
    /// defect: `record_overflow` was effectively dead code).
    pub fn record_overflow(&mut self, input_tokens: u32, today: &str) -> bool {
        let new_safe = input_tokens * 75 / 100;
        self.overflow_at = Some(input_tokens);
        self.consecutive_ok = 0;
        self.tune_confidence = TuneConfidence::Low;
        self.tune_date = Some(today.to_string());
        let changed = self.safe_context.map(|s| new_safe < s).unwrap_or(true);
        if changed {
            self.safe_context = Some(new_safe);
        }
        if self.max_ok_input.map(|m| new_safe < m).unwrap_or(false) {
            // Never SET an absent max_ok_input here — an overflow proves no
            // acceptance; it only reins an existing (now disproven) ratchet.
            self.max_ok_input = Some(new_safe);
        }
        true // always dirty after overflow
    }

    /// Record one backend-ACCEPTED prompt of `prompt_tokens` (Phase 20 §2.2:
    /// per-round evidence, applied at the moment of observation). Pure
    /// high-water ratchet: raises `max_ok_input` only when strictly higher
    /// and stamps `tune_date`; deliberately does NOT touch `consecutive_ok`
    /// or `tune_confidence` — those belong to the turn-level
    /// [`record_success`] accounting (one turn is one data point, however
    /// many rounds it ran). Returns `true` when dirty (caller should save).
    pub fn record_accepted_prompt(&mut self, prompt_tokens: u32, today: &str) -> bool {
        if self.max_ok_input.map(|m| prompt_tokens > m).unwrap_or(true) {
            self.max_ok_input = Some(prompt_tokens);
            self.tune_date = Some(today.to_string());
            return true;
        }
        false
    }

    /// Record one calibration sample: the backend evaluated `observed` real
    /// prompt tokens where the loop's chars/4 figure was `estimated`
    /// (Phase 20 §2.3). EMA `0.75·old + 0.25·sample`, clamped [0.5, 3.0].
    ///
    /// Samples with `observed < 0.5 × estimated` are SKIPPED: an Ollama
    /// prompt-cache hit reports only newly-evaluated tokens and would poison
    /// the ratio downward (spec §2.3). Returns `true` only when the stored
    /// value moved by more than 0.01 — the value itself is stored as-is, the
    /// threshold just avoids a disk write per round (save thrash).
    pub fn record_estimate_sample(&mut self, observed: u32, estimated: usize) -> bool {
        if estimated == 0 {
            return false;
        }
        let raw = observed as f32 / estimated as f32;
        if raw < 0.5 {
            return false;
        }
        let sample = raw.clamp(0.5, 3.0);
        let new = match self.estimate_ratio {
            None => sample,
            Some(old) => (0.75 * old + 0.25 * sample).clamp(0.5, 3.0),
        };
        let dirty = match self.estimate_ratio {
            None => true,
            Some(old) => (new - old).abs() > 0.01,
        };
        self.estimate_ratio = Some(new);
        dirty
    }

    /// Record the thinking-only response quirk (Phase 20 §2.1): empty
    /// content with a non-empty `thinking`/`reasoning` field. Sticky once
    /// observed (manual reset only); dirty only on the first observation.
    pub fn record_thinking_only(&mut self) -> bool {
        if self.emits_thinking == Some(true) {
            return false;
        }
        self.emits_thinking = Some(true);
        true
    }
}

/// Apply one loop-reported [`newt_core::RoundObservation`] to a capability
/// entry (Phase 20 §2.2) — the unit-testable seam behind the TUI's
/// `on_round_usage` closure, which stays a one-liner over this. Returns
/// `true` when the entry changed (caller should save the cache).
pub fn apply_observation(
    entry: &mut CapabilityEntry,
    obs: &newt_core::RoundObservation,
    today: &str,
) -> bool {
    apply_observation_with_input_ceiling_pct(entry, obs, today, 80)
}

/// Runtime form of [`apply_observation`] that composes numbered hard-window
/// evidence with the active normalized input ceiling instead of assuming 80%.
pub fn apply_observation_with_input_ceiling_pct(
    entry: &mut CapabilityEntry,
    obs: &newt_core::RoundObservation,
    today: &str,
    input_ceiling_pct: u32,
) -> bool {
    match *obs {
        newt_core::RoundObservation::Accepted {
            prompt_tokens,
            estimated_tokens,
        } => {
            // Bitwise OR, not `||`: both records must run — short-circuiting
            // would drop the calibration sample whenever the ratchet moved.
            entry.record_accepted_prompt(prompt_tokens, today)
                | entry.record_estimate_sample(prompt_tokens, estimated_tokens)
        }
        newt_core::RoundObservation::SuspectedOverflow { prompt_tokens } => {
            entry.record_overflow(prompt_tokens, today)
        }
        newt_core::RoundObservation::ContextWindow400 { context_window } => {
            entry.record_context_window_400_with_pct(context_window, input_ceiling_pct, today)
        }
        newt_core::RoundObservation::ThinkingOnly => entry.record_thinking_only(),
    }
}

/// The canonical capability-cache key for the active serving principal (#1126
/// B2). The ONLY constructor is [`cap_key`], and there is deliberately no
/// `From<String>` / `From<&str>`: a raw model name cannot be substituted for a
/// canonical key, so every keying site is compiler-forced through [`cap_key`]
/// and the Multiplexer-vs-Instance identity law can never be silently bypassed.
///
/// It is `#[serde(transparent)]` over its inner `String`, so the on-disk
/// `model-capabilities.json` map is byte-for-byte the pre-newtype format: a
/// Multiplexer entry is still keyed by the bare model name (every existing file
/// keeps loading), and an Instance entry is keyed `backend:<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapKey(String);

impl CapKey {
    /// The underlying string key — for the rare read-only lookup that must
    /// bridge to a `&str` API (e.g. tracing). Never used to CONSTRUCT a key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CapKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The full cache: capability key → entry. See [`cap_key`] for the keying
/// discipline (#1126 Phase B2): per-MODEL for multiplexer backends (today's
/// bare model name, unchanged), per-BACKEND for instance backends.
pub type CapabilityCache = HashMap<CapKey, CapabilityEntry>;

/// The capability-cache key for a backend/model pair (#1126 B2).
///
/// - **Multiplexer** (ollama; many-model gateways): capabilities are a property
///   of the MODEL — key by bare model name, exactly today's scheme, so every
///   existing model-capabilities.json entry keeps working.
/// - **Instance** (vLLM): the backend serves one model whose behavior can also
///   depend on the instance's launch flags — capabilities attach to the
///   BACKEND: key `backend:<name>`. Re-probing after a restart with a new
///   model naturally overwrites the same key (the old model's entry would be
///   stale anyway), and two instances serving the same model name don't
///   collide.
///
/// This is the SINGLE precedence algorithm for capability identity — callers
/// must not re-derive it. Returns a [`CapKey`] so the result can only be used as
/// a cache key, never confused with a wire model name.
#[must_use]
pub fn cap_key(serving: newt_core::Serving, backend_name: &str, model: &str) -> CapKey {
    CapKey(match serving {
        newt_core::Serving::Multiplexer => model.to_string(),
        newt_core::Serving::Instance => format!("backend:{backend_name}"),
    })
}

/// Metadata about a model from Ollama's `/api/tags`.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    /// Human-readable parameter size (e.g. "32.8B"), empty if unknown.
    pub param_size: String,
}

// ---------------------------------------------------------------------------
// Cache persistence
// ---------------------------------------------------------------------------

// Test-only: redirect the capability cache to an isolated dir on THIS thread, so a
// test can exercise cache load/save persistence WITHOUT swapping the process-global
// `$HOME`. Swapping global HOME raced every HOME-reading test in this binary (#507:
// ~20 tests intermittently failed with "Permission denied" writing `~/.newt/...`
// when their thread saw the cw-400 test's transient HOME). A thread-local override
// is invisible to the other test threads, so the race is gone at the source.
#[cfg(test)]
thread_local! {
    static CACHE_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test hook: point this thread's capability cache at `dir` (`None` clears it).
#[cfg(test)]
pub(crate) fn set_cache_dir_override(dir: Option<PathBuf>) {
    CACHE_DIR_OVERRIDE.with(|c| *c.borrow_mut() = dir);
}

#[cfg(test)]
mod cap_key_tests {
    use super::cap_key;
    use newt_core::Serving;

    #[test]
    fn multiplexer_keys_by_bare_model_name_backwards_compatible() {
        // Every existing model-capabilities.json entry keeps working: the
        // multiplexer key IS the model name, byte-for-byte.
        assert_eq!(
            cap_key(
                Serving::Multiplexer,
                "gpu-runner-ollama",
                "qwen2.5-coder:7b"
            )
            .as_str(),
            "qwen2.5-coder:7b"
        );
    }

    #[test]
    fn instance_keys_by_backend_so_restarts_overwrite_and_twins_dont_collide() {
        // Capabilities attach to the BACKEND for an instance: a vLLM restart
        // with a new model overwrites the same key (stale-by-definition), and
        // two instances serving the same model name stay distinct.
        assert_eq!(
            cap_key(Serving::Instance, "dgx1-vllm-8000", "ornith-1.0-35b").as_str(),
            "backend:dgx1-vllm-8000"
        );
        // The identity law in one place: two DISTINCT instances serving the
        // SAME model name resolve to DISTINCT keys, so neither poisons the
        // other's empirical capability evidence (the #1647 instance-isolation
        // gap this follow-up closes).
        assert_ne!(
            cap_key(Serving::Instance, "dgx1-vllm-8000", "m"),
            cap_key(Serving::Instance, "dgx1-vllm-8001", "m")
        );
        // …while a multiplexer keys purely by model, so two routes to the same
        // model on one gateway intentionally SHARE evidence.
        assert_eq!(
            cap_key(Serving::Multiplexer, "gpu-runner-ollama", "m"),
            cap_key(Serving::Multiplexer, "other-gateway", "m")
        );
    }

    /// The cache is serde-`transparent` over the inner string, so the on-disk
    /// `model-capabilities.json` is byte-for-byte the pre-newtype format:
    /// existing (multiplexer, bare-model-keyed) files keep round-tripping, and
    /// an instance entry persists as `backend:<name>`.
    #[test]
    fn capability_cache_round_trips_through_the_legacy_string_key_format() {
        use super::{CapabilityCache, CapabilityEntry};
        let entry = |max_ok_input: Option<u32>, safe_context: Option<u32>| CapabilityEntry {
            conformance: super::ToolConformance::Native,
            tested_date: "2026-06-10".into(),
            safe_context,
            max_ok_input,
            ..Default::default()
        };
        let mut cache = CapabilityCache::default();
        cache.insert(
            cap_key(
                Serving::Multiplexer,
                "gpu-runner-ollama",
                "qwen2.5-coder:7b",
            ),
            entry(Some(24_000), Some(26_214)),
        );
        cache.insert(
            cap_key(Serving::Instance, "dgx1-vllm-8000", "m"),
            entry(Some(120_000), None),
        );
        let json = serde_json::to_string(&cache).unwrap();
        // Legacy string keys, exactly as prior versions wrote them.
        assert!(json.contains("\"qwen2.5-coder:7b\""));
        assert!(json.contains("\"backend:dgx1-vllm-8000\""));
        let restored: CapabilityCache = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, cache);
    }

    /// INSTANCE ISOLATION — the headline #1647 gap this follow-up closes. Two
    /// vLLM `Serving::Instance` backends A and B serve the SAME model name "m"
    /// but have different real capabilities. Exercising A → B → A must prove B
    /// never inherits A's `max_ok_input` / `safe_context` / recovered window /
    /// `estimate_ratio` / budget, and B's smaller limit + failure evidence never
    /// poison A. (Before the cap_key rekey, both keyed by "m" and collided.)
    #[test]
    fn two_instances_serving_the_same_model_are_isolated_through_a_to_b_to_a() {
        use super::{resolve_memory_budget, CapKey, CapabilityCache, CapabilityEntry};
        let entry = |max_ok_input: Option<u32>,
                     safe_context: Option<u32>,
                     estimate_ratio: Option<f32>| CapabilityEntry {
            conformance: super::ToolConformance::Native,
            tested_date: "2026-08-01".into(),
            safe_context,
            max_ok_input,
            estimate_ratio,
            ..Default::default()
        };
        // A: a 128K-class instance; B: a 32K-class instance — SAME model "m".
        let key_a = cap_key(Serving::Instance, "vllm-8000", "m");
        let key_b = cap_key(Serving::Instance, "vllm-8001", "m");
        assert_ne!(
            key_a, key_b,
            "same model, distinct backends → distinct keys"
        );

        let mut cache = CapabilityCache::default();
        cache.insert(
            key_a.clone(),
            entry(Some(120_000), Some(120_000), Some(1.10)),
        );
        cache.insert(key_b.clone(), entry(Some(30_000), Some(30_000), Some(0.90)));

        // A → B → A: each resolves to ITS OWN budget, never the other's.
        assert_eq!(
            resolve_memory_budget(None, None, cache.get(&key_a)),
            120_000
        );
        assert_eq!(
            resolve_memory_budget(None, None, cache.get(&key_b)),
            30_000,
            "B must NOT inherit A's 120K high-water mark / safe context / budget"
        );
        assert_eq!(
            resolve_memory_budget(None, None, cache.get(&key_a)),
            120_000,
            "A survives the round trip unchanged"
        );
        // Distinct empirical state — ratios never cross.
        assert_eq!(cache.get(&key_a).unwrap().estimate_ratio, Some(1.10));
        assert_eq!(cache.get(&key_b).unwrap().estimate_ratio, Some(0.90));

        // B hits a hard 400 and tightens its OWN safe_context — A is untouched.
        cache.get_mut(&key_b).unwrap().safe_context = Some(16_000);
        assert_eq!(
            resolve_memory_budget(None, None, cache.get(&key_a)),
            120_000,
            "B's failure/recovery must not poison A"
        );

        // recovered_context_windows is the same HashMap<CapKey, u32> run_chat
        // now keys by cap_id: B's recovery ceiling never lands on A's key.
        let mut recovered = std::collections::HashMap::<CapKey, u32>::new();
        recovered.insert(key_b.clone(), 16_000);
        assert_eq!(recovered.get(&key_a).copied(), None);
        assert_eq!(recovered.get(&key_b).copied(), Some(16_000));

        // MULTIPLEXER SHARING preserved: two routes to the same model on a
        // gateway intentionally share one model-keyed entry.
        let mux = cap_key(Serving::Multiplexer, "gpu-runner-ollama", "m");
        cache.insert(mux.clone(), entry(Some(64_000), Some(64_000), None));
        assert_eq!(resolve_memory_budget(None, None, cache.get(&mux)), 64_000);
        assert_eq!(
            cache.get(&cap_key(Serving::Multiplexer, "other-route", "m")),
            cache.get(&mux),
            "any route to model m on a multiplexer shares the same evidence"
        );
    }
}

fn cache_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(dir) = CACHE_DIR_OVERRIDE.with(|c| c.borrow().clone()) {
        return Some(dir.join("model-capabilities.json"));
    }
    newt_core::Config::user_config_path().map(|p| p.with_file_name("model-capabilities.json"))
}

/// Load the capability cache from disk, returning an empty map on any error.
///
/// Runs [`migrate_accounting`] on the parsed cache and persists the result
/// when anything changed, so poisoned pre-18.1 ratchet values are invalidated
/// exactly once.
pub fn load_cache() -> CapabilityCache {
    let Some(path) = cache_path() else {
        return Default::default();
    };
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Default::default();
    };
    let mut cache: CapabilityCache = serde_json::from_str(&data).unwrap_or_default();
    if migrate_accounting(&mut cache) {
        save_cache(&cache);
    }
    cache
}

/// One-time de-poisoning of ratchet values recorded under the pre-18.1
/// double-counting regime (issue #247, live evidence in the B3 baseline).
///
/// An entry is invalidated when it predates `accounting_version` — i.e. its
/// `max_ok_input` was ratcheted from the per-turn SUM of `prompt_eval_count`
/// across rounds, not from any prompt the backend actually evaluated. The
/// measured poisoned entry also fails the honesty cross-check (`max_ok_input`
/// 25,602 > `safe_context` 6,553 — a success above the KV window is
/// impossible for an Ollama-tuned entry); both conditions collapse onto the
/// same set here because every versionless entry was recorded double-counted.
///
/// Invalidation drops `max_ok_input` and resets `consecutive_ok` /
/// `tune_confidence` so the ratchet re-learns from truthful numbers; the
/// entry is then stamped with the current version, making the migration
/// idempotent. Entries already at the current version are never touched —
/// in particular a post-#223 `max_ok_input` above `safe_context` is
/// legitimate there (the cw-400 path derives it from the endpoint's reported
/// hard limit while `safe_context` stays VRAM-capped).
///
/// Returns `true` when anything changed (caller should persist).
pub fn migrate_accounting(cache: &mut CapabilityCache) -> bool {
    let mut dirty = false;
    for (key, entry) in cache.iter_mut() {
        if entry.accounting_version >= ACCOUNTING_VERSION {
            continue;
        }
        if entry.max_ok_input.is_some() {
            tracing::info!(
                cap_key = %key,
                max_ok_input = entry.max_ok_input,
                "invalidating max_ok_input recorded under the double-counting \
                 regime (Step 18.1); the ratchet will re-learn"
            );
            entry.max_ok_input = None;
            entry.consecutive_ok = 0;
            entry.tune_confidence = TuneConfidence::None;
        }
        entry.accounting_version = ACCOUNTING_VERSION;
        dirty = true;
    }
    dirty
}

/// Persist the capability cache to disk (best-effort).
pub fn save_cache(cache: &CapabilityCache) {
    let Some(path) = cache_path() else { return };
    if let Ok(data) = serde_json::to_string_pretty(cache) {
        let _ = std::fs::write(path, data);
    }
}

// ---------------------------------------------------------------------------
// Memory-budget resolution (Step 18.2, #247)
// ---------------------------------------------------------------------------

/// Resolve the context-token budget injected into the memory providers
/// (`TokenBudget` / `Summarizing`) at construction and rebound in place every
/// turn (#1647) so the budget follows the active model/backend.
///
/// **One unambiguous precedence, highest first:**
/// 1. **Explicit `[memory] context_tokens`** — a deliberate operator override;
///    always honoured.
/// 2. **`declared_window`** — the active model's declared context window
///    (live endpoint metadata, explicit `[[model]]` tuning, or an imported
///    community profile), resolved by the caller for the CURRENTLY SELECTED
///    model. Since #1647 a declared window DOES participate: a `/model` switch
///    on a multiplexing gateway changes the window without any probe, and the
///    memory budget must follow. This supersedes the older doctrine (a declared
///    window is "a claim, not a measurement") — that rule governed the empirical
///    ratchet in tier 3, NOT this construction-time budget seed.
/// 3. **Capability-derived** — from the already-resolved capability `entry` for
///    the active serving principal (keyed by [`cap_key`], NOT by a raw model
///    name): `max(max_ok_input, safe_context)` when both exist, else whichever
///    exists (Phase 20, `docs/design/model-self-tuning.md` §2.1 — the table is
///    the contract). `max_ok_input` is a high-water mark of PROVEN-good input —
///    a floor, not a ceiling — so it never pulls the budget below the
///    believed-safe window; a prompt proven beyond the claim outranks it. The
///    cw-400 path reins `safe_context` to its authoritative cap, so `max()`
///    still lands on the authoritative number after a hard 400.
/// 4. **Static default** — [`newt_core::DEFAULT_CONTEXT_TOKENS`] only when none
///    of the above exists (fresh model, no probe data yet).
///
/// The caller resolves the capability `entry` itself
/// (`cache.get(&cap_key(...))`) and passes it in, so this function is NOT a
/// keying site: it cannot be handed a raw model string where a canonical
/// [`CapKey`] belongs. See [`cap_key`] for the Multiplexer-vs-Instance law.
pub fn resolve_memory_budget(
    explicit: Option<u32>,
    declared_window: Option<u32>,
    entry: Option<&CapabilityEntry>,
) -> u32 {
    explicit
        .or(declared_window)
        .or_else(|| {
            entry.and_then(|e| match (e.max_ok_input, e.safe_context) {
                (Some(m), Some(s)) => Some(m.max(s)),
                (m, s) => m.or(s),
            })
        })
        .unwrap_or(newt_core::DEFAULT_CONTEXT_TOKENS)
}

// ---------------------------------------------------------------------------
// Model list (with metadata)
// ---------------------------------------------------------------------------

/// Fetch model info from Ollama's `/api/tags`, returning name + param_size.
pub fn fetch_ollama_models(endpoint: &str) -> anyhow::Result<Vec<ModelInfo>> {
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let json: serde_json::Value = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let resp = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?
                .get(&url)
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("HTTP {}", resp.status());
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        })
    })?;
    Ok(json["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m["name"].as_str()?.to_string();
                    let param_size = m["details"]["parameter_size"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    Some(ModelInfo { name, param_size })
                })
                .collect()
        })
        .unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Context window discovery via /api/show
// ---------------------------------------------------------------------------

/// Query Ollama's `/api/show` and return the model's declared context window.
///
/// Checks two sources in order and returns the smaller (most conservative):
/// 1. `model_info["<arch>.context_length"]` — architecture-level limit
/// 2. `num_ctx` line in the `parameters` string — Modelfile override
///
/// Returns `None` if the endpoint is unreachable or the response lacks both fields.
pub fn fetch_context_window(
    endpoint: &str,
    model: &str,
    kind: newt_core::BackendKind,
) -> Option<u32> {
    // Ask the backend (#backend-trait): Ollama reads /api/show, vLLM reads
    // /v1/models max_model_len (#1195) — the per-API impl owns which. Sync
    // bridge (block_in_place) since the probe path is sync.
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()?;
            newt_core::backend_probe::api_for(kind)
                .context_window(&client, endpoint, model, None)
                .await
        })
    })
}

/// Ensure `entry` has a `context_window` and an initial `safe_context`.
/// Calls `/api/show` only when the context window is not yet known.
/// Returns `true` if the entry was updated (caller should save cache).
///
/// `trust_declared` (the default posture, `real_context_discovery = false`):
/// the declared window is authoritative — `safe_context` is (re)asserted to
/// ~80 % of it every session, **raising** a value a past overflow reined down,
/// so a capable model is never permanently capped. When `false` (empirical
/// mode) the original VRAM rule holds: bootstrap once, never auto-raise.
pub fn ensure_context_window(
    entry: &mut CapabilityEntry,
    endpoint: &str,
    model: &str,
    trust_declared: bool,
    kind: newt_core::BackendKind,
) -> bool {
    // Empirical mode keeps the original fetch-once contract: when the window is
    // already known, do nothing — a reined-down safe_context must persist.
    if !trust_declared && entry.context_window.is_some() {
        return false;
    }
    // Fetch the declared window once (negative-cached by the caller). When it's
    // already known we skip the /api/show round trip but still (re)assert
    // safe_context below in trust-declared mode.
    let mut changed = false;
    if entry.context_window.is_none() {
        let Some(window) = fetch_context_window(endpoint, model, kind) else {
            return false;
        };
        entry.context_window = Some(window);
        changed = true;
    }
    let Some(window) = entry.context_window else {
        return changed;
    };
    let declared_safe = window * 80 / 100;
    if trust_declared {
        // Authoritative declared window: raise/assert safe_context to ~80 %,
        // un-sticking any reined-down value (issue #382/#383).
        if entry.safe_context != Some(declared_safe) {
            entry.safe_context = Some(declared_safe);
            changed = true;
        }
    } else if entry.safe_context.is_none() {
        // Empirical: bootstrap at 80 % unless already set; never auto-raise.
        entry.safe_context = Some(declared_safe);
        changed = true;
    }
    changed
}

// ---------------------------------------------------------------------------
// Active discovery (Step 20.2 — docs/design/model-self-tuning.md §4)
// ---------------------------------------------------------------------------

/// Today's date as `YYYY-MM-DD` in local time — the stamp the active probes
/// write into `tested_date` / `tune_date`. Lives here so both the TUI handler
/// and the `newt tunings` staleness surface (newt-cli, which has no chrono
/// dependency) share one source of truth.
pub fn today_local_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Staleness-aware sibling of [`ensure_context_window`] (Step 20.2 §4.2).
///
/// `/probe` is the explicit re-discover command, so this **always** calls
/// [`fetch_context_window`] (no early return on a known window) and updates
/// `entry.context_window` whenever the fetch succeeds — catching a re-pulled
/// model whose Modelfile `num_ctx` changed since the last probe. With
/// `trust_declared` (the default) it asserts `safe_context` to 80 % of the
/// declared window, **raising** it if needed; in empirical mode
/// (`real_context_discovery`) it re-bootstraps only when `safe_context.is_none()`
/// and never auto-raises (the VRAM rule, §2.1 / §4.2): a larger declared window
/// does not free more KV cache. Returns `true` when anything changed (caller
/// saves); a fetch failure leaves the entry untouched and returns `false`.
///
/// The passive session path keeps using [`ensure_context_window`]
/// (fetch-once, negative-cached); only `/probe` forces this refresh.
pub fn refresh_context_window(
    entry: &mut CapabilityEntry,
    endpoint: &str,
    model: &str,
    trust_declared: bool,
    kind: newt_core::BackendKind,
) -> bool {
    let Some(window) = fetch_context_window(endpoint, model, kind) else {
        return false;
    };
    let mut changed = entry.context_window != Some(window);
    entry.context_window = Some(window);
    let declared_safe = window * 80 / 100;
    if trust_declared {
        // `/probe` re-trusts the declared window: raise safe_context to ~80 %.
        if entry.safe_context != Some(declared_safe) {
            entry.safe_context = Some(declared_safe);
            changed = true;
        }
    } else if entry.safe_context.is_none() {
        // Empirical: bootstrap (never auto-raise) at 80 % of declared max.
        entry.safe_context = Some(declared_safe);
    }
    changed
}

/// Local, content-free re-detection of the thinking-only response quirk
/// (Step 20.2 §4.3): `true` when `message.content` is empty/whitespace AND
/// any of `thinking` / `reasoning` / `reasoning_content` is a non-empty
/// string. Deliberately NOT a dependency on newt-core's private
/// `ollama_non_content_fields` (the crate boundary forbids it, and the logic
/// is tiny) — kept in lock-step with §2.1's `record_thinking_only` semantics.
pub fn message_thinking_fields(message: &serde_json::Value) -> bool {
    let content_empty = message["content"]
        .as_str()
        .map(|c| c.trim().is_empty())
        .unwrap_or(true);
    if !content_empty {
        return false;
    }
    ["thinking", "reasoning", "reasoning_content"]
        .iter()
        .any(|field| {
            message[*field]
                .as_str()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        })
}

/// Outcome of the cheap thinking probe (Step 20.2 §4.3 / §4.4).
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeThinking {
    /// The model returned a thinking-only response (empty content + a
    /// non-empty `thinking`/`reasoning` field) to the tiny probe.
    pub emits_thinking: bool,
    /// `(estimated_tokens, observed prompt_eval_count)` for the probe request,
    /// `Some` only when the response carried `prompt_eval_count` — a
    /// calibration sample for [`CapabilityEntry::record_estimate_sample`]
    /// (§4.4). `estimated_tokens` is the request body's chars/4 figure.
    pub calibration: Option<(u32, usize)>,
}

/// Send one tiny `stream:false` `/api/chat` request and classify the thinking
/// quirk (Step 20.2 §4.3), harvesting a calibration sample (§4.4) from the
/// same request. 60 s timeout; the model must already be warm. Mirrors
/// [`fetch_context_window`]'s `block_in_place` + `Handle::block_on` pattern so
/// it runs from inside the synchronous slash dispatcher.
pub fn probe_thinking(
    endpoint: &str,
    model: &str,
    est: TokenEstimation,
) -> anyhow::Result<ProbeThinking> {
    let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
    let prompt = "Reply with the single word: ok";
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    });
    // chars/4 estimate of the serialized request body (same currency the
    // agentic loop estimates in) — paired with the backend's real count.
    let estimated = serde_json::to_string(&body)
        .map(|s| est.tokens_for_chars(s.chars().count()))
        .unwrap_or(est.tokens_for_chars(prompt.chars().count()));

    let json: serde_json::Value = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let resp = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()?
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
            if !resp.status().is_success() {
                anyhow::bail!("Ollama returned {}", resp.status());
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        })
    })?;

    let emits_thinking = message_thinking_fields(&json["message"]);
    // calibration = (observed real prompt tokens, our chars/4 estimate) —
    // the (observed, estimated) order record_estimate_sample expects.
    let calibration = json["prompt_eval_count"]
        .as_u64()
        .map(|observed| (observed as u32, estimated));

    Ok(ProbeThinking {
        emits_thinking,
        calibration,
    })
}

/// Sanitize a learned `estimate_ratio` for use as a multiplier (Step 20.1
/// hygiene, reused by §4.5): a non-finite or out-of-band value falls back to
/// `1.0`; otherwise it is clamped to the same `[0.5, 3.0]` band the EMA
/// stores in.
fn sanitize_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() && (0.5..=3.0).contains(&ratio) {
        ratio
    } else {
        1.0
    }
}

/// Build a deterministic filler prompt whose chars/4 estimate, scaled by the
/// model's `estimate_ratio`, lands near `target_real_tokens` (Step 20.2 §4.5).
///
/// No RNG (and none is available in this environment): the filler is a short
/// clause repeated to the needed length. The chars/4 estimator counts ~4
/// chars per token, and the learned `ratio` converts estimate→real, so to make
/// the backend evaluate ≈ `target_real_tokens` real tokens we need
/// `chars ≈ target / ratio * 4`. Sanitized like 20.1 (finite, `[0.5, 3.0]`,
/// else `1.0`).
pub fn build_padded_prompt(target_real_tokens: u32, ratio: f32) -> String {
    let ratio = sanitize_ratio(ratio);
    // estimate_tokens * ratio ≈ real_tokens  ⇒  estimate_tokens ≈ target / ratio
    // chars ≈ estimate_tokens * 4.
    let target_estimate = (target_real_tokens as f32 / ratio).round().max(1.0);
    let target_chars = (target_estimate * 4.0).round() as usize;
    // A short, space-terminated clause; repeating it lands within one clause
    // length of the target, i.e. well within the ±10 % unit-test tolerance.
    const CLAUSE: &str = "lorem ipsum dolor sit amet ";
    let mut s = String::with_capacity(target_chars + CLAUSE.len());
    while s.chars().count() < target_chars {
        s.push_str(CLAUSE);
    }
    s
}

/// Classification of one boundary probe (Step 20.2 §4.5) — the pure decision,
/// split from the HTTP so every arm is unit-testable without a server.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundaryClass {
    /// HTTP 200, a usable completion, and the backend evaluated ≥ 90 % of the
    /// sent estimate: the model genuinely accepted the prompt.
    Accepted { prompt_tokens: u32 },
    /// HTTP 200 but `prompt_eval_count` well below the sent estimate — Ollama
    /// silently dropped the head of the prompt. Treated as rejected.
    Truncated,
    /// A hard context-window 400 whose body parsed to a real `limit`.
    CtxWindow400 { limit: u32 },
    /// Any other transport/5xx/OOM error: stop raising, keep the last
    /// accepted value, do not record a false boundary.
    Inconclusive,
}

/// Classify a boundary probe result (Step 20.2 §4.5). `http` is the parsed
/// `/api/chat` body on success or the transport error on failure;
/// `sent_real_estimate` is the candidate `N` (the real-token target the
/// padded prompt was sized for). Pure: no I/O.
///
/// - HTTP 200 + usable (non-empty content OR a tool call OR `eval_count > 0`)
///   plus `prompt_eval_count >= 90% x N` => [`BoundaryClass::Accepted`]
///   carrying the observed `prompt_eval_count`.
/// - HTTP 200 but `prompt_eval_count < 90% x N` => [`BoundaryClass::Truncated`].
/// - An `Err` whose message parses via [`parse_context_window_error`] =>
///   [`BoundaryClass::CtxWindow400`].
/// - Any other `Err` => [`BoundaryClass::Inconclusive`].
pub fn classify_boundary_probe(
    http: Result<&serde_json::Value, &anyhow::Error>,
    sent_real_estimate: u32,
) -> BoundaryClass {
    match http {
        Ok(json) => {
            let message = &json["message"];
            let content_nonempty = message["content"]
                .as_str()
                .map(|c| !c.trim().is_empty())
                .unwrap_or(false);
            let has_tool_call = message["tool_calls"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            let eval_count = json["eval_count"].as_u64().unwrap_or(0);
            let usable = content_nonempty || has_tool_call || eval_count > 0;

            let prompt_eval = json["prompt_eval_count"].as_u64().unwrap_or(0) as u32;
            // 90 % of the sent estimate, computed without overflow.
            let threshold = (sent_real_estimate as u64 * 90 / 100) as u32;
            if usable && prompt_eval >= threshold {
                BoundaryClass::Accepted {
                    prompt_tokens: prompt_eval,
                }
            } else {
                // 200 but the model evaluated far fewer tokens than we sent
                // (head silently dropped) — or produced nothing usable.
                BoundaryClass::Truncated
            }
        }
        Err(e) => match parse_context_window_error(&e.to_string()) {
            Some((_, limit)) => BoundaryClass::CtxWindow400 {
                limit: limit as u32,
            },
            None => BoundaryClass::Inconclusive,
        },
    }
}

/// One HTTP boundary probe at candidate `n` real tokens (Step 20.2 §4.5).
/// Sends a padded prompt with `options.num_ctx = n + reply_margin` and a
/// minimal `num_predict` (we test acceptance, not output). Returns the parsed
/// body on a 2xx, or an error (carrying any context-window-400 body) so the
/// caller can [`classify_boundary_probe`]. 120 s timeout — large prompt eval
/// is slow. Returns the sent chars/4 estimate alongside for calibration.
fn boundary_probe_request(
    endpoint: &str,
    model: &str,
    prompt: &str,
    num_ctx: u32,
    est: TokenEstimation,
) -> (anyhow::Result<serde_json::Value>, usize) {
    let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
        "options": {"num_ctx": num_ctx, "num_predict": 8},
    });
    let sent_chars4 = serde_json::to_string(&body)
        .map(|s| est.tokens_for_chars(s.chars().count()))
        .unwrap_or(est.tokens_for_chars(prompt.chars().count()));

    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let resp = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()?
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
            let status = resp.status();
            // Capture the body either way: a 400's body carries the hard limit.
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("inference endpoint {status}: {text}");
            }
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| anyhow::anyhow!("bad JSON from /api/chat: {e}"))
        })
    });
    (result, sent_chars4)
}

/// Result of the empirical input-boundary search (Step 20.2 §4.5).
#[derive(Debug, Clone, PartialEq)]
pub struct BoundarySearchOutcome {
    /// Highest `prompt_eval_count` the backend confirmed it accepted, if any.
    pub highest_accepted: Option<u32>,
    /// Number of probe steps performed.
    pub steps: u32,
    /// Final search bounds `(low, high)` when the loop terminated.
    pub final_bounds: (u32, u32),
    /// A surfaced error message when the search stopped on an inconclusive
    /// transport failure (the last accepted value is still kept).
    pub error: Option<String>,
}

/// Active binary search for the largest input the model genuinely accepts at a
/// matching `num_ctx` (Step 20.2 §4.5). Records `max_ok_input` at `High`
/// confidence on completion.
///
/// `low` starts at `entry.safe_context` (or a 2,048 floor); `high` at the
/// declared `entry.context_window`. When the window is unknown, `high` is
/// found by doubling from `low` until the first non-`Accepted` probe (capped),
/// to bracket the boundary. **Never** probes `num_ctx` above a known declared
/// window (VRAM safety — the model was pulled to run at that window).
///
/// Per candidate `N`: build a padded prompt of ≈ N real tokens (sized by the
/// learned `estimate_ratio`), send with `options.num_ctx = N + reply_margin`,
/// classify, and:
/// - **Accepted** → [`CapabilityEntry::record_accepted_prompt`] +
///   [`CapabilityEntry::record_estimate_sample`]; raise `low`.
/// - **Truncated** → lower `high`.
/// - **CtxWindow400** → [`CapabilityEntry::record_context_window_400`]; set
///   `high = limit`.
/// - **Inconclusive** → break, keeping the last accepted value, surface the
///   error.
///
/// Converges until `high − low ≤ max(1024, high·5%)` or the step cap (12).
/// On any acceptance, sets `tune_confidence = High` and `tune_date = today`.
/// `progress` is called once per step with a one-line status. The entry is
/// mutated in place; the caller persists.
pub fn probe_input_boundary(
    endpoint: &str,
    model: &str,
    entry: &mut CapabilityEntry,
    mut progress: impl FnMut(&str),
    today: &str,
    est: TokenEstimation,
) -> anyhow::Result<BoundarySearchOutcome> {
    const STEP_CAP: u32 = 12;
    const REPLY_MARGIN: u32 = 256;
    const LOW_FLOOR: u32 = 2_048;
    const DOUBLE_CAP: u32 = 1_000_000;

    let ratio = entry.estimate_ratio.map(sanitize_ratio).unwrap_or(1.0);
    let mut low = entry.safe_context.unwrap_or(LOW_FLOOR).max(LOW_FLOOR);
    let declared = entry.context_window;
    let mut highest_accepted: Option<u32> = None;
    let mut steps = 0u32;
    let mut error: Option<String> = None;

    // One probe at candidate `n`, with the matching num_ctx, classified.
    let run_probe =
        |n: u32, entry: &mut CapabilityEntry, progress: &mut dyn FnMut(&str)| -> BoundaryClass {
            // VRAM safety: never request num_ctx above a known declared window.
            let num_ctx = match declared {
                Some(w) => (n + REPLY_MARGIN).min(w),
                None => n + REPLY_MARGIN,
            };
            let prompt = build_padded_prompt(n, ratio);
            let (http, sent_chars4) =
                boundary_probe_request(endpoint, model, &prompt, num_ctx, est);
            let class = classify_boundary_probe(http.as_ref(), n);
            match &class {
                BoundaryClass::Accepted { prompt_tokens } => {
                    entry.record_accepted_prompt(*prompt_tokens, today);
                    entry.record_estimate_sample(*prompt_tokens, sent_chars4);
                    progress(&format!(
                        "  num_ctx={num_ctx}: accepted (prompt_eval={prompt_tokens})"
                    ));
                }
                BoundaryClass::Truncated => {
                    progress(&format!("  num_ctx={num_ctx}: truncated/rejected"));
                }
                BoundaryClass::CtxWindow400 { limit } => {
                    progress(&format!(
                        "  num_ctx={num_ctx}: context-window 400 (limit {limit})"
                    ));
                }
                BoundaryClass::Inconclusive => {
                    progress(&format!(
                        "  num_ctx={num_ctx}: inconclusive — {}",
                        http.err()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "transport error".into())
                    ));
                }
            }
            class
        };

    // Establish `high`. Known window → use it (hard cap). Unknown → double
    // from `low` until the first non-Accepted probe brackets the boundary.
    let mut high = match declared {
        Some(w) => w.max(low + 1),
        None => {
            let mut candidate = low.saturating_mul(2).max(low + 1024).min(DOUBLE_CAP);
            let mut bracket_high = DOUBLE_CAP;
            loop {
                if steps >= STEP_CAP {
                    break;
                }
                steps += 1;
                match run_probe(candidate, entry, &mut progress) {
                    BoundaryClass::Accepted { prompt_tokens } => {
                        highest_accepted =
                            Some(highest_accepted.map_or(prompt_tokens, |h| h.max(prompt_tokens)));
                        low = low.max(candidate);
                        if candidate >= DOUBLE_CAP {
                            bracket_high = DOUBLE_CAP;
                            break;
                        }
                        candidate = candidate.saturating_mul(2).min(DOUBLE_CAP);
                    }
                    BoundaryClass::CtxWindow400 { limit } => {
                        entry.record_context_window_400(limit, today);
                        bracket_high = limit;
                        break;
                    }
                    BoundaryClass::Truncated => {
                        bracket_high = candidate;
                        break;
                    }
                    BoundaryClass::Inconclusive => {
                        error = Some("boundary search stopped: inconclusive probe".into());
                        bracket_high = candidate;
                        break;
                    }
                }
            }
            bracket_high
        }
    };

    // Binary search the bracket [low, high].
    while error.is_none() && steps < STEP_CAP {
        let tolerance = 1_024u32.max((high as u64 * 5 / 100) as u32);
        if high.saturating_sub(low) <= tolerance {
            break;
        }
        let mid = low + (high - low) / 2;
        steps += 1;
        match run_probe(mid, entry, &mut progress) {
            BoundaryClass::Accepted { prompt_tokens } => {
                highest_accepted =
                    Some(highest_accepted.map_or(prompt_tokens, |h| h.max(prompt_tokens)));
                low = mid;
            }
            BoundaryClass::Truncated => {
                high = mid;
            }
            BoundaryClass::CtxWindow400 { limit } => {
                entry.record_context_window_400(limit, today);
                high = limit.min(high);
                if limit < low {
                    low = limit;
                }
            }
            BoundaryClass::Inconclusive => {
                error = Some("boundary search stopped: inconclusive probe".into());
                break;
            }
        }
    }

    if highest_accepted.is_some() {
        // record_accepted_prompt already raised max_ok_input to the highest
        // accepted value; stamp the High-confidence discovery (§4.5).
        entry.tune_confidence = TuneConfidence::High;
        entry.tune_date = Some(today.to_string());
    }

    Ok(BoundarySearchOutcome {
        highest_accepted,
        steps,
        final_bounds: (low, high),
        error,
    })
}

/// Tuning-staleness predicate (Step 20.2 §4.6): `true` when `tune_date` is
/// `None`, unparseable, or older than `max_age_days` relative to `today`.
/// Dates are `YYYY-MM-DD` parsed via [`chrono::NaiveDate`]; an unparseable
/// stored date is treated as stale (re-probe rather than trust a bad stamp).
pub fn is_tuning_stale(tune_date: Option<&str>, today: &str, max_age_days: i64) -> bool {
    let Some(stamp) = tune_date else {
        return true;
    };
    let (Ok(then), Ok(now)) = (
        chrono::NaiveDate::parse_from_str(stamp, "%Y-%m-%d"),
        chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d"),
    ) else {
        return true;
    };
    (now - then).num_days() > max_age_days
}

/// Age in days of a `YYYY-MM-DD` tuning stamp relative to `today` (Step 20.2
/// §4.6 — the human-readable figure behind [`is_tuning_stale`]'s yes/no).
/// `None` when either date is absent or unparseable. Shared with newt-cli's
/// `newt tunings show`, which has no chrono dependency of its own.
pub fn tuning_age_days(tune_date: Option<&str>, today: &str) -> Option<i64> {
    let then = chrono::NaiveDate::parse_from_str(tune_date?, "%Y-%m-%d").ok()?;
    let now = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d").ok()?;
    Some((now - then).num_days())
}

/// The report [`full_probe`] returns for the TUI to print (Step 20.2 §4.1).
#[derive(Debug, Clone, PartialEq)]
pub struct FullProbeReport {
    pub conformance: ToolConformance,
    pub context_window: Option<u32>,
    pub emits_thinking: bool,
    pub estimate_ratio: Option<f32>,
    /// `Some` only in `/probe window` mode (the expensive pass ran).
    pub boundary: Option<BoundarySearchOutcome>,
    /// Any non-fatal error surfaced by a cheap sub-probe (e.g. the thinking
    /// probe failed but conformance and the window refresh succeeded).
    pub notes: Vec<String>,
}

/// Thin orchestrator (Step 20.2 §4.1) tying the discovery passes together so
/// the TUI handler stays a printer. Runs tool conformance (via
/// [`probe_tool_conformance_calibrated`], which also harvests the §4.4
/// calibration sample from the tool-schema-bearing request),
/// [`refresh_context_window`], [`probe_thinking`] and — when `do_window` —
/// [`probe_input_boundary`]. Feeds every calibration
/// sample into [`CapabilityEntry::record_estimate_sample`], records the
/// thinking quirk, updates `conformance` / `tested_date`, and **mutates
/// `entry` in place** so the caller's `..existing` 20.1 fields are preserved.
/// Persisting is the caller's job; the report is for display.
#[allow(clippy::too_many_arguments)]
pub fn full_probe(
    endpoint: &str,
    model: &str,
    entry: &mut CapabilityEntry,
    do_window: bool,
    today: &str,
    mut progress: impl FnMut(&str),
    est: TokenEstimation,
    kind: newt_core::BackendKind,
) -> FullProbeReport {
    let mut notes = Vec::new();

    // 1. Tool conformance (unchanged classification) + calibration bootstrap
    //    (§4.4): the conformance request carries the tool schema, so its
    //    prompt_eval_count is the most informative of the cheap-probe samples.
    let conformance = match tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(probe_tool_conformance_calibrated(endpoint, model, est))
    }) {
        Ok(pc) => {
            if let Some((observed, estimated)) = pc.calibration {
                entry.record_estimate_sample(observed, estimated);
            }
            pc.conformance
        }
        Err(e) => {
            notes.push(format!("conformance probe failed: {e}"));
            entry.conformance.clone()
        }
    };
    entry.conformance = conformance.clone();
    entry.tested_date = today.to_string();

    // 2. Context-window refresh (§4.2) — always re-queries /api/show. This is
    // the active (empirical) discovery driver, so it keeps the conservative
    // bootstrap-if-none behaviour; the passive session path
    // (`ensure_context_window`) delivers the trust-declared default.
    refresh_context_window(entry, endpoint, model, false, kind);

    // 3. Thinking probe + calibration bootstrap (§4.3 / §4.4).
    let mut emits_thinking = entry.emits_thinking.unwrap_or(false);
    match probe_thinking(endpoint, model, est) {
        Ok(pt) => {
            if pt.emits_thinking {
                entry.record_thinking_only();
                emits_thinking = true;
            }
            if let Some((observed, estimated)) = pt.calibration {
                entry.record_estimate_sample(observed, estimated);
            }
        }
        Err(e) => notes.push(format!("thinking probe failed: {e}")),
    }

    // 4. Optional empirical boundary search (§4.5).
    let boundary = if do_window {
        match probe_input_boundary(endpoint, model, entry, &mut progress, today, est) {
            Ok(outcome) => Some(outcome),
            Err(e) => {
                notes.push(format!("boundary search failed: {e}"));
                None
            }
        }
    } else {
        None
    };

    FullProbeReport {
        conformance,
        context_window: entry.context_window,
        emits_thinking,
        estimate_ratio: entry.estimate_ratio,
        boundary,
        notes,
    }
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// The minimal `list_dir` tool schema used in the probe request.
fn probe_tool_schema() -> serde_json::Value {
    serde_json::json!([{
        "type": "function",
        "function": {
            "name": "list_dir",
            "description": "List files in a directory",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path (use '.' for current directory)"
                    }
                },
                "required": ["path"]
            }
        }
    }])
}

/// Return `true` if `content` looks like a tool-call JSON object or array
/// embedded as text — the "text mode" conformance pattern.
pub fn looks_like_tool_call_json(content: &str) -> bool {
    let trimmed = content.trim();
    // Fast path: must contain both "name" and "arguments" keys.
    if !trimmed.contains("\"name\"") || !trimmed.contains("\"arguments\"") {
        return false;
    }
    // Try to parse as a JSON value and check its shape.
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let is_call =
            |v: &serde_json::Value| v.get("name").is_some() && v.get("arguments").is_some();
        if is_call(&val) {
            return true;
        }
        if val
            .as_array()
            .map(|a| a.iter().any(is_call))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Outcome of the cheap tool-conformance probe (Step 20.2 §4.1/§4.4):
/// the classification plus a calibration sample harvested from the same
/// request, mirroring [`ProbeThinking`]'s shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeConformance {
    /// How the model handled the one-tool request.
    pub conformance: ToolConformance,
    /// `(observed prompt_eval_count, estimated chars/4 tokens)` for the
    /// conformance request, `Some` only when the response carried
    /// `prompt_eval_count` — a calibration sample for
    /// [`CapabilityEntry::record_estimate_sample`] (§4.4). This is the
    /// largest of the three cheap-probe requests (it carries the tool
    /// schema), so harvesting it is the most informative of the bootstrap
    /// samples; dropping it (as the un-calibrated wrapper does) wastes the
    /// only request whose token count spans realistic tool-schema overhead.
    pub calibration: Option<(u32, usize)>,
}

/// Classify a parsed `/api/chat` `message` value into a [`ToolConformance`]
/// (Step 20.2 §4.1) — the pure decision, split from the HTTP so both the
/// public wrapper and the calibrated variant share one classifier.
fn classify_conformance(message: &serde_json::Value) -> ToolConformance {
    // Native: non-empty tool_calls array.
    if let Some(tcs) = message["tool_calls"].as_array() {
        if !tcs.is_empty() {
            return ToolConformance::Native;
        }
    }
    // TextMode: content parses as tool-call JSON.
    let content = message["content"].as_str().unwrap_or("");
    if looks_like_tool_call_json(content) {
        return ToolConformance::TextMode;
    }
    ToolConformance::NoTools
}

/// Send the minimal one-tool prompt, classify the response, and harvest a
/// calibration sample (Step 20.2 §4.1/§4.4) from the same request's
/// `prompt_eval_count` vs the request body's chars/4 estimate. 120 s timeout —
/// the model must already be warm. The conformance request is one of the
/// "those same requests" §4.4 names as a calibration source; this is the
/// variant [`full_probe`] uses so the tool-schema-bearing request is not
/// wasted. [`probe_tool_conformance`] stays the conformance-only public API.
pub async fn probe_tool_conformance_calibrated(
    endpoint: &str,
    model: &str,
    est: TokenEstimation,
) -> anyhow::Result<ProbeConformance> {
    let url = format!("{}/api/chat", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": [{
            "role": "user",
            "content": "Call the list_dir tool on path '.'. \
                        Do not explain — just call the tool."
        }],
        "tools": probe_tool_schema(),
        "stream": false,
    });
    // chars/4 estimate of the serialized request body (same currency the
    // agentic loop estimates in) — paired with the backend's real count.
    let estimated = serde_json::to_string(&body)
        .map(|s| est.tokens_for_chars(s.chars().count()))
        .unwrap_or(0);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("Ollama returned {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    let conformance = classify_conformance(&json["message"]);
    // (observed real prompt tokens, our chars/4 estimate) — the
    // (observed, estimated) order record_estimate_sample expects.
    let calibration = json["prompt_eval_count"]
        .as_u64()
        .map(|observed| (observed as u32, estimated));
    Ok(ProbeConformance {
        conformance,
        calibration,
    })
}

/// Send a minimal one-tool prompt and classify how the model responds.
/// Uses a 120 s timeout — the model must already be warm.
///
/// Conformance-only public API; [`probe_tool_conformance_calibrated`] is the
/// richer sibling [`full_probe`] uses to also harvest a §4.4 calibration
/// sample from the same request.
pub async fn probe_tool_conformance(
    endpoint: &str,
    model: &str,
    est: TokenEstimation,
) -> anyhow::Result<ToolConformance> {
    probe_tool_conformance_calibrated(endpoint, model, est)
        .await
        .map(|p| p.conformance)
}

// ---------------------------------------------------------------------------
// Table display
// ---------------------------------------------------------------------------

/// Print the full capabilities matrix to stdout.
pub fn print_capabilities_table(
    models: &[ModelInfo],
    cache: &CapabilityCache,
    active: &str,
    endpoint: &str,
    color: bool,
) {
    // This table is an Ollama (Multiplexer) view, so the capability key is the
    // bare model name — go through cap_key so the keying discipline lives in one
    // place rather than open-coding `&m.name`.
    let tested = models
        .iter()
        .filter(|m| cache.contains_key(&cap_key(newt_core::Serving::Multiplexer, "", &m.name)))
        .count();
    println!(
        "Models on {}  ({} total, {} tested)\n",
        endpoint,
        models.len(),
        tested,
    );

    // Column widths.
    let name_w = models
        .iter()
        .map(|m| m.name.len())
        .max()
        .unwrap_or(20)
        .max(20);

    // Header.
    let sep = "─".repeat(name_w);
    println!(
        "  {:<name_w$}  {:>6}  {:<8}  {:<5}  {:>8}  {:>8}  Conf  Tested",
        "Model", "Size", "Tool Use", "Think", "Ctx Win", "Safe Ctx"
    );
    println!("  {sep}  ──────  ────────  ─────  ────────  ────────  ────  ──────────");

    for m in models {
        let is_active = m.name == active;
        let active_tag = if is_active { " ◀" } else { "  " };
        let size = if m.param_size.is_empty() {
            "  —   ".to_string()
        } else {
            format!("{:>6}", m.param_size)
        };
        let (conformance_str, think_str, ctx_win_str, safe_ctx_str, conf_str, date_str) =
            match cache.get(&cap_key(newt_core::Serving::Multiplexer, "", &m.name)) {
                Some(e) => {
                    let ctx = e
                        .context_window
                        .map(|c| format!("{:>8}", fmt_k(c)))
                        .unwrap_or_else(|| "       —".to_string());
                    let safe = e
                        .safe_context
                        .map(|c| format!("{:>8}", fmt_k(c)))
                        .unwrap_or_else(|| "       —".to_string());
                    let conf = match e.tune_confidence {
                        TuneConfidence::None => "  — ".to_string(),
                        TuneConfidence::Low => " Low".to_string(),
                        TuneConfidence::Medium => " Med".to_string(),
                        TuneConfidence::High => "High".to_string(),
                    };
                    // A reasoning/"thinking" model: it has been observed
                    // returning chain-of-thought tokens (emits_thinking sticky).
                    let think = if e.emits_thinking == Some(true) {
                        "✓".to_string()
                    } else {
                        "—".to_string()
                    };
                    (
                        e.conformance.symbol().to_string(),
                        think,
                        ctx,
                        safe,
                        conf,
                        e.tested_date.clone(),
                    )
                }
                None => (
                    "—       ".to_string(),
                    "—".to_string(),
                    "       —".to_string(),
                    "       —".to_string(),
                    "  — ".to_string(),
                    "(untested)".to_string(),
                ),
            };

        let name = &m.name;
        let row = format!(
            "  {name:<name_w$}{active_tag}  {size}  {conformance_str}  {think_str:<5}  {ctx_win_str}  {safe_ctx_str}  {conf_str}  {date_str}"
        );
        if color && is_active {
            use crossterm::style::Color as CtColor;
            use crossterm::{
                execute,
                style::{Print, ResetColor, SetForegroundColor},
            };
            execute!(
                std::io::stdout(),
                SetForegroundColor(CtColor::Rgb {
                    r: 220,
                    g: 60,
                    b: 20
                }),
                Print(format!("{row}\n")),
                ResetColor,
            )
            .ok();
        } else {
            println!("{row}");
        }
    }

    println!();
    println!("Legend:");
    println!("  ✓ native  tool_calls field — works with this harness");
    println!("  ~ text    JSON embedded in content — NOT dispatched by newt");
    println!("  ✗ none    ignores tools, answers directly");
    println!("  —         untested  →  /probe <model> to classify");
    println!();
    println!("  Think ✓   reasoning model — emits chain-of-thought tokens (auto-detected;");
    println!("            newt streams it dimmed and keeps it out of the saved answer)");
    println!("  Ctx Win   declared context window from Ollama /api/show");
    println!("  Safe Ctx  num_ctx sent to Ollama (auto-tuned; human-overridable in config)");
    println!("  Conf      tuning confidence: None | Low | Med | High");
    println!();
    println!("Run /probe <model>       to test a model (warm-up included).");
    println!("Run /probe all           to test every untested model in sequence.");
    println!("Run /probe window <model> for an empirical input-boundary search (High confidence).");
}

/// Format a token count as a human-readable kilo string (e.g. 32768 → "32k").
fn fmt_k(n: u32) -> String {
    if n >= 1024 {
        format!("{}k", n / 1024)
    } else {
        n.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a capability key the way a Multiplexer (Ollama) backend would, so
    /// migrate/lookup tests can key by a bare model name through the ONE
    /// canonical constructor (no raw-String keys — that is now a type error).
    fn mk(name: &str) -> CapKey {
        cap_key(newt_core::Serving::Multiplexer, "", name)
    }

    #[test]
    fn looks_like_tool_call_json_native_object() {
        assert!(looks_like_tool_call_json(
            r#"{"name":"list_dir","arguments":{"path":"."}}"#
        ));
    }

    #[test]
    fn looks_like_tool_call_json_array() {
        assert!(looks_like_tool_call_json(
            r#"[{"name":"list_dir","arguments":{"path":"."}}]"#
        ));
    }

    #[test]
    fn looks_like_tool_call_json_plain_text() {
        assert!(!looks_like_tool_call_json(
            "Here are the files: README.md, src/"
        ));
    }

    #[test]
    fn looks_like_tool_call_json_incomplete_object() {
        // Has "name" but no "arguments" — not a tool call.
        assert!(!looks_like_tool_call_json(r#"{"name":"list_dir"}"#));
    }

    #[test]
    fn load_cache_returns_empty_on_missing_file() {
        // Can't mock the path, but at minimum it must not panic.
        let _ = load_cache();
    }

    #[test]
    fn conformance_symbol_coverage() {
        assert!(ToolConformance::Native.symbol().contains('✓'));
        assert!(ToolConformance::TextMode.symbol().contains('~'));
        assert!(ToolConformance::NoTools.symbol().contains('✗'));
    }

    #[test]
    fn tune_confidence_promotes_correctly() {
        assert_eq!(TuneConfidence::None.promote(), TuneConfidence::Low);
        assert_eq!(TuneConfidence::Low.promote(), TuneConfidence::Medium);
        assert_eq!(TuneConfidence::Medium.promote(), TuneConfidence::High);
        assert_eq!(TuneConfidence::High.promote(), TuneConfidence::High);
    }

    fn make_entry() -> CapabilityEntry {
        CapabilityEntry {
            conformance: ToolConformance::Native,
            tested_date: "2026-06-06".to_string(),
            context_window: Some(32768),
            safe_context: Some(26214),
            ..Default::default()
        }
    }

    #[test]
    fn record_success_updates_max_ok_input() {
        let mut e = make_entry();
        e.record_success(10_000, "2026-06-06");
        assert_eq!(e.max_ok_input, Some(10_000));
        e.record_success(8_000, "2026-06-06");
        // Lower value should not replace higher.
        assert_eq!(e.max_ok_input, Some(10_000));
    }

    #[test]
    fn record_success_promotes_confidence_after_five() {
        let mut e = make_entry();
        for i in 0..4 {
            e.record_success(5_000, "2026-06-06");
            assert_eq!(e.tune_confidence, TuneConfidence::None, "early iter {i}");
        }
        e.record_success(5_000, "2026-06-06");
        assert_eq!(e.tune_confidence, TuneConfidence::Low);
        assert_eq!(e.consecutive_ok, 0); // reset after promotion
    }

    #[test]
    fn record_overflow_reduces_safe_context() {
        let mut e = make_entry();
        e.record_overflow(30_000, "2026-06-06");
        // 30_000 * 75 / 100 = 22_500
        assert_eq!(e.safe_context, Some(22_500));
        assert_eq!(e.tune_confidence, TuneConfidence::Low);
        assert_eq!(e.overflow_at, Some(30_000));
    }

    /// Phase 20 (§2.1): overflow learning was inert because both budget
    /// resolvers prefer the larger figure and only `safe_context` was
    /// lowered — `record_overflow` must now rein `max_ok_input` too.
    #[test]
    fn record_overflow_reins_max_ok_input_down() {
        let mut e = make_entry();
        e.max_ok_input = Some(28_000);
        e.record_overflow(20_000, "2026-06-12");
        // 20_000 * 75% = 15_000 — both figures reined to the same cap.
        assert_eq!(e.safe_context, Some(15_000));
        assert_eq!(e.max_ok_input, Some(15_000));
        // A LOWER existing ratchet is untouched (never raised by overflow).
        let mut e2 = make_entry();
        e2.max_ok_input = Some(10_000);
        e2.record_overflow(20_000, "2026-06-12");
        assert_eq!(e2.max_ok_input, Some(10_000));
        // An absent ratchet stays absent — overflow proves no acceptance.
        let mut e3 = make_entry();
        e3.record_overflow(20_000, "2026-06-12");
        assert_eq!(e3.max_ok_input, None);
    }

    // --- record_accepted_prompt (Phase 20 §2.2) ---

    #[test]
    fn record_accepted_prompt_is_a_pure_high_water_ratchet() {
        let mut e = make_entry();
        e.consecutive_ok = 3;
        e.tune_confidence = TuneConfidence::Medium;
        assert!(
            e.record_accepted_prompt(8_734, "2026-06-12"),
            "first: dirty"
        );
        assert_eq!(e.max_ok_input, Some(8_734));
        assert_eq!(e.tune_date.as_deref(), Some("2026-06-12"), "date stamped");
        // Confidence accounting is the turn-level record_success's job — a
        // multi-round turn must not inflate it per round.
        assert_eq!(e.consecutive_ok, 3, "untouched");
        assert_eq!(e.tune_confidence, TuneConfidence::Medium, "untouched");
        // Equal or lower observations are not dirty and do not lower.
        assert!(
            !e.record_accepted_prompt(8_734, "2026-06-13"),
            "equal: clean"
        );
        assert!(
            !e.record_accepted_prompt(4_000, "2026-06-13"),
            "lower: clean"
        );
        assert_eq!(e.max_ok_input, Some(8_734), "HWM only raises");
        assert_eq!(e.tune_date.as_deref(), Some("2026-06-12"), "no re-stamp");
        // Strictly higher raises again.
        assert!(e.record_accepted_prompt(9_000, "2026-06-13"));
        assert_eq!(e.max_ok_input, Some(9_000));
        assert_eq!(e.tune_date.as_deref(), Some("2026-06-13"));
    }

    // --- record_estimate_sample (Phase 20 §2.3) ---

    #[test]
    fn record_estimate_sample_initializes_then_emas() {
        let mut e = make_entry();
        // Init: first sample is stored verbatim. 8_734 / 6_600 ≈ 1.3233…
        assert!(e.record_estimate_sample(8_734, 6_600));
        let first = e.estimate_ratio.unwrap();
        assert!((first - 8_734.0 / 6_600.0).abs() < 1e-6, "got {first}");
        // EMA: 0.75·old + 0.25·sample (sample 2.0 here).
        assert!(e.record_estimate_sample(2_000, 1_000));
        let second = e.estimate_ratio.unwrap();
        assert!(
            (second - (0.75 * first + 0.25 * 2.0)).abs() < 1e-6,
            "got {second}"
        );
    }

    #[test]
    fn record_estimate_sample_clamps_both_ends() {
        // A wild over-report clamps the SAMPLE to 3.0 before the EMA.
        let mut e = make_entry();
        assert!(e.record_estimate_sample(10_000, 1_000)); // raw 10.0
        assert_eq!(e.estimate_ratio, Some(3.0), "init clamped to 3.0");
        // A 0.5 raw sample is the under-report boundary: NOT skipped
        // (cache-hit skip is strictly below 0.5) and clamps to 0.5.
        let mut e2 = make_entry();
        assert!(e2.record_estimate_sample(500, 1_000));
        assert_eq!(e2.estimate_ratio, Some(0.5));
        // The EMA result is clamped too: stored value can never escape
        // [0.5, 3.0] no matter the history.
        assert!(!e.record_estimate_sample(10_000, 1_000), "3.0 → 3.0: clean");
        assert_eq!(e.estimate_ratio, Some(3.0));
    }

    #[test]
    fn record_estimate_sample_skips_cache_hits_and_zero_estimates() {
        let mut e = make_entry();
        // Ollama prompt-cache hit: observed < 0.5 × estimated — would poison
        // the ratio downward (spec §2.3). Skipped, nothing stored.
        assert!(!e.record_estimate_sample(400, 1_000));
        assert_eq!(e.estimate_ratio, None);
        // Zero estimate: no honest ratio exists.
        assert!(!e.record_estimate_sample(400, 0));
        assert_eq!(e.estimate_ratio, None);
        // And a skip never disturbs an already-learned ratio.
        e.estimate_ratio = Some(1.3);
        assert!(!e.record_estimate_sample(100, 1_000));
        assert_eq!(e.estimate_ratio, Some(1.3));
    }

    #[test]
    fn record_estimate_sample_dirty_only_above_threshold() {
        let mut e = make_entry();
        assert!(e.record_estimate_sample(1_300, 1_000)); // ratio 1.3
        let stored = e.estimate_ratio.unwrap();
        // A near-identical sample moves the EMA by ≪ 0.01: value updates
        // in memory but the call reports CLEAN (no save thrash).
        assert!(!e.record_estimate_sample(1_301, 1_000));
        let drifted = e.estimate_ratio.unwrap();
        assert!((drifted - stored).abs() < 0.01, "stored as-is, tiny drift");
        // A materially different sample (raw 2.0) moves the EMA by ~0.17.
        assert!(e.record_estimate_sample(2_000, 1_000));
    }

    // --- record_thinking_only (Phase 20 §2.1) ---

    #[test]
    fn record_thinking_only_is_sticky_and_dirty_once() {
        let mut e = make_entry();
        assert_eq!(e.emits_thinking, None);
        assert!(e.record_thinking_only(), "first observation: dirty");
        assert_eq!(e.emits_thinking, Some(true));
        assert!(!e.record_thinking_only(), "repeat: clean");
        assert_eq!(e.emits_thinking, Some(true));
    }

    // --- apply_observation (Phase 20 §2.2 dispatch seam) ---

    #[test]
    fn apply_observation_dispatches_each_variant() {
        let today = "2026-06-12";
        // Accepted → ratchet AND calibration sample (OR of both flags).
        let mut e = make_entry();
        let obs = newt_core::RoundObservation::Accepted {
            prompt_tokens: 8_734,
            estimated_tokens: 6_600,
        };
        assert!(apply_observation(&mut e, &obs, today));
        assert_eq!(e.max_ok_input, Some(8_734));
        assert!(e.estimate_ratio.is_some());
        // Same observation again: ratchet clean AND ratio drift below the
        // save threshold → overall clean.
        assert!(!apply_observation(&mut e, &obs, today));
        // Ratchet clean but the calibration sample materially different →
        // still dirty (the OR must not short-circuit the second record).
        let recal = newt_core::RoundObservation::Accepted {
            prompt_tokens: 8_000,
            estimated_tokens: 3_000,
        };
        assert!(apply_observation(&mut e, &recal, today));
        assert_eq!(e.max_ok_input, Some(8_734), "lower prompt: no ratchet");

        // SuspectedOverflow → record_overflow (always dirty, reins both).
        let mut e = make_entry();
        e.max_ok_input = Some(28_000);
        let obs = newt_core::RoundObservation::SuspectedOverflow {
            prompt_tokens: 20_000,
        };
        assert!(apply_observation(&mut e, &obs, today));
        assert_eq!(e.safe_context, Some(15_000));
        assert_eq!(e.max_ok_input, Some(15_000));
        assert_eq!(e.overflow_at, Some(20_000));

        // ThinkingOnly → sticky quirk.
        let mut e = make_entry();
        assert!(apply_observation(
            &mut e,
            &newt_core::RoundObservation::ThinkingOnly,
            today
        ));
        assert_eq!(e.emits_thinking, Some(true));
        assert!(!apply_observation(
            &mut e,
            &newt_core::RoundObservation::ThinkingOnly,
            today
        ));

        // A numbered context-window 400 updates the same in-memory entry as
        // the accepted retry, so the latter cannot overwrite recovered facts
        // from a separately loaded stale cache.
        let mut e = make_entry();
        e.context_window = Some(65_536);
        e.safe_context = Some(52_428);
        e.max_ok_input = Some(52_428);
        assert!(apply_observation(
            &mut e,
            &newt_core::RoundObservation::ContextWindow400 {
                context_window: 32_768,
            },
            today,
        ));
        assert_eq!(e.context_window, Some(32_768));
        assert_eq!(e.safe_context, Some(26_214));
        assert_eq!(e.max_ok_input, Some(26_214));
        assert!(apply_observation(
            &mut e,
            &newt_core::RoundObservation::Accepted {
                prompt_tokens: 1_000,
                estimated_tokens: 1_000,
            },
            today,
        ));
        assert_eq!(e.context_window, Some(32_768));
        assert_eq!(e.safe_context, Some(26_214));
    }

    /// New fields round-trip through JSON and stay absent (not `null`) when
    /// unset — additive format change, old caches parse unchanged.
    #[test]
    fn estimate_ratio_and_emits_thinking_roundtrip_json() {
        let mut e = make_entry();
        e.estimate_ratio = Some(1.29);
        e.emits_thinking = Some(true);
        let json = serde_json::to_string(&e).unwrap();
        let back: CapabilityEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.estimate_ratio, Some(1.29));
        assert_eq!(back.emits_thinking, Some(true));
        // Unset → keys skipped entirely.
        let bare = serde_json::to_string(&make_entry()).unwrap();
        assert!(!bare.contains("estimate_ratio"), "{bare}");
        assert!(!bare.contains("emits_thinking"), "{bare}");
    }

    #[test]
    fn record_overflow_does_not_increase_safe_context() {
        let mut e = make_entry();
        e.safe_context = Some(10_000);
        // Overflow at only 5_000 — 75% = 3_750; safe_context must shrink.
        e.record_overflow(5_000, "2026-06-06");
        assert_eq!(e.safe_context, Some(3_750));
        // A second overflow at a higher token count should not raise safe_context.
        e.record_overflow(40_000, "2026-06-06");
        // 40_000 * 75% = 30_000 > 3_750 → new_safe > old; plan says keep the lower.
        // Actually looking at the impl: changed = new_safe < current → false → skip.
        assert_eq!(e.safe_context, Some(3_750));
    }

    #[test]
    fn parse_context_window_error_none_for_unrelated_400() {
        let msg = "inference endpoint 400: invalid api key";
        assert_eq!(super::parse_context_window_error(msg), None);
    }

    #[test]
    fn parse_context_window_error_extracts_prompt_and_max() {
        // The real litellm body from issue #223, embedded in the harness's
        // "inference endpoint 400: <body>" wrapper.
        let msg = "inference endpoint 400: litellm.ContextWindowExceededError: prompt is too long: 5960028 tokens > 1000000 maximum";
        assert_eq!(
            super::parse_context_window_error(msg),
            Some((5_960_028, 1_000_000))
        );
    }

    #[test]
    fn parse_context_window_error_extracts_vllm_output_and_prompt_limits() {
        // Exact vLLM 0.19 validation wording when requested output plus prompt
        // input exceeds max_model_len (wrapped as Newt sees the HTTP 400 body).
        let msg = "inference endpoint 400 Bad Request: This model's maximum context length is 32768 tokens. However, you requested 16000 output tokens and your prompt contains 20000 input tokens, for a total of 36000 tokens (20000 + 16000 = 36000 > 32768). Please reduce the length of the input prompt or the number of requested output tokens.";
        assert_eq!(
            super::parse_context_window_error(msg),
            Some((20_000, 32_768))
        );
    }

    #[test]
    fn parse_context_window_error_extracts_vllm_input_only_limit() {
        // Exact sibling validation wording when the input alone reaches the
        // full model window.
        let msg = "inference endpoint 400 Bad Request: This model's maximum context length is 32768 tokens. However, your request has 33000 input tokens. Please reduce the length of the input messages.";
        assert_eq!(
            super::parse_context_window_error(msg),
            Some((33_000, 32_768))
        );
    }

    #[test]
    fn parse_context_window_error_none_without_max_clause() {
        // Truncated message missing the max half must not panic.
        let msg = "prompt is too long: 5960028 tokens";
        assert_eq!(super::parse_context_window_error(msg), None);
    }

    #[test]
    fn record_context_window_400_tightens_max_ok_input_to_80pct() {
        // Reproduces issue #223: max_ok_input was stale-high (251_640) while the
        // endpoint's real limit is 1_000_000. A 400 must pull the gate down.
        let mut e = make_entry();
        e.max_ok_input = Some(251_640);
        let dirty = e.record_context_window_400(1_000_000, "2026-06-08");
        assert!(dirty);
        assert_eq!(e.context_window, Some(1_000_000));
        // 1_000_000 * 80% = 800_000 (headroom below the hard max).
        assert_eq!(e.max_ok_input, Some(800_000));
        assert_eq!(e.tune_confidence, TuneConfidence::Low);
        assert_eq!(e.consecutive_ok, 0);
    }

    #[test]
    fn runtime_context_window_400_persists_the_configured_percentage_cap() {
        let mut e = make_entry();
        e.max_ok_input = Some(52_428);
        e.record_context_window_400_with_pct(32_768, 90, "2026-08-01");
        assert_eq!(e.context_window, Some(32_768));
        assert_eq!(e.hard_context_window, Some(32_768));
        assert_eq!(e.max_ok_input, Some(29_491));

        e.record_context_window_400_with_pct(65_536, 90, "2026-08-01");
        assert_eq!(
            e.hard_context_window,
            Some(32_768),
            "a later, larger error must not raise the persisted hard ceiling",
        );
        assert_eq!(e.context_window, Some(32_768));
        assert_eq!(e.max_ok_input, Some(29_491));

        let persisted = serde_json::to_string(&e).unwrap();
        let restored: CapabilityEntry = serde_json::from_str(&persisted).unwrap();
        assert_eq!(restored.hard_context_window, Some(32_768));
    }

    #[test]
    fn record_context_window_400_lowers_an_overshot_cap() {
        // When tuning had overshot (max_ok_input above the model's real max),
        // a 400 pulls the gate down to 80% of the reported limit.
        let mut e = make_entry();
        e.max_ok_input = Some(2_000_000);
        e.record_context_window_400(1_000_000, "2026-06-08");
        assert_eq!(e.max_ok_input, Some(800_000));
    }

    #[test]
    fn record_context_window_400_caps_safe_context_without_raising_it() {
        let mut e = make_entry();
        e.safe_context = Some(64_000); // small KV window
                                       // 80% of 1_000_000 = 800_000 > 64_000 → safe_context must NOT rise.
        e.record_context_window_400(1_000_000, "2026-06-08");
        assert_eq!(e.safe_context, Some(64_000));
    }

    #[test]
    fn fmt_k_formats_correctly() {
        assert_eq!(fmt_k(1024), "1k");
        assert_eq!(fmt_k(32768), "32k");
        assert_eq!(fmt_k(131072), "128k");
        assert_eq!(fmt_k(512), "512");
    }

    #[test]
    fn capability_entry_roundtrips_json_with_new_fields() {
        let mut e = make_entry();
        e.overflow_at = Some(28_000);
        e.max_ok_input = Some(25_000);
        e.tune_confidence = TuneConfidence::Medium;
        e.tune_date = Some("2026-06-06".to_string());
        let json = serde_json::to_string(&e).unwrap();
        let back: CapabilityEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.context_window, Some(32768));
        assert_eq!(back.overflow_at, Some(28_000));
        assert_eq!(back.tune_confidence, TuneConfidence::Medium);
    }

    #[test]
    fn capability_entry_deserializes_legacy_json_without_new_fields() {
        // Old cache entries only have conformance + tested_date.
        let legacy = r#"{"conformance":"native","tested_date":"2026-06-04"}"#;
        let e: CapabilityEntry = serde_json::from_str(legacy).unwrap();
        assert_eq!(e.conformance, ToolConformance::Native);
        assert_eq!(e.context_window, None);
        assert_eq!(e.tune_confidence, TuneConfidence::None);
        // Missing accounting_version means the double-counting regime —
        // NOT the current version that in-process Default entries get.
        assert_eq!(e.accounting_version, 0);
    }

    // --- migrate_accounting (Step 18.1 ratchet de-poison) ---

    /// The live poisoned entry from the B3 baseline: max_ok_input 25,602 at
    /// High confidence when the largest evaluated prompt was 4,748 tokens
    /// (and safe_context was 6,553 — provably impossible). Versionless →
    /// invalidated once; tuning that is honest either way survives.
    #[test]
    fn migrate_accounting_invalidates_poisoned_entry() {
        let mut cache = CapabilityCache::default();
        cache.insert(
            mk("llama3.1:8b"),
            CapabilityEntry {
                conformance: ToolConformance::Native,
                tested_date: "2026-06-08".into(),
                context_window: Some(8_192),
                hard_context_window: None,
                safe_context: Some(6_553),
                overflow_at: None,
                max_ok_input: Some(25_602),
                consecutive_ok: 3,
                tune_confidence: TuneConfidence::High,
                tune_date: Some("2026-06-08".into()),
                estimate_ratio: None,
                emits_thinking: None,
                accounting_version: 0, // pre-18.1 (missing in the JSON)
            },
        );
        assert!(
            migrate_accounting(&mut cache),
            "migration must report dirty"
        );
        let e = &cache[&mk("llama3.1:8b")];
        assert_eq!(e.max_ok_input, None, "poisoned ratchet value dropped");
        assert_eq!(e.consecutive_ok, 0);
        assert_eq!(e.tune_confidence, TuneConfidence::None);
        assert_eq!(e.accounting_version, ACCOUNTING_VERSION);
        // Non-ratchet state survives: the declared window and the
        // conservatively-derived safe_context are not regime-dependent.
        assert_eq!(e.context_window, Some(8_192));
        assert_eq!(e.safe_context, Some(6_553));
        assert_eq!(e.conformance, ToolConformance::Native);
    }

    /// A clean current-version entry — including the legitimate post-#223
    /// shape where max_ok_input (from the endpoint's reported hard limit)
    /// exceeds the VRAM-capped safe_context — must be left untouched.
    #[test]
    fn migrate_accounting_leaves_current_version_entry_untouched() {
        let mut cache = CapabilityCache::default();
        let entry = CapabilityEntry {
            conformance: ToolConformance::Native,
            tested_date: "2026-06-09".into(),
            safe_context: Some(64_000),
            max_ok_input: Some(800_000), // cw-400 discovery: legit > safe_context
            consecutive_ok: 2,
            tune_confidence: TuneConfidence::Medium,
            ..Default::default() // accounting_version = current
        };
        cache.insert(mk("hosted-model"), entry.clone());
        assert!(!migrate_accounting(&mut cache), "nothing to migrate");
        let e = &cache[&mk("hosted-model")];
        assert_eq!(e.max_ok_input, Some(800_000));
        assert_eq!(e.consecutive_ok, 2);
        assert_eq!(e.tune_confidence, TuneConfidence::Medium);
    }

    /// A versionless entry WITHOUT tuning values just gets stamped (still
    /// dirty — the stamp itself must persist so the check never re-runs).
    #[test]
    fn migrate_accounting_stamps_untuned_legacy_entry() {
        let mut cache = CapabilityCache::default();
        cache.insert(
            mk("old-model"),
            CapabilityEntry {
                conformance: ToolConformance::TextMode,
                tested_date: "2026-06-04".into(),
                accounting_version: 0,
                ..Default::default()
            },
        );
        assert!(migrate_accounting(&mut cache));
        assert_eq!(
            cache[&mk("old-model")].accounting_version,
            ACCOUNTING_VERSION
        );
        assert_eq!(
            cache[&mk("old-model")].conformance,
            ToolConformance::TextMode
        );
    }

    /// Running the migration twice must be a no-op the second time.
    #[test]
    fn migrate_accounting_is_idempotent() {
        let mut cache = CapabilityCache::default();
        let mut e = make_entry();
        e.max_ok_input = Some(25_602);
        e.accounting_version = 0;
        cache.insert(mk("m"), e);
        assert!(migrate_accounting(&mut cache), "first pass migrates");
        let snapshot = serde_json::to_string(&cache).unwrap();
        assert!(!migrate_accounting(&mut cache), "second pass is a no-op");
        assert_eq!(serde_json::to_string(&cache).unwrap(), snapshot);
    }

    // --- resolve_memory_budget (Step 18.2, #247) ---

    /// Fixture capability cache with one tuned entry for "tuned-model".
    fn fixture_entry(max_ok_input: Option<u32>, safe_context: Option<u32>) -> CapabilityEntry {
        CapabilityEntry {
            conformance: ToolConformance::Native,
            tested_date: "2026-06-10".into(),
            context_window: Some(32_768),
            safe_context,
            max_ok_input,
            ..Default::default()
        }
    }

    /// Tier 1: an explicit `[memory] context_tokens` is a deliberate user
    /// override — it wins even when capability data exists.
    #[test]
    fn resolve_memory_budget_explicit_config_wins() {
        let entry = fixture_entry(Some(24_000), Some(26_214));
        assert_eq!(
            resolve_memory_budget(Some(16_000), Some(1_000_000), Some(&entry)),
            16_000
        );
    }

    #[test]
    fn resolve_memory_budget_uses_the_active_models_declared_window() {
        let entry = fixture_entry(Some(24_000), None);
        assert_eq!(
            resolve_memory_budget(None, Some(1_000_000), Some(&entry)),
            1_000_000
        );
        // A newly selected model with its own declared window and no capability
        // entry yet: the declaration is the budget (the prior model's is gone).
        assert_eq!(
            resolve_memory_budget(None, Some(131_072), None),
            131_072,
            "the selected model's declaration replaces the prior model's budget"
        );
    }

    /// Tier 3a: without an override or declaration, the capability-derived
    /// budget is `max(max_ok_input, safe_context)` (Phase 20 §2.1) — here the
    /// proven figure exceeds the claim-derived one and wins.
    #[test]
    fn resolve_memory_budget_capability_max_ok_input_second() {
        let entry = fixture_entry(Some(24_000), Some(6_553));
        assert_eq!(resolve_memory_budget(None, None, Some(&entry)), 24_000);
    }

    /// Phase 20 §2.1: the high-water mark is a floor of proven-good, not a
    /// ceiling — when it sits BELOW the believed-safe window, `max()` keeps
    /// the budget at the window instead of shrinking it to the largest
    /// prompt merely seen so far (the motivating 6,068-vs-8,734 failure).
    #[test]
    fn resolve_memory_budget_max_keeps_safe_context_over_low_hwm() {
        let entry = fixture_entry(Some(6_068), Some(26_214));
        assert_eq!(resolve_memory_budget(None, None, Some(&entry)), 26_214);
    }

    /// Tier 3b: with no `max_ok_input` yet (e.g. freshly de-poisoned by the
    /// 18.1 migration), `safe_context` is the capability-derived budget.
    #[test]
    fn resolve_memory_budget_falls_back_to_safe_context() {
        let entry = fixture_entry(None, Some(6_553));
        assert_eq!(resolve_memory_budget(None, None, Some(&entry)), 6_553);
        // And the mirror: max_ok_input alone serves when safe_context is
        // absent (hosted endpoints discovered via cw-400 have no num_ctx).
        let entry = fixture_entry(Some(24_000), None);
        assert_eq!(resolve_memory_budget(None, None, Some(&entry)), 24_000);
    }

    /// Tier 4: the static default applies ONLY when no override, no declared
    /// window, and no empirical tuning exists — a cache MISS (fresh model), or
    /// an entry present but untuned. Cross-principal leakage is now impossible
    /// by construction: the caller resolves the entry via `cap_key`, so this
    /// function can never look up the wrong entry (see the cap_key keying
    /// tests for the instance-isolation guarantee).
    #[test]
    fn resolve_memory_budget_static_default_last() {
        // Cache miss (fresh model / unknown principal → the caller passes None).
        assert_eq!(
            resolve_memory_budget(None, None, None),
            newt_core::DEFAULT_CONTEXT_TOKENS
        );
        // Entry present (declared window known) but no empirical tuning.
        let untuned = fixture_entry(None, None);
        assert_eq!(
            resolve_memory_budget(None, None, Some(&untuned)),
            newt_core::DEFAULT_CONTEXT_TOKENS
        );
    }

    /// Regression for the pre-18.2 parallel default: the TUI built providers
    /// with `context_tokens.unwrap_or(8_192)`, silently ignoring probe data.
    /// A session with capability data must NOT resolve to the static
    /// default. (Phase 20 §2.1 updated the expected figure: the budget is
    /// now `max(max_ok_input, safe_context)` = 26,214, not the HWM alone.)
    #[test]
    fn resolve_memory_budget_never_ignores_probe_data() {
        let entry = fixture_entry(Some(24_000), Some(26_214));
        let budget = resolve_memory_budget(None, None, Some(&entry));
        assert_ne!(
            budget,
            newt_core::DEFAULT_CONTEXT_TOKENS,
            "capability data present — the static default must not win"
        );
        assert_eq!(budget, 26_214);
    }

    // -----------------------------------------------------------------------
    // Step 20.2 active discovery (docs/design/model-self-tuning.md §4)
    // -----------------------------------------------------------------------

    // --- refresh_context_window (§4.2) ---
    //
    // refresh_context_window's HTTP path (always-fetches, re-bootstrap only
    // when safe_context is unset, never auto-raises) needs a Tokio reactor
    // for the `/api/show` call, so it is covered end-to-end in
    // `tests/probe_integration.rs` — mirroring how `ensure_context_window`
    // is tested there rather than in this `#[cfg(test)]` block.

    // --- message_thinking_fields (§4.3) truth table ---

    #[test]
    fn message_thinking_fields_truth_table() {
        // Empty content + non-empty thinking → true.
        assert!(message_thinking_fields(&serde_json::json!({
            "content": "", "thinking": "reasoning here"
        })));
        // Whitespace content + non-empty reasoning → true.
        assert!(message_thinking_fields(&serde_json::json!({
            "content": "   \n", "reasoning": "x"
        })));
        // reasoning_content variant → true.
        assert!(message_thinking_fields(&serde_json::json!({
            "content": "", "reasoning_content": "y"
        })));
        // Missing content key (treated as empty) + thinking → true.
        assert!(message_thinking_fields(&serde_json::json!({
            "thinking": "z"
        })));
        // Non-empty content → false even with a thinking field.
        assert!(!message_thinking_fields(&serde_json::json!({
            "content": "ok", "thinking": "z"
        })));
        // Empty content but no/empty thinking fields → false.
        assert!(!message_thinking_fields(&serde_json::json!({
            "content": "", "thinking": "  "
        })));
        assert!(!message_thinking_fields(
            &serde_json::json!({"content": ""})
        ));
    }

    // --- build_padded_prompt (§4.5) sizing ---

    #[test]
    fn build_padded_prompt_sizes_near_target_across_ratios() {
        for &(target, ratio) in &[
            (512u32, 1.0f32),
            (2_048, 1.0),
            (8_000, 1.3),
            (4_096, 0.8),
            (16_000, 2.5),
        ] {
            let s = build_padded_prompt(target, ratio);
            let est = s.chars().count() as f32 / 4.0;
            let predicted_real = est * sanitize_ratio(ratio);
            let rel = (predicted_real - target as f32).abs() / target as f32;
            assert!(
                rel <= 0.10,
                "target={target} ratio={ratio}: chars/4*ratio={predicted_real} ({:.1}% off)",
                rel * 100.0
            );
        }
    }

    #[test]
    fn build_padded_prompt_sanitizes_bad_ratio_to_one() {
        // NaN / out-of-band ratios fall back to 1.0 (same as estimate-space).
        let s = build_padded_prompt(4_000, f32::NAN);
        let est = s.chars().count() as f32 / 4.0;
        assert!((est - 4_000.0).abs() / 4_000.0 <= 0.10);
        let s2 = build_padded_prompt(4_000, 99.0);
        let est2 = s2.chars().count() as f32 / 4.0;
        assert!((est2 - 4_000.0).abs() / 4_000.0 <= 0.10);
    }

    // --- classify_boundary_probe (§4.5) every arm ---

    #[test]
    fn classify_boundary_probe_accepted_when_eval_meets_threshold() {
        let json = serde_json::json!({
            "message": {"content": "ok"},
            "prompt_eval_count": 9_500,
            "eval_count": 3
        });
        // sent 10_000; 9_500 ≥ 90% → Accepted carrying the observed count.
        assert_eq!(
            classify_boundary_probe(Ok(&json), 10_000),
            BoundaryClass::Accepted {
                prompt_tokens: 9_500
            }
        );
    }

    #[test]
    fn classify_boundary_probe_accepted_via_tool_call_or_eval_count() {
        // Empty content but a tool call is still usable.
        let tc = serde_json::json!({
            "message": {"content": "", "tool_calls": [{"function": {"name": "x"}}]},
            "prompt_eval_count": 9_900
        });
        assert!(matches!(
            classify_boundary_probe(Ok(&tc), 10_000),
            BoundaryClass::Accepted { .. }
        ));
        // Empty content, no tool call, but eval_count > 0 → usable.
        let ec = serde_json::json!({
            "message": {"content": ""},
            "prompt_eval_count": 9_900,
            "eval_count": 5
        });
        assert!(matches!(
            classify_boundary_probe(Ok(&ec), 10_000),
            BoundaryClass::Accepted { .. }
        ));
    }

    #[test]
    fn classify_boundary_probe_truncated_when_eval_below_threshold() {
        // 200 but only 4_000 of 10_000 evaluated (head dropped) → Truncated.
        let json = serde_json::json!({
            "message": {"content": "ok"},
            "prompt_eval_count": 4_000,
            "eval_count": 2
        });
        assert_eq!(
            classify_boundary_probe(Ok(&json), 10_000),
            BoundaryClass::Truncated
        );
        // 200 with a usable body but no prompt_eval_count at all → Truncated
        // (we cannot confirm the prompt was evaluated).
        let no_count = serde_json::json!({"message": {"content": "ok"}, "eval_count": 1});
        assert_eq!(
            classify_boundary_probe(Ok(&no_count), 10_000),
            BoundaryClass::Truncated
        );
    }

    #[test]
    fn classify_boundary_probe_ctx_window_400() {
        let err = anyhow::anyhow!(
            "inference endpoint 400: litellm.ContextWindowExceededError: \
             prompt is too long: 12000 tokens > 8192 maximum"
        );
        assert_eq!(
            classify_boundary_probe(Err(&err), 12_000),
            BoundaryClass::CtxWindow400 { limit: 8_192 }
        );
    }

    #[test]
    fn classify_boundary_probe_inconclusive_for_other_errors() {
        let err = anyhow::anyhow!("request failed: connection reset");
        assert_eq!(
            classify_boundary_probe(Err(&err), 10_000),
            BoundaryClass::Inconclusive
        );
    }

    // --- is_tuning_stale (§4.6) boundaries ---

    #[test]
    fn is_tuning_stale_boundaries() {
        // None → stale.
        assert!(is_tuning_stale(None, "2026-06-13", 30));
        // Exactly max_age days old → NOT stale (strictly greater is stale).
        assert!(!is_tuning_stale(Some("2026-05-14"), "2026-06-13", 30));
        // One day over max_age → stale.
        assert!(is_tuning_stale(Some("2026-05-13"), "2026-06-13", 30));
        // Same day → fresh.
        assert!(!is_tuning_stale(Some("2026-06-13"), "2026-06-13", 30));
        // Unparseable stored date → treat as stale.
        assert!(is_tuning_stale(Some("not-a-date"), "2026-06-13", 30));
        // Unparseable `today` is also stale (defensive).
        assert!(is_tuning_stale(Some("2026-06-13"), "garbage", 30));
    }

    // --- sanitize_ratio ---

    #[test]
    fn sanitize_ratio_clamps_and_defaults() {
        assert_eq!(sanitize_ratio(1.3), 1.3);
        assert_eq!(sanitize_ratio(0.5), 0.5);
        assert_eq!(sanitize_ratio(3.0), 3.0);
        assert_eq!(sanitize_ratio(0.4), 1.0); // below band → default
        assert_eq!(sanitize_ratio(3.1), 1.0); // above band → default
        assert_eq!(sanitize_ratio(f32::NAN), 1.0);
        assert_eq!(sanitize_ratio(f32::INFINITY), 1.0);
    }

    // --- parse_show_response ---

    #[test]
    fn parse_show_response_reads_llama_key() {
        let json = serde_json::json!({"model_info": {"llama.context_length": 32768}});
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            Some(32768)
        );
    }

    #[test]
    fn parse_show_response_reads_nemotron_key() {
        let json = serde_json::json!({"model_info": {"nemotron_h_omni.context_length": 131072}});
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            Some(131072)
        );
    }

    #[test]
    fn parse_show_response_bare_context_length_key() {
        let json = serde_json::json!({"model_info": {"context_length": 8192}});
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            Some(8192)
        );
    }

    #[test]
    fn parse_show_response_modelfile_num_ctx_wins_when_smaller() {
        let json = serde_json::json!({
            "model_info": {"llama.context_length": 131072},
            "parameters": "num_ctx 32768\ntemperature 0.7"
        });
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            Some(32768)
        );
    }

    #[test]
    fn parse_show_response_arch_wins_when_num_ctx_larger() {
        let json = serde_json::json!({
            "model_info": {"llama.context_length": 4096},
            "parameters": "num_ctx 32768"
        });
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            Some(4096)
        );
    }

    #[test]
    fn parse_show_response_returns_none_when_no_keys() {
        let json = serde_json::json!({"model_info": {"general.architecture": "llama"}});
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            None
        );
    }

    #[test]
    fn parse_show_response_uses_minimum_when_multiple_arch_keys() {
        let json = serde_json::json!({
            "model_info": {
                "llama.context_length": 131072,
                "gemma.context_length": 8192
            }
        });
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            Some(8192)
        );
    }

    #[test]
    fn parse_show_response_modelfile_only_no_model_info() {
        // No model_info at all — the Modelfile num_ctx line is the only source.
        let json = serde_json::json!({
            "parameters": "stop \"<|end|>\"\nnum_ctx 16384\ntemperature 0.2"
        });
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            Some(16384)
        );
    }

    #[test]
    fn parse_show_response_ignores_unparsable_num_ctx() {
        // num_ctx value that isn't a u32 must be skipped, not panic.
        let json = serde_json::json!({"parameters": "num_ctx lots"});
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            None
        );
    }

    #[test]
    fn parse_show_response_parameters_without_num_ctx() {
        let json = serde_json::json!({"parameters": "temperature 0.7\ntop_p 0.9"});
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            None
        );
    }

    #[test]
    fn parse_show_response_non_numeric_context_length_ignored() {
        // A context_length that isn't a u64 (e.g. a string) must not match.
        let json = serde_json::json!({"model_info": {"llama.context_length": "32768"}});
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&json),
            None
        );
    }

    #[test]
    fn parse_show_response_empty_json() {
        assert_eq!(
            newt_core::backend_probe::parse_ollama_show_window(&serde_json::json!({})),
            None
        );
    }

    // --- probe_tool_schema ---

    #[test]
    fn probe_tool_schema_is_single_list_dir_function() {
        let schema = super::probe_tool_schema();
        let arr = schema.as_array().expect("schema is a JSON array");
        assert_eq!(arr.len(), 1, "probe uses exactly one tool");
        let f = &arr[0];
        assert_eq!(f["type"], "function");
        assert_eq!(f["function"]["name"], "list_dir");
        // The probe prompt tells the model to pass `path` — the schema must
        // declare it as a required string parameter or the probe is invalid.
        let params = &f["function"]["parameters"];
        assert_eq!(params["properties"]["path"]["type"], "string");
        assert_eq!(params["required"][0], "path");
    }

    // --- defaults ---

    #[test]
    fn capability_entry_default_is_untested_no_tools() {
        let e = CapabilityEntry::default();
        assert_eq!(e.conformance, ToolConformance::NoTools);
        assert!(e.tested_date.is_empty());
        assert_eq!(e.context_window, None);
        assert_eq!(e.safe_context, None);
        assert_eq!(e.overflow_at, None);
        assert_eq!(e.max_ok_input, None);
        assert_eq!(e.consecutive_ok, 0);
        assert_eq!(e.tune_confidence, TuneConfidence::None);
        assert_eq!(e.tune_date, None);
    }

    #[test]
    fn tune_confidence_default_is_none() {
        assert_eq!(TuneConfidence::default(), TuneConfidence::None);
    }

    // --- print_capabilities_table ---
    //
    // The table writes straight to stdout, so these tests can't assert on the
    // rendered text without refactoring production code (out of scope).  They
    // are edge-case exercises: every formatting branch (tested/untested,
    // every confidence level, missing ctx fields, active-row colouring, empty
    // model list hitting the `max().unwrap_or(20)` width fallback) must
    // complete without panicking.

    #[test]
    fn print_capabilities_table_handles_empty_model_list() {
        let cache = CapabilityCache::default();
        print_capabilities_table(&[], &cache, "none", "http://localhost:11434", false);
    }

    #[test]
    fn print_capabilities_table_renders_all_branches() {
        let mut cache = CapabilityCache::default();
        // Fully-populated entry at each confidence level.
        for (name, conf) in [
            ("m-none", TuneConfidence::None),
            ("m-low", TuneConfidence::Low),
            ("m-med", TuneConfidence::Medium),
            ("m-high", TuneConfidence::High),
        ] {
            let mut e = make_entry();
            e.tune_confidence = conf;
            cache.insert(mk(name), e);
        }
        // Tested entry with no ctx data (the `—` placeholders).
        cache.insert(
            mk("m-noctx"),
            CapabilityEntry {
                conformance: ToolConformance::TextMode,
                tested_date: "2026-06-06".to_string(),
                ..Default::default()
            },
        );
        // A reasoning model: emits_thinking → the ✓ in the Think column.
        cache.insert(
            mk("m-think"),
            CapabilityEntry {
                conformance: ToolConformance::Native,
                tested_date: "2026-06-10".to_string(),
                emits_thinking: Some(true),
                ..Default::default()
            },
        );
        let models: Vec<ModelInfo> = [
            ("m-none", "7B"),
            ("m-low", "13B"),
            ("m-med", ""),
            ("m-high", "32.8B"),
            ("m-noctx", "3B"),
            ("m-think", "30B"),
            ("m-untested", "1B"),
        ]
        .into_iter()
        .map(|(n, s)| ModelInfo {
            name: n.to_string(),
            param_size: s.to_string(),
        })
        .collect();
        // Plain path, with an active row.
        print_capabilities_table(&models, &cache, "m-low", "http://localhost:11434", false);
        // Colour path for the active row (execute! to stdout).
        print_capabilities_table(&models, &cache, "m-high", "http://localhost:11434", true);
        // Active model not in list — no row gets the active tag.
        print_capabilities_table(&models, &cache, "absent", "http://localhost:11434", true);
    }
}
