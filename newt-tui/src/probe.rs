//! Tool-conformance probing for Ollama models.
//!
//! Sends a minimal test request to classify how a model handles tool calls:
//! - `Native`   — uses Ollama's `tool_calls` field correctly
//! - `TextMode` — embeds tool-call JSON in the `content` field as text
//! - `NoTools`  — ignores tools and answers with plain text
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

/// One row in the capability cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityEntry {
    pub conformance: ToolConformance,
    /// ISO-8601 date (YYYY-MM-DD) the probe was last run.
    pub tested_date: String,
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
        "  {:<name_w$}  {:>6}  {:<8}  Tested",
        "Model", "Size", "Tool Use"
    );
    println!("  {sep}  ──────  ────────  ──────────");

    for m in models {
        let is_active = m.name == active;
        let active_tag = if is_active { " ◀" } else { "  " };
        let size = if m.param_size.is_empty() {
            "  —   ".to_string()
        } else {
            format!("{:>6}", m.param_size)
        };
        let (conformance_str, date_str) = match cache.get(&m.name) {
            Some(e) => (e.conformance.symbol().to_string(), e.tested_date.clone()),
            None => ("—       ".to_string(), "(untested)".to_string()),
        };

        let name = &m.name;
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
                Print(format!(
                    "  {name:<name_w$}{active_tag}  {size}  {conformance_str}  {date_str}\n"
                )),
                ResetColor,
            )
            .ok();
        } else {
            println!("  {name:<name_w$}{active_tag}  {size}  {conformance_str}  {date_str}");
        }
    }

    println!();
    println!("Legend:");
    println!("  ✓ native  tool_calls field — works with this harness");
    println!("  ~ text    JSON embedded in content — NOT dispatched by newt");
    println!("  ✗ none    ignores tools, answers directly");
    println!("  —         untested  →  /probe <model> to classify");
    println!();
    println!("Run /probe <model> to test a model (warm-up included).");
    println!("Run /probe all    to test every untested model in sequence.");
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
}
