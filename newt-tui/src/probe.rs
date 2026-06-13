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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityEntry {
    pub conformance: ToolConformance,
    /// ISO-8601 date (YYYY-MM-DD) the probe was last run.
    pub tested_date: String,

    // --- Context window tuning (all optional for backward compat) ---
    /// Model's declared maximum context length from Ollama `/api/show`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,

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
    /// maximum input size as `hard_limit` tokens.
    ///
    /// Sets `max_ok_input` to 80 % of the reported limit (leaving headroom for
    /// the chars/4 estimate's inaccuracy) so the pre-send guard trims future
    /// requests *before* they are dispatched, and persists the discovery so
    /// later sessions don't repeat the same crash. Confidence drops to `Low`
    /// because the previous tuning clearly overshot. See issue #223.
    ///
    /// Returns `true` if state changed (caller should save cache).
    pub fn record_context_window_400(&mut self, hard_limit: u32, today: &str) -> bool {
        // The reported `hard_limit` is authoritative about the model's true
        // ceiling, so set the pre-send gate to 80 % of it directly — even when
        // that raises a previously-low `max_ok_input` (issue #223 saw a stale
        // 251_640 while the real max was 1_000_000, so the gate must move up to
        // ~800_000, not stay needlessly tiny).
        let new_cap = (hard_limit as u64 * 80 / 100) as u32;
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
        newt_core::RoundObservation::ThinkingOnly => entry.record_thinking_only(),
    }
}

/// The full cache: model name → capability entry.
pub type CapabilityCache = HashMap<String, CapabilityEntry>;

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

