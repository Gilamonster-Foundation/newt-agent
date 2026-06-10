//! Structural prune — zero-LLM context compression over the wire-shape
//! message list (Step 18.3, issue #247).
//!
//! Pure functions over the `serde_json::Value` message shape the TUI loop
//! sends to Ollama / OpenAI-compatible backends: objects with `role` /
//! `content` / `tool_calls`, and `role: "tool"` results (with a
//! `tool_call_id` on the OpenAI path, positional on the Ollama path).
//!
//! Three passes, in the hermes-proven order (see
//! `docs/design/context-memory-hermes-learnings.md`, gap-matrix row
//! "Structural pruning before LLM summary"):
//!
//! 1. [`collapse_duplicate_tool_results`] — identical oversized tool results
//!    are collapsed: later occurrences become a one-liner referencing the
//!    first.
//! 2. [`summarize_aged_tool_results`] — tool results outside the protected
//!    tail are replaced with informative per-tool one-liners
//!    (`[run_command] ran 'npm test' -> ok, 47 lines output`).
//! 3. [`shrink_tool_call_args`] — oversized `function.arguments` are parsed,
//!    truncated *inside* the JSON structure (long string values), and
//!    reserialized, so the output is always valid JSON. Hermes learned the
//!    hard way that naive byte-slicing causes provider 400-loops
//!    (hermes #11762).
//!
//! Invariants (enforced by construction, verified by the property tests):
//! no message is ever added or removed, roles and `tool_call_id`s are never
//! touched, the last [`PruneConfig::keep_last`] messages are byte-identical,
//! a replacement is only applied when it is strictly shorter (serialized),
//! and `prune(prune(x)) == prune(x)`.
//!
//! Nothing calls this module yet — Step 18.4 wires it into the compression
//! pipeline after Step 9.7 lands the `agentic` module. Hashing uses `blake3`,
//! which is already in newt-core's build graph via `agent-mesh-protocol`.

use serde_json::Value;
use std::collections::HashMap;

/// Floor for [`PruneConfig::arg_string_cap`]. The truncation marker
/// (`… [+N chars]`) needs at most 31 chars even for a `usize::MAX` count, so
/// any cap >= 32 guarantees truncated strings land at or under the cap —
/// which is what makes [`shrink_tool_call_args`] idempotent.
const MIN_ARG_STRING_CAP: usize = 32;

/// Thresholds for the three prune passes. All sizes are in characters.
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Protected tail: the last `keep_last` messages are never modified
    /// (byte-identical in the output). "Aged" means anything before them.
    pub keep_last: usize,
    /// Pass 1 only collapses tool results strictly longer than this.
    pub dedupe_min_chars: usize,
    /// Pass 2 only summarizes tool results strictly longer than this.
    pub summarize_min_chars: usize,
    /// Pass 3 only rewrites `function.arguments` whose serialized form is
    /// strictly longer than this.
    pub args_max_chars: usize,
    /// Per-string cap applied *inside* parsed argument structures by pass 3.
    /// Values below [`MIN_ARG_STRING_CAP`] are raised to it.
    pub arg_string_cap: usize,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            keep_last: 10,
            dedupe_min_chars: 200,
            summarize_min_chars: 200,
            args_max_chars: 500,
            arg_string_cap: 200,
        }
    }
}

impl PruneConfig {
    fn effective_arg_string_cap(&self) -> usize {
        self.arg_string_cap.max(MIN_ARG_STRING_CAP)
    }

    /// Index of the first protected-tail message; `[0, aged)` may be edited.
    fn aged_len(&self, total: usize) -> usize {
        total.saturating_sub(self.keep_last)
    }
}

/// Result of [`prune`]: the rewritten message list plus exact accounting of
/// how many serialized characters the three passes reclaimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneOutcome {
    /// The pruned message list (same length and order as the input).
    pub messages: Vec<Value>,
    /// Exactly `serialized_len(input) - serialized_len(output)`, where
    /// `serialized_len` is the summed `serde_json::to_string` length of each
    /// message. Never negative: passes only apply strictly-shorter rewrites.
    pub chars_reclaimed: usize,
}

/// Run all three structural-prune passes in order.
///
/// ```
/// use newt_core::prune::{prune, PruneConfig};
/// use serde_json::json;
///
/// let mut messages = vec![
///     json!({"role": "user", "content": "fix the bug"}),
///     json!({"role": "assistant", "content": "",
///            "tool_calls": [{"function": {"name": "read_file",
///                                         "arguments": {"path": "src/lib.rs"}}}]}),
///     json!({"role": "tool", "content": "x".repeat(500)}),
/// ];
/// // Pad so the tool result falls outside the protected tail.
/// for i in 0..10 {
///     messages.push(json!({"role": "user", "content": format!("turn {i}")}));
/// }
///
/// let outcome = prune(&messages, &PruneConfig::default());
/// assert_eq!(
///     outcome.messages[2]["content"].as_str().unwrap(),
///     "[read_file] read 'src/lib.rs' -> ok, 1 lines (500 chars)",
/// );
/// assert!(outcome.chars_reclaimed > 400);
/// ```
pub fn prune(messages: &[Value], cfg: &PruneConfig) -> PruneOutcome {
    let before = serialized_len(messages);
    let out = collapse_duplicate_tool_results(messages, cfg);
    let out = summarize_aged_tool_results(&out, cfg);
    let out = shrink_tool_call_args(&out, cfg);
    let after = serialized_len(&out);
    PruneOutcome {
        messages: out,
        chars_reclaimed: before.saturating_sub(after),
    }
}

