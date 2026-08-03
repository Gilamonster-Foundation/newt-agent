//! Pure Value↔Value bridge between the OpenAI **Responses** `input` shape and
//! the chat-shaped message list [`super::compress::compress`] operates on.
//!
//! The compressor is the ONE owner of history compaction (roles
//! system/user/assistant/tool). The Responses loop speaks a different wire
//! (`instructions` + `input` items: `function_call` / `function_call_output` /
//! `reasoning`). Rather than fork a second compactor, these two pure converters
//! translate in and back out — no `Message` type, no I/O, fully unit-testable.
//!
//! The rebuilt Responses `input` deliberately carries ONLY `user` / `assistant`
//! items (the original `instructions` stays separate and unchanged): the
//! structured `function_call` / `function_call_output` / `reasoning` items are
//! not replayable after their surrounding history is summarized, so they render
//! to plain assistant / user text — the estimator still sees their weight and no
//! dangling call correlation reaches the provider.

use serde_json::{json, Value};

/// Responses `input` items → chat-shaped messages for
/// [`super::compress::compress`]. `instructions` is prepended as a `system` card
/// so the compressor's head protection (system card + user task) applies and its
/// weight is counted. `reasoning` items are dropped (opaque, not replayable
/// post-compaction); `function_call` / `function_call_output` render to
/// assistant / tool text so the estimator sees their size.
pub(super) fn responses_input_to_chat(instructions: Option<&str>, input: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(input.len() + 1);
    if let Some(ins) = instructions {
        out.push(json!({ "role": "system", "content": ins }));
    }
    for item in input {
        // Already chat-shaped (no `type`, carries a `role`): clone verbatim.
        if item.get("type").is_none() && item.get("role").is_some() {
            out.push(item.clone());
            continue;
        }
        match item.get("type").and_then(Value::as_str) {
            // Opaque reasoning is not replayable once its history is summarized.
            Some("reasoning") => {}
            Some("function_call") => {
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                let args = stringify(item.get("arguments"));
                out.push(json!({
                    "role": "assistant",
                    "content": format!("[tool call {name}] {args}"),
                }));
            }
            Some("function_call_output") => {
                out.push(json!({
                    "role": "tool",
                    "content": stringify(item.get("output")),
                }));
            }
            // Any other structured item: keep its text weight as assistant text.
            _ => out.push(json!({ "role": "assistant", "content": item.to_string() })),
        }
    }
    out
}

/// A `Value` field as a compact string: a JSON string is used verbatim, any
/// other value is serialized, and an absent field is empty.
fn stringify(field: Option<&Value>) -> String {
    match field {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

/// Compacted chat messages → VALID Responses `input` items. The array never
/// carries role `system` or `tool` (Responses `input` takes user/assistant; tool
/// results are `function_call_output` items, gone after compaction). The
/// compaction summary marker (`system`/`user`) and any protected tool result
/// (`tool`) become plain `user` notes; `user` / `assistant` pass through;
/// empty-content items are dropped. Instructions stay separate (the caller keeps
/// the original `instructions`, unchanged).
pub(super) fn chat_to_responses_input(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|m| {
            let content = m.get("content").and_then(Value::as_str).unwrap_or("");
            if content.is_empty() {
                return None;
            }
            // `assistant` stays; user + everything else (the `system` compaction
            // marker, a protected `tool` result) become plain `user` notes so the
            // rebuilt `input` never carries a non-input role.
            let role = match m.get("role").and_then(Value::as_str) {
                Some("assistant") => "assistant",
                _ => "user",
            };
            Some(json!({ "role": role, "content": content }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_become_a_single_system_head_or_none() {
        let input = vec![json!({"role": "user", "content": "hi"})];

        let chat = responses_input_to_chat(Some("sys rules"), &input);
        assert_eq!(chat.len(), 2);
        assert_eq!(chat[0]["role"], "system");
        assert_eq!(chat[0]["content"], "sys rules");
        assert_eq!(chat[1]["role"], "user");

        // Absent instructions → no system head is injected.
        let none = responses_input_to_chat(None, &input);
        assert_eq!(none.len(), 1);
        assert!(none.iter().all(|m| m["role"] != "system"));
    }

    #[test]
    fn function_items_render_to_assistant_and_tool_text() {
        let input = vec![
            json!({"role": "user", "content": "do it"}),
            // string arguments are used verbatim
            json!({"type": "function_call", "name": "read", "arguments": "{\"path\":\"a\"}", "call_id": "c1"}),
            json!({"type": "function_call_output", "call_id": "c1", "output": "file contents"}),
            // object arguments are stringified compactly
            json!({"type": "function_call", "name": "grep", "arguments": {"q": "x"}, "call_id": "c2"}),
        ];
        let chat = responses_input_to_chat(Some("ins"), &input);
        assert_eq!(chat.len(), 5);
        assert_eq!(chat[0]["role"], "system");
        assert_eq!(chat[1]["role"], "user");
        assert_eq!(chat[1]["content"], "do it");
        assert_eq!(chat[2]["role"], "assistant");
        assert_eq!(chat[2]["content"], "[tool call read] {\"path\":\"a\"}");
        assert_eq!(chat[3]["role"], "tool");
        assert_eq!(chat[3]["content"], "file contents");
        assert_eq!(chat[4]["role"], "assistant");
        assert_eq!(chat[4]["content"], "[tool call grep] {\"q\":\"x\"}");
    }

    #[test]
    fn reasoning_items_are_dropped() {
        let input = vec![
            json!({"type": "reasoning", "id": "rs_1", "summary": []}),
            json!({"role": "assistant", "content": "answer"}),
        ];
        let chat = responses_input_to_chat(None, &input);
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0]["role"], "assistant");
        assert_eq!(chat[0]["content"], "answer");
    }

    #[test]
    fn chat_to_responses_input_never_emits_system_or_tool() {
        let messages = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "working"}),
            json!({"role": "system", "content": "[CONTEXT COMPACTION — REFERENCE ONLY] summary"}),
            json!({"role": "tool", "content": "tool result"}),
            json!({"role": "user", "content": ""}), // empty → dropped
        ];
        let out = chat_to_responses_input(&messages);
        assert_eq!(out.len(), 4);
        assert!(out
            .iter()
            .all(|m| m["role"] != "system" && m["role"] != "tool"));
        assert_eq!(out[0]["role"], "user"); // user passthrough
        assert_eq!(out[1]["role"], "assistant"); // assistant passthrough
        assert_eq!(out[2]["role"], "user"); // system marker → user
        assert_eq!(
            out[2]["content"],
            "[CONTEXT COMPACTION — REFERENCE ONLY] summary"
        );
        assert_eq!(out[3]["role"], "user"); // tool → user
        assert_eq!(out[3]["content"], "tool result");
    }
}