fn cache_path() -> Option<PathBuf> {
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
    for (model, entry) in cache.iter_mut() {
        if entry.accounting_version >= ACCOUNTING_VERSION {
            continue;
        }
        if entry.max_ok_input.is_some() {
            tracing::info!(
                model,
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
/// (`TokenBudget` / `Summarizing`) at construction.
///
/// Precedence:
/// 1. **Explicit `[memory] context_tokens`** — a deliberate user override;
///    always honoured.
/// 2. **Capability-derived** — the empirical probe cache entry for `model`:
///    `max(max_ok_input, safe_context)` when both exist, else whichever
///    exists (Phase 20, `docs/design/model-self-tuning.md` §2.1 — the table
///    is the contract). `max_ok_input` is a high-water mark of PROVEN-good
///    input — a floor, not a ceiling — so it must never pull the budget
///    below the believed-safe window; conversely a prompt proven beyond the
///    claim outranks it. The cw-400 path reins `safe_context` to its
///    authoritative cap, so `max()` still lands on the authoritative number
///    after a hard 400. The declared `context_window` is deliberately NOT a
///    source: it is a claim, not a measurement.
/// 3. **Static default** — [`newt_core::DEFAULT_CONTEXT_TOKENS`] only when
///    neither exists (fresh model, no probe data yet).
///
/// The resolved value is injected by value at provider construction —
/// newt-core has no dependency on the probe types (crate-boundary note in
/// the Phase 18 design). Budgets therefore refresh per session: if the
/// capability cache ratchets mid-session, providers keep their
/// construction-time value while the agentic loop's own guard tracks the
/// live numbers.
pub fn resolve_memory_budget(explicit: Option<u32>, cache: &CapabilityCache, model: &str) -> u32 {
    explicit
        .or_else(|| {
            cache
                .get(model)
                .and_then(|e| match (e.max_ok_input, e.safe_context) {
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
pub fn fetch_context_window(endpoint: &str, model: &str) -> Option<u32> {
    let url = format!("{}/api/show", endpoint.trim_end_matches('/'));
    let body = serde_json::json!({"name": model});
    let json: serde_json::Value = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()?
                .post(&url)
                .json(&body)
                .send()
                .await
                .ok()?
                .json::<serde_json::Value>()
                .await
                .ok()
        })
    })?;

    parse_show_response(&json)
}

/// Extract the context window from a parsed `/api/show` response.
/// Separated from the HTTP call so it can be unit-tested without a server.
pub(crate) fn parse_show_response(json: &serde_json::Value) -> Option<u32> {
    // 1. Architecture limit from model_info.
    // Ollama returns the field as "model_info" (with underscore). The key name
    // is architecture-prefixed (e.g. "llama.context_length",
    // "nemotron_h_omni.context_length") — scan for any key ending in
    // ".context_length" so new architectures work without code changes.
    let arch_limit: Option<u32> = json["model_info"].as_object().and_then(|info| {
        // Exact bare key first (unlikely but defensive).
        if let Some(v) = info.get("context_length").and_then(|v| v.as_u64()) {
            return Some(v as u32);
        }
        // Any architecture-prefixed key ending in ".context_length".
        info.iter()
            .filter(|(k, _)| k.ends_with(".context_length"))
            .filter_map(|(_, v)| v.as_u64())
            .map(|v| v as u32)
            .min() // take the smallest if there are multiple (conservative)
    });

    // 2. Modelfile `num_ctx` parameter line (user override, takes precedence if smaller).
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
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Ensure `entry` has a `context_window` and an initial `safe_context`.
/// Calls `/api/show` only when the context window is not yet known.
/// Returns `true` if the entry was updated (caller should save cache).
pub fn ensure_context_window(entry: &mut CapabilityEntry, endpoint: &str, model: &str) -> bool {
    if entry.context_window.is_some() {
        return false;
    }
    let Some(window) = fetch_context_window(endpoint, model) else {
        return false;
    };
    entry.context_window = Some(window);
    // Bootstrap safe_context at 80 % of declared max unless already set.
    if entry.safe_context.is_none() {
        entry.safe_context = Some(window * 80 / 100);
    }
    true
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

/// Parse a context-window-exceeded error and extract `(prompt_tokens,
/// max_tokens)`.
///
/// Hosted endpoints (NVIDIA inference API → LiteLLM → Bedrock/Anthropic)
/// surface context overflow as an HTTP 400 whose body contains a message like:
///
/// ```text
/// litellm.ContextWindowExceededError: prompt is too long: 5960028 tokens > 1000000 maximum
/// ```
///
/// The error body is embedded in the harness's `"inference endpoint 400: <body>"`
/// string, so this scans the whole message for the `prompt is too long: …`
/// pattern (the `N` and `M` numbers) rather than parsing structured JSON.
/// Returns `None` when the pattern is absent (the 400 was for some other
/// reason). See issue #223.
pub fn parse_context_window_error(msg: &str) -> Option<(u64, u64)> {
    // Anchor on the stable phrase; tolerate surrounding JSON/escaping.
    let after = msg.split("prompt is too long:").nth(1)?;
    let prompt = first_number(after)?;
    let after_gt = after.split('>').nth(1)?;
    let max = first_number(after_gt)?;
    Some((prompt, max))
}

/// Return the first run of ASCII digits in `s` parsed as `u64`, if any.
fn first_number(s: &str) -> Option<u64> {
    s.split(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())
        .and_then(|t| t.parse().ok())
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

/// Send a minimal one-tool prompt and classify how the model responds.
/// Uses a 120 s timeout — the model must already be warm.
pub async fn probe_tool_conformance(
    endpoint: &str,
    model: &str,
) -> anyhow::Result<ToolConformance> {
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
    let message = &json["message"];

    // Native: non-empty tool_calls array.
    if let Some(tcs) = message["tool_calls"].as_array() {
        if !tcs.is_empty() {
            return Ok(ToolConformance::Native);
        }
    }

    // TextMode: content parses as tool-call JSON.
    let content = message["content"].as_str().unwrap_or("");
    if looks_like_tool_call_json(content) {
        return Ok(ToolConformance::TextMode);
    }

    Ok(ToolConformance::NoTools)
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
    let tested = models
        .iter()
        .filter(|m| cache.contains_key(&m.name))
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
        "  {:<name_w$}  {:>6}  {:<8}  {:>8}  {:>8}  Conf  Tested",
        "Model", "Size", "Tool Use", "Ctx Win", "Safe Ctx"
    );
    println!("  {sep}  ──────  ────────  ────────  ────────  ────  ──────────");

    for m in models {
        let is_active = m.name == active;
        let active_tag = if is_active { " ◀" } else { "  " };
        let size = if m.param_size.is_empty() {
            "  —   ".to_string()
        } else {
            format!("{:>6}", m.param_size)
        };
        let (conformance_str, ctx_win_str, safe_ctx_str, conf_str, date_str) =
            match cache.get(&m.name) {
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
                    (
                        e.conformance.symbol().to_string(),
                        ctx,
                        safe,
                        conf,
                        e.tested_date.clone(),
                    )
                }
                None => (
                    "—       ".to_string(),
                    "       —".to_string(),
                    "       —".to_string(),
                    "  — ".to_string(),
                    "(untested)".to_string(),
                ),
            };

        let name = &m.name;
        let row = format!(
            "  {name:<name_w$}{active_tag}  {size}  {conformance_str}  {ctx_win_str}  {safe_ctx_str}  {conf_str}  {date_str}"
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
    println!("  Ctx Win   declared context window from Ollama /api/show");
    println!("  Safe Ctx  num_ctx sent to Ollama (auto-tuned; human-overridable in config)");
    println!("  Conf      tuning confidence: None | Low | Med | High");
    println!();
    println!("Run /probe <model> to test a model (warm-up included).");
    println!("Run /probe all    to test every untested model in sequence.");
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
        // 1_000_000 * 80% = 800_000 (headroom below the hard max).
        assert_eq!(e.max_ok_input, Some(800_000));
        assert_eq!(e.tune_confidence, TuneConfidence::Low);
        assert_eq!(e.consecutive_ok, 0);
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
            "llama3.1:8b".into(),
            CapabilityEntry {
                conformance: ToolConformance::Native,
                tested_date: "2026-06-08".into(),
                context_window: Some(8_192),
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
        let e = &cache["llama3.1:8b"];
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
        cache.insert("hosted-model".into(), entry.clone());
        assert!(!migrate_accounting(&mut cache), "nothing to migrate");
        let e = &cache["hosted-model"];
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
            "old-model".into(),
            CapabilityEntry {
                conformance: ToolConformance::TextMode,
                tested_date: "2026-06-04".into(),
                accounting_version: 0,
                ..Default::default()
            },
        );
        assert!(migrate_accounting(&mut cache));
        assert_eq!(cache["old-model"].accounting_version, ACCOUNTING_VERSION);
        assert_eq!(cache["old-model"].conformance, ToolConformance::TextMode);
    }

    /// Running the migration twice must be a no-op the second time.
    #[test]
    fn migrate_accounting_is_idempotent() {
        let mut cache = CapabilityCache::default();
        let mut e = make_entry();
        e.max_ok_input = Some(25_602);
        e.accounting_version = 0;
        cache.insert("m".into(), e);
        assert!(migrate_accounting(&mut cache), "first pass migrates");
        let snapshot = serde_json::to_string(&cache).unwrap();
        assert!(!migrate_accounting(&mut cache), "second pass is a no-op");
        assert_eq!(serde_json::to_string(&cache).unwrap(), snapshot);
    }

    // --- resolve_memory_budget (Step 18.2, #247) ---

    /// Fixture capability cache with one tuned entry for "tuned-model".
    fn fixture_cache(max_ok_input: Option<u32>, safe_context: Option<u32>) -> CapabilityCache {
        let mut cache = CapabilityCache::default();
        cache.insert(
            "tuned-model".into(),
            CapabilityEntry {
                conformance: ToolConformance::Native,
                tested_date: "2026-06-10".into(),
                context_window: Some(32_768),
                safe_context,
                max_ok_input,
                ..Default::default()
            },
        );
        cache
    }

    /// Tier 1: an explicit `[memory] context_tokens` is a deliberate user
    /// override — it wins even when capability data exists.
    #[test]
    fn resolve_memory_budget_explicit_config_wins() {
        let cache = fixture_cache(Some(24_000), Some(26_214));
        assert_eq!(
            resolve_memory_budget(Some(16_000), &cache, "tuned-model"),
            16_000
        );
    }

    /// Tier 2a: without an override, the capability-derived budget is
    /// `max(max_ok_input, safe_context)` (Phase 20 §2.1) — here the proven
    /// figure exceeds the claim-derived one and wins.
    #[test]
    fn resolve_memory_budget_capability_max_ok_input_second() {
        let cache = fixture_cache(Some(24_000), Some(6_553));
        assert_eq!(resolve_memory_budget(None, &cache, "tuned-model"), 24_000);
    }

    /// Phase 20 §2.1: the high-water mark is a floor of proven-good, not a
    /// ceiling — when it sits BELOW the believed-safe window, `max()` keeps
    /// the budget at the window instead of shrinking it to the largest
    /// prompt merely seen so far (the motivating 6,068-vs-8,734 failure).
    #[test]
    fn resolve_memory_budget_max_keeps_safe_context_over_low_hwm() {
        let cache = fixture_cache(Some(6_068), Some(26_214));
        assert_eq!(resolve_memory_budget(None, &cache, "tuned-model"), 26_214);
    }

    /// Tier 2b: with no `max_ok_input` yet (e.g. freshly de-poisoned by the
    /// 18.1 migration), `safe_context` is the capability-derived budget.
    #[test]
    fn resolve_memory_budget_falls_back_to_safe_context() {
        let cache = fixture_cache(None, Some(6_553));
        assert_eq!(resolve_memory_budget(None, &cache, "tuned-model"), 6_553);
        // And the mirror: max_ok_input alone serves when safe_context is
        // absent (hosted endpoints discovered via cw-400 have no num_ctx).
        let cache = fixture_cache(Some(24_000), None);
        assert_eq!(resolve_memory_budget(None, &cache, "tuned-model"), 24_000);
    }

    /// Tier 3: the static default applies ONLY when neither an override nor
    /// any empirical tuning exists — unknown model, or an entry with no
    /// tuning data. The declared `context_window` alone is a claim, not a
    /// measurement, and must not become a budget.
    #[test]
    fn resolve_memory_budget_static_default_last() {
        // Model absent from the cache entirely (fresh model, never probed).
        let empty = CapabilityCache::default();
        assert_eq!(
            resolve_memory_budget(None, &empty, "fresh-model"),
            newt_core::DEFAULT_CONTEXT_TOKENS
        );
        // Entry exists (declared window known) but no empirical tuning.
        let untuned = fixture_cache(None, None);
        assert_eq!(
            resolve_memory_budget(None, &untuned, "tuned-model"),
            newt_core::DEFAULT_CONTEXT_TOKENS
        );
        // Some OTHER model's tuning must not leak onto this one.
        let cache = fixture_cache(Some(24_000), Some(26_214));
        assert_eq!(
            resolve_memory_budget(None, &cache, "different-model"),
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
        let cache = fixture_cache(Some(24_000), Some(26_214));
        let budget = resolve_memory_budget(None, &cache, "tuned-model");
        assert_ne!(
            budget,
            newt_core::DEFAULT_CONTEXT_TOKENS,
            "capability data present — the static default must not win"
        );
        assert_eq!(budget, 26_214);
    }

    // --- parse_show_response ---

    #[test]
    fn parse_show_response_reads_llama_key() {
        let json = serde_json::json!({"model_info": {"llama.context_length": 32768}});
        assert_eq!(super::parse_show_response(&json), Some(32768));
    }

    #[test]
    fn parse_show_response_reads_nemotron_key() {
        let json = serde_json::json!({"model_info": {"nemotron_h_omni.context_length": 131072}});
        assert_eq!(super::parse_show_response(&json), Some(131072));
    }

    #[test]
    fn parse_show_response_bare_context_length_key() {
        let json = serde_json::json!({"model_info": {"context_length": 8192}});
        assert_eq!(super::parse_show_response(&json), Some(8192));
    }

    #[test]
    fn parse_show_response_modelfile_num_ctx_wins_when_smaller() {
        let json = serde_json::json!({
            "model_info": {"llama.context_length": 131072},
            "parameters": "num_ctx 32768\ntemperature 0.7"
        });
        assert_eq!(super::parse_show_response(&json), Some(32768));
    }

    #[test]
    fn parse_show_response_arch_wins_when_num_ctx_larger() {
        let json = serde_json::json!({
            "model_info": {"llama.context_length": 4096},
            "parameters": "num_ctx 32768"
        });
        assert_eq!(super::parse_show_response(&json), Some(4096));
    }

    #[test]
    fn parse_show_response_returns_none_when_no_keys() {
        let json = serde_json::json!({"model_info": {"general.architecture": "llama"}});
        assert_eq!(super::parse_show_response(&json), None);
    }

    #[test]
    fn parse_show_response_uses_minimum_when_multiple_arch_keys() {
        let json = serde_json::json!({
            "model_info": {
                "llama.context_length": 131072,
                "gemma.context_length": 8192
            }
        });
        assert_eq!(super::parse_show_response(&json), Some(8192));
    }

    #[test]
    fn parse_show_response_modelfile_only_no_model_info() {
        // No model_info at all — the Modelfile num_ctx line is the only source.
        let json = serde_json::json!({
            "parameters": "stop \"<|end|>\"\nnum_ctx 16384\ntemperature 0.2"
        });
        assert_eq!(super::parse_show_response(&json), Some(16384));
    }

    #[test]
    fn parse_show_response_ignores_unparsable_num_ctx() {
        // num_ctx value that isn't a u32 must be skipped, not panic.
        let json = serde_json::json!({"parameters": "num_ctx lots"});
        assert_eq!(super::parse_show_response(&json), None);
    }

    #[test]
    fn parse_show_response_parameters_without_num_ctx() {
        let json = serde_json::json!({"parameters": "temperature 0.7\ntop_p 0.9"});
        assert_eq!(super::parse_show_response(&json), None);
    }

    #[test]
    fn parse_show_response_non_numeric_context_length_ignored() {
        // A context_length that isn't a u64 (e.g. a string) must not match.
        let json = serde_json::json!({"model_info": {"llama.context_length": "32768"}});
        assert_eq!(super::parse_show_response(&json), None);
    }

    #[test]
    fn parse_show_response_empty_json() {
        assert_eq!(super::parse_show_response(&serde_json::json!({})), None);
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
            cache.insert(name.to_string(), e);
        }
        // Tested entry with no ctx data (the `—` placeholders).
        cache.insert(
            "m-noctx".to_string(),
            CapabilityEntry {
                conformance: ToolConformance::TextMode,
                tested_date: "2026-06-06".to_string(),
                ..Default::default()
            },
        );
        let models: Vec<ModelInfo> = [
            ("m-none", "7B"),
            ("m-low", "13B"),
            ("m-med", ""),
            ("m-high", "32.8B"),
            ("m-noctx", "3B"),
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