/// Summed `serde_json::to_string` length of each message — the currency of
/// [`PruneOutcome::chars_reclaimed`].
pub fn serialized_len(messages: &[Value]) -> usize {
    messages.iter().map(json_len).sum()
}

/// Pass 1: collapse duplicate tool results.
///
/// Identical `role:"tool"` contents longer than
/// [`PruneConfig::dedupe_min_chars`] are hashed (blake3); aged later
/// occurrences are replaced with a one-liner referencing the first
/// occurrence's message index. The first occurrence and anything in the
/// protected tail are left untouched.
pub fn collapse_duplicate_tool_results(messages: &[Value], cfg: &PruneConfig) -> Vec<Value> {
    let mut out = messages.to_vec();
    let aged = cfg.aged_len(out.len());
    let paired = pair_tool_results(&out);
    let mut first_seen: HashMap<[u8; 32], usize> = HashMap::new();
    let mut replacements: Vec<(usize, String)> = Vec::new();

    for (i, msg) in out.iter().enumerate() {
        if msg["role"].as_str() != Some("tool") {
            continue;
        }
        let Some(content) = msg["content"].as_str() else {
            continue;
        };
        if content.chars().count() <= cfg.dedupe_min_chars
            || already_pruned(content, paired_name(&paired, i))
        {
            continue;
        }
        let key = *blake3::hash(content.as_bytes()).as_bytes();
        match first_seen.get(&key) {
            None => {
                first_seen.insert(key, i);
            }
            // Hash hit: confirm true equality before collapsing (paranoia
            // against collisions — the first occurrence is never rewritten,
            // so comparing against `out[first]` is comparing originals).
            Some(&first) if i < aged && out[first]["content"].as_str() == Some(content) => {
                let n = content.chars().count();
                let line = format!(
                    "[duplicate of message {first}: identical {n}-char tool result elided]"
                );
                if json_str_len(&line) < json_str_len(content) {
                    replacements.push((i, line));
                }
            }
            Some(_) => {}
        }
    }
    for (i, line) in replacements {
        out[i]["content"] = Value::String(line);
    }
    out
}

/// Pass 2: replace aged oversized tool results with per-tool one-liners.
///
/// Each `role:"tool"` message outside the protected tail whose content is
/// longer than [`PruneConfig::summarize_min_chars`] is paired with the
/// `tool_calls` entry that produced it (by `tool_call_id` when present,
/// positionally otherwise — matching both wire dialects the TUI loop uses)
/// and rewritten as e.g. `[run_command] ran 'npm test' -> ok, 47 lines
/// output`. Unpairable (orphaned) results fall back to a generic `[tool]`
/// one-liner.
pub fn summarize_aged_tool_results(messages: &[Value], cfg: &PruneConfig) -> Vec<Value> {
    let mut out = messages.to_vec();
    let aged = cfg.aged_len(out.len());
    let paired = pair_tool_results(&out);

    for (i, msg) in out.iter_mut().enumerate().take(aged) {
        if msg["role"].as_str() != Some("tool") {
            continue;
        }
        let line = {
            let Some(content) = msg["content"].as_str() else {
                continue;
            };
            if content.chars().count() <= cfg.summarize_min_chars
                || already_pruned(content, paired_name(&paired, i))
            {
                continue;
            }
            let (name, args) = match &paired[i] {
                Some(p) => (p.name.as_str(), Some(&p.args)),
                None => ("tool", None),
            };
            let line = one_line_summary(name, args, content);
            if json_str_len(&line) >= json_str_len(content) {
                continue;
            }
            line
        };
        msg["content"] = Value::String(line);
    }
    out
}

