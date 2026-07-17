//! Episodic scratchpad — the `scratchpad` context feature (Step 26.4, #583).
//!
//! A structured `<state>` object (subtask status, open file paths, working
//! variables) kept SEPARATE from the conversation log: it is injected at the
//! HEAD of each turn and mutated by the model via `state_set` / `state_get` /
//! `state_clear` tool calls, rather than inferred from a chatty history. The
//! full snapshot is auto-injected every turn, so there is no `state_list` tool
//! (it would just duplicate context).
//!
//! Session-scoped, pure in-memory (no filesystem), discarded at `/new` / session
//! end. Mirrors `spill.rs`: a `&self` interior-mutability trait + an in-memory
//! impl, deterministic (a `BTreeMap` — sorted, no clock/uuid) for stable tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Per-value char cap when rendering the `<state>` block (longer values render
/// truncated with `[…]`; the full value stays retrievable via `state_get`).
pub(crate) const STATE_PER_VALUE_CAP: usize = 2_000;
/// Whole-block char cap so a runaway scratchpad can never blow the budget.
pub(crate) const STATE_TOTAL_CAP: usize = 8_000;

/// A session store for the scratchpad `<state>` (Step 26.4). `&self` methods
/// (interior mutability) so a single shared `&dyn ScratchpadStore` serves both
/// the per-turn injection and the tool-mutation path.
pub trait ScratchpadStore: Send + Sync {
    /// Set (or overwrite) one key.
    fn set(&self, key: &str, value: String);
    /// Read one key's full (un-truncated) value.
    fn get(&self, key: &str) -> Option<String>;
    /// A sorted snapshot of all entries (for the `<state>` block).
    fn entries(&self) -> BTreeMap<String, String>;
    /// Drop all entries (`/new`, or the model's `state_clear`).
    fn clear(&self);
    /// Number of keys held (for `/context stats`).
    fn keys_count(&self) -> u64;
    /// Total chars across all values (for `/context stats`).
    fn state_chars(&self) -> u64;
}

/// In-memory, session-scoped [`ScratchpadStore`] — pure (no fs), discarded at
/// session end / `/new`. A `BTreeMap` keeps the `<state>` block in deterministic
/// (sorted) order for stable tests.
#[derive(Default)]
pub struct SessionScratchpadStore {
    map: Mutex<BTreeMap<String, String>>,
}

impl ScratchpadStore for SessionScratchpadStore {
    fn set(&self, key: &str, value: String) {
        self.map.lock().unwrap().insert(key.to_string(), value);
    }
    fn get(&self, key: &str) -> Option<String> {
        self.map.lock().unwrap().get(key).cloned()
    }
    fn entries(&self) -> BTreeMap<String, String> {
        self.map.lock().unwrap().clone()
    }
    fn clear(&self) {
        self.map.lock().unwrap().clear();
    }
    fn keys_count(&self) -> u64 {
        self.map.lock().unwrap().len() as u64
    }
    fn state_chars(&self) -> u64 {
        self.map
            .lock()
            .unwrap()
            .values()
            .map(|v| v.chars().count() as u64)
            .sum()
    }
}

/// Render the `<state>…</state>` block injected at the head of a turn (Step
/// 26.4). `None` when the store is empty — the OFF/empty bit-for-bit guarantee
/// (an empty scratchpad changes nothing about the turn). Per-value and
/// whole-block caps are applied HERE (at injection), not at store time, so
/// `state_get` can still return a full value the block truncated.
pub(crate) fn build_state_block(
    store: &dyn ScratchpadStore,
    per_value_cap: usize,
    total_cap: usize,
) -> Option<String> {
    let entries = store.entries();
    if entries.is_empty() {
        return None;
    }
    let mut body = String::from("<state>\n");
    for (k, v) in &entries {
        let shown = if v.chars().count() > per_value_cap {
            let head: String = v.chars().take(per_value_cap).collect();
            format!("{head}[…]")
        } else {
            v.clone()
        };
        let line = format!("{k}: {shown}\n");
        // Leave room for the closing tag; stop cleanly if we'd overflow.
        if body.chars().count() + line.chars().count() + "</state>".len() > total_cap {
            body.push_str("[… state truncated to fit the budget …]\n");
            break;
        }
        body.push_str(&line);
    }
    body.push_str("</state>");
    Some(body)
}

/// Render the `<state>` block with the default budget caps (Step 26.4) — the
/// TUI-facing entry called per turn. `None` when the scratchpad is empty.
pub fn scratchpad_state_block(store: &dyn ScratchpadStore) -> Option<String> {
    build_state_block(store, STATE_PER_VALUE_CAP, STATE_TOTAL_CAP)
}

// ---------------------------------------------------------------------------
// Tool schemas (advertised only when the feature is on + a store is present)
// ---------------------------------------------------------------------------

/// `state_set` tool definition.
pub fn state_set_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "state_set",
            "description": "Record or update one piece of working state (a subtask's \
                            status, an open file path, a variable) that should persist \
                            across turns. The whole <state> block is shown to you at the \
                            head of every turn, so set what you need to remember and read \
                            it back there — don't restate it in prose.",
            "parameters": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "A short stable name, e.g. 'current_subtask' or 'open_file'." },
                    "value": { "type": "string", "description": "The value to store; overwrites any existing value for this key." }
                },
                "required": ["key", "value"]
            }
        }
    })
}

/// `state_get` tool definition.
pub fn state_get_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "state_get",
            "description": "Read back one key's FULL value (the <state> block at the head \
                            of the turn may truncate long values). Use this to confirm a \
                            single value without re-reading the whole block.",
            "parameters": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The key to read." }
                },
                "required": ["key"]
            }
        }
    })
}

