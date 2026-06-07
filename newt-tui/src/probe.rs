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
}

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

    /// Record an overflow (empty response at `input_tokens` tokens).
    /// Reduces `safe_context` to 75 % of the overflow point.
    /// Returns `true` if state changed (caller should save cache).
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
        true // always dirty after overflow
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
pub fn load_cache() -> CapabilityCache {
    let Some(path) = cache_path() else {
        return Default::default();
    };
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Default::default();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

/// Persist the capability cache to disk (best-effort).
pub fn save_cache(cache: &CapabilityCache) {
    let Some(path) = cache_path() else { return };
    if let Ok(data) = serde_json::to_string_pretty(cache) {
        let _ = std::fs::write(path, data);
    }
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
            overflow_at: None,
            max_ok_input: None,
            consecutive_ok: 0,
            tune_confidence: TuneConfidence::None,
            tune_date: None,
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
}