/// Pass 3: JSON-aware shrinking of oversized tool-call arguments.
///
/// For aged assistant messages, any `function.arguments` whose serialized
/// form exceeds [`PruneConfig::args_max_chars`] is parsed, long string
/// values are truncated *inside* the structure (recursively, to
/// [`PruneConfig::arg_string_cap`] chars), and the result is reserialized —
/// so the output always parses, and string-typed arguments stay strings
/// while object-typed arguments stay objects. An oversized arguments string
/// that does not parse is replaced with a small valid-JSON placeholder.
pub fn shrink_tool_call_args(messages: &[Value], cfg: &PruneConfig) -> Vec<Value> {
    let mut out = messages.to_vec();
    let aged = cfg.aged_len(out.len());
    let cap = cfg.effective_arg_string_cap();

    for msg in out.iter_mut().take(aged) {
        if msg["role"].as_str() != Some("assistant") {
            continue;
        }
        let Some(tcs) = msg.get_mut("tool_calls").and_then(Value::as_array_mut) else {
            continue;
        };
        for tc in tcs {
            let Some(arguments) = tc.get_mut("function").and_then(|f| f.get_mut("arguments"))
            else {
                continue;
            };
            shrink_arguments(arguments, cfg.args_max_chars, cap);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// A tool call paired to its result message: the tool name plus its parsed
/// arguments (string-encoded arguments are parsed; unparseable ones → Null).
struct PairedCall {
    name: String,
    args: Value,
}

/// For each message index, the tool call that produced it (None for
/// non-results and orphans). Results match their assistant's `tool_calls`
/// by `tool_call_id` when present (OpenAI dialect) or positionally (Ollama
/// dialect, which emits `role:"tool"` results without ids).
fn pair_tool_results(messages: &[Value]) -> Vec<Option<PairedCall>> {
    let mut paired = Vec::with_capacity(messages.len());
    let mut pending: Vec<(String, PairedCall)> = Vec::new();

    for msg in messages {
        let role = msg["role"].as_str().unwrap_or("");
        if role == "tool" {
            let id = msg["tool_call_id"].as_str().unwrap_or("");
            let pos = if id.is_empty() {
                (!pending.is_empty()).then_some(0)
            } else {
                pending.iter().position(|(pid, _)| pid == id)
            };
            paired.push(pos.map(|p| pending.remove(p).1));
            continue;
        }
        pending.clear();
        if role == "assistant" {
            if let Some(tcs) = msg["tool_calls"].as_array() {
                for tc in tcs {
                    let id = tc["id"].as_str().unwrap_or("").to_string();
                    let name = tc["function"]["name"]
                        .as_str()
                        .unwrap_or("tool")
                        .to_string();
                    let args = match &tc["function"]["arguments"] {
                        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                        v => v.clone(),
                    };
                    pending.push((id, PairedCall { name, args }));
                }
            }
        }
        paired.push(None);
    }
    paired
}

fn paired_name(paired: &[Option<PairedCall>], i: usize) -> &str {
    paired[i].as_ref().map_or("tool", |p| p.name.as_str())
}

/// True when `content` is already a pass-1 or pass-2 marker — keeps both
/// passes idempotent even under tiny thresholds.
fn already_pruned(content: &str, paired_name: &str) -> bool {
    content.starts_with("[duplicate of message ")
        || content.starts_with(&format!("[{paired_name}] "))
}

/// Per-tool one-line summary of a tool result (the pass-2 rewrite).
fn one_line_summary(name: &str, args: Option<&Value>, content: &str) -> String {
    let lines = content.lines().count();
    let chars = content.chars().count();
    let status = if looks_like_error(content) {
        "error"
    } else {
        "ok"
    };
    let arg = |key: &str| -> String {
        args.and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .map_or_else(|| "?".to_string(), |s| excerpt(s, 80))
    };
    match name {
        // Note: newt's `(exit N)` empty-output results are always shorter
        // than any one-liner, so the strictly-shorter rule keeps them as-is —
        // no exit-code special case is reachable here.
        "run_command" => {
            format!(
                "[run_command] ran '{}' -> {status}, {lines} lines output",
                arg("command")
            )
        }
        "read_file" => {
            format!(
                "[read_file] read '{}' -> {status}, {lines} lines ({chars} chars)",
                arg("path")
            )
        }
        "write_file" => {
            format!(
                "[write_file] wrote '{}' -> {status}, {lines} lines result",
                arg("path")
            )
        }
        "edit_file" => {
            format!(
                "[edit_file] edited '{}' -> {status}, {lines} lines result",
                arg("path")
            )
        }
        "list_dir" => format!(
            "[list_dir] listed '{}' -> {status}, {lines} entries",
            arg("path")
        ),
        "search" => {
            let q = args
                .and_then(|a| a.get("query").or_else(|| a.get("pattern")))
                .and_then(Value::as_str)
                .map_or_else(|| "?".to_string(), |s| excerpt(s, 80));
            format!("[search] searched '{q}' -> {status}, {lines} matching lines")
        }
        "web_fetch" => {
            format!(
                "[web_fetch] fetched '{}' -> {status}, {lines} lines ({chars} chars)",
                arg("url")
            )
        }
        _ => format!("[{name}] result elided -> {status}, {lines} lines ({chars} chars)"),
    }
}

/// First `max_chars` chars with newlines flattened, `…`-terminated if cut.
fn excerpt(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= max_chars {
        cleaned
    } else {
        let head: String = cleaned.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// Matches the error shapes `execute_tool` produces (`error…`,
/// `capability denied…`).
fn looks_like_error(content: &str) -> bool {
    let t = content.trim_start();
    t.starts_with("error") || t.starts_with("Error") || t.starts_with("capability denied")
}

/// Shrink one `function.arguments` value in place (pass-3 core). String-typed
/// arguments are parsed/shrunk/reserialized and stay strings; structured
/// arguments are shrunk directly. A rewrite is only applied when strictly
/// shorter serialized.
fn shrink_arguments(arguments: &mut Value, max_chars: usize, cap: usize) {
    match arguments {
        Value::String(s) => {
            if s.chars().count() <= max_chars {
                return;
            }
            let shrunk = match serde_json::from_str::<Value>(s) {
                Ok(mut v) => {
                    shrink_value(&mut v, cap);
                    v
                }
                Err(_) => {
                    // Not JSON to begin with — substitute a small valid-JSON
                    // placeholder rather than slicing bytes (hermes #11762).
                    let n = s.chars().count();
                    let mut ph = serde_json::json!({
                        "truncated": format!("original arguments were not valid JSON ({n} chars elided)"),
                    });
                    shrink_value(&mut ph, cap);
                    ph
                }
            };
            let new = serde_json::to_string(&shrunk).unwrap_or_else(|_| "{}".to_string());
            if json_str_len(&new) < json_str_len(s) {
                *arguments = Value::String(new);
            }
        }
        other => {
            if json_len(other) <= max_chars {
                return;
            }
            let mut v = other.clone();
            if shrink_value(&mut v, cap) && json_len(&v) < json_len(other) {
                *other = v;
            }
        }
    }
}

/// Recursively truncate long string values inside a parsed JSON structure.
/// Returns true when anything changed.
fn shrink_value(v: &mut Value, cap: usize) -> bool {
    match v {
        Value::String(s) => match truncate_chars(s, cap) {
            Some(t) => {
                *s = t;
                true
            }
            None => false,
        },
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, it| shrink_value(it, cap) || acc),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |acc, it| shrink_value(it, cap) || acc),
        _ => false,
    }
}

/// Char-boundary-safe truncation to at most `cap` chars including the
/// `… [+N chars]` marker (None when already within `cap`). Reserves marker
/// space using the *total* count, whose digit count bounds the omitted
/// count's — so the result never exceeds `cap`, which keeps repeated
/// applications stable.
fn truncate_chars(s: &str, cap: usize) -> Option<String> {
    let total = s.chars().count();
    if total <= cap {
        return None;
    }
    let marker_reserve = format!("… [+{total} chars]").chars().count();
    let keep = cap.saturating_sub(marker_reserve);
    let head: String = s.chars().take(keep).collect();
    let omitted = total - keep;
    Some(format!("{head}… [+{omitted} chars]"))
}

/// Serialized length of one JSON value.
fn json_len(v: &Value) -> usize {
    serde_json::to_string(v).map_or(0, |s| s.len())
}

/// Serialized length of a string as a JSON string (quotes + escapes) — the
/// exact cost of a `content` value inside its message.
fn json_str_len(s: &str) -> usize {
    json_len(&Value::String(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- builders ----------------------------------------------------------

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn assistant_text(text: &str) -> Value {
        json!({"role": "assistant", "content": text})
    }

    /// Ollama dialect: no ids, `arguments` is a JSON object.
    fn assistant_calls(calls: &[(&str, Value)]) -> Value {
        let tcs: Vec<Value> = calls
            .iter()
            .map(|(name, args)| json!({"function": {"name": name, "arguments": args}}))
            .collect();
        json!({"role": "assistant", "content": "", "tool_calls": tcs})
    }

    /// OpenAI dialect: ids, `arguments` is a serialized JSON string.
    fn assistant_calls_openai(calls: &[(&str, &str, Value)]) -> Value {
        let tcs: Vec<Value> = calls
            .iter()
            .map(|(id, name, args)| {
                json!({"id": id, "type": "function",
                       "function": {"name": name, "arguments": args.to_string()}})
            })
            .collect();
        json!({"role": "assistant", "content": "", "tool_calls": tcs})
    }

    fn tool_result(content: &str) -> Value {
        json!({"role": "tool", "content": content})
    }

    fn tool_result_id(id: &str, content: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "content": content})
    }

    /// `n` distinct lines, ~13 chars each.
    fn text_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("test line {i:03}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pad_tail(msgs: &mut Vec<Value>, n: usize) {
        for i in 0..n {
            msgs.push(user(&format!("tail filler {i}")));
        }
    }

    fn content_of(msg: &Value) -> &str {
        msg["content"].as_str().unwrap()
    }

    // -- pass 1: duplicate collapse -----------------------------------------

    #[test]
    fn dedupe_collapses_later_identical_aged_result() {
        let big = text_lines(40); // ~560 chars
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("run_command", json!({"command": "cargo test"}))]),
            tool_result(&big),
            assistant_calls(&[("run_command", json!({"command": "cargo test"}))]),
            tool_result(&big),
        ];
        pad_tail(&mut msgs, 10);
        let out = collapse_duplicate_tool_results(&msgs, &PruneConfig::default());
        // First occurrence untouched; later one collapsed with a reference.
        assert_eq!(content_of(&out[2]), big);
        assert_eq!(
            content_of(&out[4]),
            format!(
                "[duplicate of message 2: identical {}-char tool result elided]",
                big.chars().count()
            ),
        );
    }

    #[test]
    fn dedupe_ignores_short_results_and_non_tool_roles() {
        let small = "short duplicate";
        let big = text_lines(40);
        let mut msgs = vec![
            user(&big), // non-tool role with big duplicate content
            tool_result(small),
            tool_result(small),
            user(&big),
        ];
        pad_tail(&mut msgs, 4);
        let cfg = PruneConfig {
            keep_last: 4,
            ..PruneConfig::default()
        };
        let out = collapse_duplicate_tool_results(&msgs, &cfg);
        assert_eq!(out, msgs);
    }

    #[test]
    fn dedupe_never_touches_protected_tail() {
        let big = text_lines(40);
        let mut msgs = vec![user("task"), tool_result(&big)];
        pad_tail(&mut msgs, 3);
        msgs.push(tool_result(&big)); // duplicate, but inside the tail
        let cfg = PruneConfig {
            keep_last: 2,
            ..PruneConfig::default()
        };
        let out = collapse_duplicate_tool_results(&msgs, &cfg);
        assert_eq!(content_of(out.last().unwrap()), big);
    }

    #[test]
    fn dedupe_leaves_distinct_results_alone() {
        let mut msgs = vec![
            user("task"),
            tool_result(&text_lines(40)),
            tool_result(&text_lines(41)),
        ];
        pad_tail(&mut msgs, 10);
        let out = collapse_duplicate_tool_results(&msgs, &PruneConfig::default());
        assert_eq!(out, msgs);
    }

    // -- pass 2: per-tool one-liners ----------------------------------------

    /// Build `[user, assistant_call, tool_result, …tail]`, run pass 2, and
    /// return the rewritten result content.
    fn summarize_one(name: &str, args: Value, content: &str) -> String {
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[(name, args)]),
            tool_result(content),
        ];
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &PruneConfig::default());
        content_of(&out[2]).to_string()
    }

    #[test]
    fn one_liner_run_command() {
        let line = summarize_one(
            "run_command",
            json!({"command": "npm test"}),
            &text_lines(47),
        );
        assert_eq!(line, "[run_command] ran 'npm test' -> ok, 47 lines output");
    }

    #[test]
    fn one_liner_run_command_error() {
        let content = format!("error: build failed\n{}", text_lines(30));
        let line = summarize_one("run_command", json!({"command": "cargo build"}), &content);
        assert_eq!(
            line,
            "[run_command] ran 'cargo build' -> error, 31 lines output"
        );
    }

    #[test]
    fn one_liner_never_replaces_with_something_longer() {
        // `(exit 0)` (run_command's empty-output result) is shorter than any
        // one-liner — the strictly-shorter rule must leave it alone even when
        // the threshold would otherwise fire.
        let cfg = PruneConfig {
            summarize_min_chars: 4,
            ..PruneConfig::default()
        };
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("run_command", json!({"command": "true"}))]),
            tool_result("(exit 0)"),
        ];
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &cfg);
        assert_eq!(content_of(&out[2]), "(exit 0)");
    }

    #[test]
    fn one_liner_read_file() {
        let content = text_lines(120);
        let chars = content.chars().count();
        let line = summarize_one("read_file", json!({"path": "src/main.rs"}), &content);
        assert_eq!(
            line,
            format!("[read_file] read 'src/main.rs' -> ok, 120 lines ({chars} chars)")
        );
    }

    #[test]
    fn one_liner_write_edit_list_search_fetch_and_generic() {
        let content = text_lines(20);
        let chars = content.chars().count();
        assert_eq!(
            summarize_one(
                "write_file",
                json!({"path": "a.rs", "content": "xx"}),
                &content
            ),
            "[write_file] wrote 'a.rs' -> ok, 20 lines result",
        );
        assert_eq!(
            summarize_one("edit_file", json!({"path": "b.rs"}), &content),
            "[edit_file] edited 'b.rs' -> ok, 20 lines result",
        );
        assert_eq!(
            summarize_one("list_dir", json!({"path": "src"}), &content),
            "[list_dir] listed 'src' -> ok, 20 entries",
        );
        assert_eq!(
            summarize_one("search", json!({"query": "fn main"}), &content),
            "[search] searched 'fn main' -> ok, 20 matching lines",
        );
        assert_eq!(
            summarize_one("search", json!({"pattern": "TODO"}), &content),
            "[search] searched 'TODO' -> ok, 20 matching lines",
        );
        assert_eq!(
            summarize_one(
                "web_fetch",
                json!({"url": "https://example.com/doc"}),
                &content
            ),
            format!(
                "[web_fetch] fetched 'https://example.com/doc' -> ok, 20 lines ({chars} chars)"
            ),
        );
        assert_eq!(
            summarize_one("gitea__issue_view", json!({"index": 1}), &content),
            format!("[gitea__issue_view] result elided -> ok, 20 lines ({chars} chars)"),
        );
    }

    #[test]
    fn one_liner_missing_args_uses_placeholder() {
        let line = summarize_one("read_file", json!(null), &text_lines(20));
        assert!(
            line.starts_with("[read_file] read '?' -> ok, 20 lines"),
            "{line}"
        );
    }

    #[test]
    fn one_liner_long_command_excerpted_and_newlines_flattened() {
        let cmd = format!("echo {}\nsecond", "x".repeat(100));
        let line = summarize_one("run_command", json!({"command": cmd}), &text_lines(20));
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains('…'), "{line}");
        assert!(
            line.chars().count() < 200,
            "one-liners stay under the default threshold"
        );
    }

    #[test]
    fn openai_results_pair_by_id_even_out_of_order() {
        let read = text_lines(50);
        let listing = text_lines(30);
        let mut msgs = vec![
            user("task"),
            assistant_calls_openai(&[
                ("call_a", "read_file", json!({"path": "x.rs"})),
                ("call_b", "list_dir", json!({"path": "src"})),
            ]),
            // results arrive swapped
            tool_result_id("call_b", &listing),
            tool_result_id("call_a", &read),
        ];
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &PruneConfig::default());
        assert!(
            content_of(&out[2]).starts_with("[list_dir] listed 'src'"),
            "{}",
            content_of(&out[2])
        );
        assert!(
            content_of(&out[3]).starts_with("[read_file] read 'x.rs'"),
            "{}",
            content_of(&out[3])
        );
    }

    #[test]
    fn ollama_results_pair_positionally() {
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[
                ("read_file", json!({"path": "x.rs"})),
                ("list_dir", json!({"path": "src"})),
            ]),
            tool_result(&text_lines(50)),
            tool_result(&text_lines(30)),
        ];
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &PruneConfig::default());
        assert!(content_of(&out[2]).starts_with("[read_file] read 'x.rs'"));
        assert!(content_of(&out[3]).starts_with("[list_dir] listed 'src'"));
    }

    #[test]
    fn orphan_tool_result_gets_generic_one_liner() {
        let content = text_lines(25);
        let chars = content.chars().count();
        let mut msgs = vec![user("task"), tool_result(&content)]; // no assistant before it
        pad_tail(&mut msgs, 10);
        let out = summarize_aged_tool_results(&msgs, &PruneConfig::default());
        assert_eq!(
            content_of(&out[1]),
            format!("[tool] result elided -> ok, 25 lines ({chars} chars)"),
        );
    }

    #[test]
    fn summarize_protects_tail_and_skips_existing_markers() {
        let big = text_lines(40);
        let marker = "[read_file] read 'x' -> ok, 3 lines (5 chars)";
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("read_file", json!({"path": "x"}))]),
            json!({"role": "tool", "content": marker}),
        ];
        pad_tail(&mut msgs, 2);
        msgs.push(tool_result(&big)); // inside tail
        let cfg = PruneConfig {
            keep_last: 1,
            summarize_min_chars: 10,
            ..PruneConfig::default()
        };
        let out = summarize_aged_tool_results(&msgs, &cfg);
        assert_eq!(content_of(&out[2]), marker, "existing marker not rewritten");
        assert_eq!(content_of(out.last().unwrap()), big, "tail untouched");
    }

    // -- pass 3: JSON-aware arg shrinking ------------------------------------

    #[test]
    fn shrink_truncates_inside_object_args() {
        let body = "fn main() {}\n".repeat(200); // 2600 chars
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[(
                "write_file",
                json!({"path": "src/main.rs", "content": body}),
            )]),
            tool_result("ok"),
        ];
        pad_tail(&mut msgs, 10);
        let out = shrink_tool_call_args(&msgs, &PruneConfig::default());
        let args = &out[1]["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_object(), "object args stay objects");
        assert_eq!(args["path"], "src/main.rs", "short values untouched");
        let content = args["content"].as_str().unwrap();
        assert!(content.chars().count() <= 200, "truncated to the cap");
        assert!(content.contains("… [+"), "{content}");
        assert!(content.ends_with("chars]"), "{content}");
    }

    #[test]
    fn shrink_keeps_string_args_as_valid_json_strings() {
        let args =
            json!({"path": "a.rs", "old_string": "x".repeat(900), "new_string": "y".repeat(900)});
        let mut msgs = vec![
            user("task"),
            assistant_calls_openai(&[("call_1", "edit_file", args)]),
            tool_result_id("call_1", "ok"),
        ];
        pad_tail(&mut msgs, 10);
        let before_len = msgs[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .len();
        let out = shrink_tool_call_args(&msgs, &PruneConfig::default());
        let s = out[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("still a string");
        let parsed: Value = serde_json::from_str(s).expect("still valid JSON");
        assert_eq!(parsed["path"], "a.rs");
        assert!(parsed["old_string"].as_str().unwrap().chars().count() <= 200);
        assert!(s.len() < before_len);
    }

    #[test]
    fn shrink_recurses_into_arrays_and_nested_objects() {
        let args = json!({"items": ["z".repeat(700), {"inner": "w".repeat(700)}], "n": 7});
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("custom", args)]),
            tool_result("ok"),
        ];
        pad_tail(&mut msgs, 10);
        let out = shrink_tool_call_args(&msgs, &PruneConfig::default());
        let a = &out[1]["tool_calls"][0]["function"]["arguments"];
        assert!(a["items"][0].as_str().unwrap().chars().count() <= 200);
        assert!(a["items"][1]["inner"].as_str().unwrap().chars().count() <= 200);
        assert_eq!(a["n"], 7);
    }

    #[test]
    fn shrink_replaces_unparseable_oversized_string_args() {
        let bad = format!("{{not json {}", "x".repeat(800));
        let mut msgs = vec![
            user("task"),
            json!({"role": "assistant", "content": "",
                   "tool_calls": [{"id": "c1", "type": "function",
                                   "function": {"name": "custom", "arguments": bad}}]}),
            tool_result_id("c1", "ok"),
        ];
        pad_tail(&mut msgs, 10);
        let out = shrink_tool_call_args(&msgs, &PruneConfig::default());
        let s = out[1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap();
        let parsed: Value = serde_json::from_str(s).expect("placeholder is valid JSON");
        assert!(parsed["truncated"]
            .as_str()
            .unwrap()
            .contains("not valid JSON"));
    }

    #[test]
    fn shrink_leaves_small_args_and_tail_untouched() {
        let small = json!({"path": "a.rs"});
        let big = json!({"content": "q".repeat(2000)});
        let mut msgs = vec![user("task"), assistant_calls(&[("read_file", small)])];
        pad_tail(&mut msgs, 3);
        msgs.push(assistant_calls(&[("write_file", big)])); // inside tail
        let cfg = PruneConfig {
            keep_last: 2,
            ..PruneConfig::default()
        };
        let out = shrink_tool_call_args(&msgs, &cfg);
        assert_eq!(out, msgs);
    }

    #[test]
    fn shrink_cap_floor_prevents_marker_oscillation() {
        let cfg = PruneConfig {
            arg_string_cap: 0,
            ..PruneConfig::default()
        };
        let mut msgs = vec![
            user("task"),
            assistant_calls(&[("write_file", json!({"content": "r".repeat(3000)}))]),
            tool_result("ok"),
        ];
        pad_tail(&mut msgs, 10);
        let once = shrink_tool_call_args(&msgs, &cfg);
        let s = once[1]["tool_calls"][0]["function"]["arguments"]["content"]
            .as_str()
            .unwrap();
        assert!(
            s.chars().count() <= MIN_ARG_STRING_CAP,
            "floored cap respected: {s}"
        );
        let twice = shrink_tool_call_args(&once, &cfg);
        assert_eq!(twice, once);
    }

    #[test]
    fn truncate_chars_is_exact_and_stable() {
        assert_eq!(truncate_chars("short", 32), None);
        let t = truncate_chars(&"é".repeat(100), 32).unwrap(); // multibyte-safe
        assert!(t.chars().count() <= 32);
        assert_eq!(
            truncate_chars(&t, 32),
            None,
            "second application is a no-op"
        );
    }

    // -- top-level prune ------------------------------------------------------

    #[test]
    fn prune_empty_and_all_tail_are_noops() {
        let cfg = PruneConfig::default();
        let out = prune(&[], &cfg);
        assert!(out.messages.is_empty());
        assert_eq!(out.chars_reclaimed, 0);

        let msgs = vec![user("task"), tool_result(&text_lines(100))];
        let out = prune(&msgs, &cfg); // len <= keep_last → fully protected
        assert_eq!(out.messages, msgs);
        assert_eq!(out.chars_reclaimed, 0);
    }

    /// The realistic synthetic transcript: a coding session with a repeated
    /// failing `cargo test`, large file reads, and bulky write/edit args.
    /// Asserts every pass contributes, and prints the per-pass numbers.
    #[test]
    fn realistic_transcript_pass_by_pass_breakdown() {
        let cargo_fail = format!("error: test failed\n{}", text_lines(300));
        let lib_rs = text_lines(600);
        let cargo_pass = text_lines(60);
        let new_body = "fn lossy_op() { /* generated */ }\n".repeat(120);
        let msgs = vec![
            json!({"role": "system", "content": "You are newt, a coding agent."}),
            user("fix the failing test in newt-core"),
            assistant_calls(&[("read_file", json!({"path": "newt-core/src/lib.rs"}))]),
            tool_result(&lib_rs),
            assistant_calls(&[("run_command", json!({"command": "cargo test -p newt-core"}))]),
            tool_result(&cargo_fail),
            assistant_calls(&[(
                "edit_file",
                json!({
                    "path": "newt-core/src/lib.rs",
                    "old_string": text_lines(60),
                    "new_string": text_lines(62),
                }),
            )]),
            tool_result("edited newt-core/src/lib.rs (+2 lines)"),
            assistant_calls(&[("run_command", json!({"command": "cargo test -p newt-core"}))]),
            tool_result(&cargo_fail), // identical failure → dedupe fodder
            assistant_calls(&[(
                "write_file",
                json!({"path": "newt-core/src/fix.rs", "content": new_body}),
            )]),
            tool_result("wrote newt-core/src/fix.rs"),
            assistant_calls(&[("run_command", json!({"command": "cargo test -p newt-core"}))]),
            tool_result(&cargo_pass),
            assistant_text("All tests pass now. The bug was an off-by-one."),
            user("great — open the PR"),
        ];

        let cfg = PruneConfig {
            keep_last: 4,
            ..PruneConfig::default()
        };
        let s0 = serialized_len(&msgs);
        let p1 = collapse_duplicate_tool_results(&msgs, &cfg);
        let s1 = serialized_len(&p1);
        let p2 = summarize_aged_tool_results(&p1, &cfg);
        let s2 = serialized_len(&p2);
        let p3 = shrink_tool_call_args(&p2, &cfg);
        let s3 = serialized_len(&p3);
        println!("baseline: {s0} chars");
        println!("pass 1 (dedupe):      -{} chars -> {s1}", s0 - s1);
        println!("pass 2 (one-liners):  -{} chars -> {s2}", s1 - s2);
        println!("pass 3 (arg shrink):  -{} chars -> {s3}", s2 - s3);
        assert!(s1 < s0, "dedupe reclaims");
        assert!(s2 < s1, "one-liners reclaim");
        assert!(s3 < s2, "arg shrink reclaims");

        let outcome = prune(&msgs, &cfg);
        assert_eq!(outcome.messages, p3, "prune == the three passes in order");
        assert_eq!(
            outcome.chars_reclaimed,
            s0 - s3,
            "accounting matches the pass chain"
        );
        println!(
            "total: {} of {s0} chars reclaimed ({:.0}%)",
            outcome.chars_reclaimed,
            100.0 * outcome.chars_reclaimed as f64 / s0 as f64
        );
        // The last user message + final answer are inside keep_last=4.
        assert_eq!(outcome.messages[15], msgs[15]);
        assert_eq!(outcome.messages[14], msgs[14]);
    }

    // -- property tests (deterministic seeded loops) --------------------------

    /// xorshift64* — deterministic, no dev-dep.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Deterministic synthetic transcript: mixed dialects, duplicate-prone
    /// result payloads, oversized and small args, interleaved chatter.
    fn synth_transcript(seed: u64) -> Vec<Value> {
        let mut rng = Rng(seed.max(1));
        let tools = [
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "list_dir",
            "search",
            "web_fetch",
            "use_skill",
        ];
        // Small payload pool → duplicates across rounds are likely.
        let pool: Vec<String> = (0..5).map(|k| text_lines(10 + k * 37)).collect();
        let mut msgs = vec![
            json!({"role": "system", "content": "You are newt."}),
            user("do the task"),
        ];
        for round in 0..(4 + rng.below(5)) {
            if rng.below(4) == 0 {
                msgs.push(assistant_text("thinking out loud"));
                msgs.push(user(&format!("continue ({round})")));
            }
            let openai = rng.below(2) == 0;
            let ncalls = 1 + rng.below(3);
            let calls: Vec<(String, String, Value)> = (0..ncalls)
                .map(|j| {
                    let name = tools[rng.below(tools.len())];
                    let key = match name {
                        "run_command" => "command",
                        "search" => "query",
                        "web_fetch" => "url",
                        _ => "path",
                    };
                    let mut args = json!({key: format!("target-{}-{}", round, j)});
                    if rng.below(3) == 0 {
                        args["content"] = Value::String("b".repeat(50 + rng.below(2000)));
                    }
                    (format!("call_{round}_{j}"), name.to_string(), args)
                })
                .collect();
            if openai {
                let refs: Vec<(&str, &str, Value)> = calls
                    .iter()
                    .map(|(id, n, a)| (id.as_str(), n.as_str(), a.clone()))
                    .collect();
                msgs.push(assistant_calls_openai(&refs));
                // Sometimes deliver results out of order (ids still pair).
                let mut order: Vec<usize> = (0..ncalls).collect();
                if ncalls > 1 && rng.below(2) == 0 {
                    order.swap(0, 1);
                }
                for &j in &order {
                    let content = if rng.below(2) == 0 {
                        pool[rng.below(pool.len())].clone()
                    } else {
                        text_lines(1 + rng.below(80))
                    };
                    msgs.push(tool_result_id(&calls[j].0, &content));
                }
            } else {
                let refs: Vec<(&str, Value)> = calls
                    .iter()
                    .map(|(_, n, a)| (n.as_str(), a.clone()))
                    .collect();
                msgs.push(assistant_calls(&refs));
                for _ in 0..ncalls {
                    let content = if rng.below(2) == 0 {
                        pool[rng.below(pool.len())].clone()
                    } else {
                        text_lines(1 + rng.below(80))
                    };
                    msgs.push(tool_result(&content));
                }
            }
        }
        msgs.push(assistant_text("done"));
        msgs.push(user("thanks"));
        msgs
    }

    fn property_configs() -> Vec<PruneConfig> {
        vec![
            PruneConfig::default(),
            PruneConfig {
                keep_last: 4,
                ..PruneConfig::default()
            },
            PruneConfig {
                keep_last: 0,
                dedupe_min_chars: 50,
                summarize_min_chars: 50,
                args_max_chars: 100,
                arg_string_cap: 0,
            },
        ]
    }

    #[test]
    fn property_output_tool_args_always_parse_and_keep_their_shape() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let out = prune(&msgs, &cfg).messages;
                for (i, msg) in out.iter().enumerate() {
                    let Some(tcs) = msg["tool_calls"].as_array() else {
                        continue;
                    };
                    for (j, tc) in tcs.iter().enumerate() {
                        let before = &msgs[i]["tool_calls"][j]["function"]["arguments"];
                        let after = &tc["function"]["arguments"];
                        match after {
                            Value::String(s) => {
                                assert!(
                                    before.is_string(),
                                    "seed {seed}: string args stay strings"
                                );
                                serde_json::from_str::<Value>(s).unwrap_or_else(|e| {
                                    panic!("seed {seed} msg {i} call {j}: invalid JSON ({e}): {s}")
                                });
                            }
                            v => assert!(
                                !before.is_string() && (v.is_object() || v.is_null()),
                                "seed {seed}: structured args stay structured"
                            ),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn property_tool_pairing_structure_is_preserved() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let out = prune(&msgs, &cfg).messages;
                assert_eq!(
                    out.len(),
                    msgs.len(),
                    "seed {seed}: no messages added or removed"
                );
                for (a, b) in msgs.iter().zip(&out) {
                    assert_eq!(a["role"], b["role"], "seed {seed}: roles preserved");
                    assert_eq!(
                        a["tool_call_id"], b["tool_call_id"],
                        "seed {seed}: result ids preserved"
                    );
                    let (atc, btc) = (a["tool_calls"].as_array(), b["tool_calls"].as_array());
                    assert_eq!(
                        atc.map(Vec::len),
                        btc.map(Vec::len),
                        "seed {seed}: tool_call counts preserved"
                    );
                    if let (Some(atc), Some(btc)) = (atc, btc) {
                        for (x, y) in atc.iter().zip(btc) {
                            assert_eq!(x["id"], y["id"], "seed {seed}: call ids preserved");
                            assert_eq!(
                                x["function"]["name"], y["function"]["name"],
                                "seed {seed}: call names preserved"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn property_prune_is_idempotent() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let once = prune(&msgs, &cfg);
                let twice = prune(&once.messages, &cfg);
                assert_eq!(
                    twice.messages, once.messages,
                    "seed {seed}: prune(prune(x)) == prune(x)"
                );
                assert_eq!(
                    twice.chars_reclaimed, 0,
                    "seed {seed}: second pass reclaims nothing"
                );
            }
        }
    }

    #[test]
    fn property_protected_tail_is_byte_identical() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let out = prune(&msgs, &cfg).messages;
                let tail_start = msgs.len().saturating_sub(cfg.keep_last);
                for i in tail_start..msgs.len() {
                    assert_eq!(
                        serde_json::to_string(&msgs[i]).unwrap(),
                        serde_json::to_string(&out[i]).unwrap(),
                        "seed {seed}: tail message {i} must be byte-identical"
                    );
                }
            }
        }
    }

    #[test]
    fn property_chars_reclaimed_accounting_is_exact() {
        for seed in 1..=40 {
            let msgs = synth_transcript(seed);
            for cfg in property_configs() {
                let outcome = prune(&msgs, &cfg);
                let before = serialized_len(&msgs);
                let after = serialized_len(&outcome.messages);
                assert!(
                    after <= before,
                    "seed {seed}: prune never grows the transcript"
                );
                assert_eq!(
                    outcome.chars_reclaimed,
                    before - after,
                    "seed {seed}: exact accounting"
                );
            }
        }
    }
}