/// `state_clear` tool definition.
pub fn state_clear_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "state_clear",
            "description": "Drop ALL scratchpad state — use when a task is done and its \
                            tracking variables are stale.",
            "parameters": { "type": "object", "properties": {}, "required": [] }
        }
    })
}

// ---------------------------------------------------------------------------
// Executors (every branch returns a tool-result String, never a loop abort)
// ---------------------------------------------------------------------------

/// Execute a `state_set` call (Step 26.4).
pub(crate) fn execute_state_set(
    args: &serde_json::Value,
    store: &dyn ScratchpadStore,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let key = args["key"].as_str().unwrap_or("").trim();
    let value = args["value"].as_str().unwrap_or("");
    if key.is_empty() {
        return "error: state_set requires a non-empty `key` (and a `value`)".to_string();
    }
    let existed = store.get(key).is_some();
    store.set(key, value.to_string());
    let out = if existed {
        format!("updated: {key}")
    } else {
        format!("stored: {key}")
    };
    out
}

/// Execute a `state_get` call (Step 26.4).
pub(crate) fn execute_state_get(
    args: &serde_json::Value,
    store: &dyn ScratchpadStore,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let key = args["key"].as_str().unwrap_or("").trim();
    if key.is_empty() {
        return "error: state_get requires a non-empty `key`".to_string();
    }
    let out = match store.get(key) {
        Some(v) => v,
        None => format!("no such key: {key}"),
    };
    out
}

/// Execute a `state_clear` call (Step 26.4).
pub(crate) fn execute_state_clear(
    store: &dyn ScratchpadStore,
    _color: bool,
    _tool_output_lines: usize,
) -> String {
    let n = store.keys_count();
    store.clear();
    let out = format!("cleared {n} entries");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_set_get_overwrite_clear_and_stats() {
        let s = SessionScratchpadStore::default();
        assert_eq!(s.keys_count(), 0);
        s.set("b", "two".to_string());
        s.set("a", "one".to_string());
        assert_eq!(s.get("a").as_deref(), Some("one"));
        assert_eq!(s.get("missing"), None);
        // entries are sorted (BTreeMap) → deterministic block order
        assert_eq!(
            s.entries().keys().cloned().collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string()]
        );
        // overwrite replaces + stats track value chars
        s.set("a", "ONE!".to_string());
        assert_eq!(s.get("a").as_deref(), Some("ONE!"));
        assert_eq!(s.keys_count(), 2);
        assert_eq!(s.state_chars(), 4 + 3); // "ONE!"(4) + "two"(3)
        s.clear();
        assert_eq!(s.keys_count(), 0);
        assert_eq!(s.state_chars(), 0);
    }

    #[test]
    fn build_state_block_none_when_empty_and_sorted_when_full() {
        let s = SessionScratchpadStore::default();
        assert_eq!(build_state_block(&s, 100, 1000), None, "empty → no block");
        s.set("zeta", "last".to_string());
        s.set("alpha", "first".to_string());
        let block = build_state_block(&s, 100, 1000).unwrap();
        assert!(block.starts_with("<state>\n") && block.ends_with("</state>"));
        // sorted: alpha before zeta
        let a = block.find("alpha").unwrap();
        let z = block.find("zeta").unwrap();
        assert!(a < z, "{block}");
        assert!(block.contains("alpha: first") && block.contains("zeta: last"));
    }

    #[test]
    fn build_state_block_truncates_per_value_and_total() {
        let s = SessionScratchpadStore::default();
        s.set("big", "x".repeat(50));
        let block = build_state_block(&s, 10, 1000).unwrap();
        assert!(block.contains("[…]"), "long value truncated: {block}");
        // the FULL value is still retrievable (cap is injection-only)
        assert_eq!(s.get("big").unwrap().chars().count(), 50);
        // total-cap: many keys stop the block with a marker
        let s2 = SessionScratchpadStore::default();
        for i in 0..50 {
            s2.set(&format!("k{i:02}"), "v".repeat(40));
        }
        let block2 = build_state_block(&s2, 2_000, 200).unwrap();
        assert!(
            block2.chars().count() <= 200 + 60,
            "total cap bounds the block"
        );
        assert!(block2.contains("state truncated"), "{block2:.120}");
    }

    #[test]
    fn executors_round_trip_and_coach() {
        let s = SessionScratchpadStore::default();
        // set (new → stored, repeat → updated)
        assert_eq!(
            execute_state_set(
                &serde_json::json!({"key": "k", "value": "v"}),
                &s,
                false,
                20
            ),
            "stored: k"
        );
        assert_eq!(
            execute_state_set(
                &serde_json::json!({"key": "k", "value": "v2"}),
                &s,
                false,
                20
            ),
            "updated: k"
        );
        // empty key → coaching, store untouched
        assert!(
            execute_state_set(&serde_json::json!({"value": "v"}), &s, false, 20)
                .starts_with("error:")
        );
        // get found / not-found / empty
        assert_eq!(
            execute_state_get(&serde_json::json!({"key": "k"}), &s, false, 20),
            "v2"
        );
        assert_eq!(
            execute_state_get(&serde_json::json!({"key": "nope"}), &s, false, 20),
            "no such key: nope"
        );
        assert!(execute_state_get(&serde_json::json!({}), &s, false, 20).starts_with("error:"));
        // clear reports the count and empties
        assert_eq!(execute_state_clear(&s, false, 20), "cleared 1 entries");
        assert_eq!(s.keys_count(), 0);
    }
}
