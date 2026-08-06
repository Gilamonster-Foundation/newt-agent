//! **Anthropic Messages API wire mapping** — pure functions only.
//!
//! The interactive loop keeps its transcript in the internal OpenAI-ish shape
//! (`role`/`content` strings, assistant `tool_calls[{id, function:{name,
//! arguments}}]`, `role:"tool"` + `tool_call_id`) so every shared subsystem —
//! compress, trim's orphan repair, nudges, the tool catalog — works unchanged.
//! This module converts that shape to Anthropic's `/v1/messages` wire **at
//! dispatch only** (mirroring `openai_chat_wire_messages`), and decodes both
//! the non-streaming reply and the SSE stream back into one [`AnthropicRound`].
//!
//! Everything here is pure and unit-tested without a server (the
//! `dgx_pull.rs` discipline); the HTTP dispatch and live display live with the
//! loop in `agentic/mod.rs`, and `newt-inference`'s simple transport reuses
//! [`build_messages_body`] / [`parse_messages_reply`] rather than duplicating
//! them (the `retry` re-export precedent).

/// Anthropic requires `max_tokens` on every request. Used when the generation
/// policy names no output cap; `NEWT_ANTHROPIC_MAX_TOKENS` overrides. 8192 is
/// safe across current Claude models (a model-specific 400 names the real
/// ceiling and is fatal-not-retried — the env var is the escape hatch).
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// [`DEFAULT_MAX_TOKENS`] with the `NEWT_ANTHROPIC_MAX_TOKENS` env override
/// applied.
pub fn default_max_tokens() -> u32 {
    std::env::var("NEWT_ANTHROPIC_MAX_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_MAX_TOKENS)
}

/// `POST` target for a configured endpoint base (`https://api.anthropic.com`
/// → `…/v1/messages`). Same base-URL convention as the OpenAI wire: the
/// endpoint carries scheme://host, paths are appended here.
pub fn messages_url(base: &str) -> String {
    format!("{}/v1/messages", base.trim_end_matches('/'))
}

/// Convert the internal message list to the Anthropic wire: the leading
/// system run coalesces into the top-level `system` string, assistant
/// `tool_calls` become `tool_use` content blocks (ids replayed verbatim),
/// each consecutive `role:"tool"` run becomes ONE user message of
/// `tool_result` blocks (Anthropic requires all parallel-call results in the
/// single next user message), and adjacent same-role messages merge (strict
/// proxies 400 on non-alternation; merging costs nothing).
///
/// A system message *after* conversation history is malformed and errors —
/// the loop never produces one (nudges are `role:"user"`), and silently
/// promoting it would rewrite history (same law as
/// `openai_chat_wire_messages`).
pub fn anthropic_wire_messages(
    messages: &[serde_json::Value],
) -> anyhow::Result<(Option<String>, Vec<serde_json::Value>)> {
    let leading_systems = messages
        .iter()
        .take_while(|m| m["role"].as_str() == Some("system"))
        .count();
    if messages[leading_systems..]
        .iter()
        .any(|m| m["role"].as_str() == Some("system"))
    {
        anyhow::bail!(
            "invalid Anthropic message order: system messages must precede conversation history"
        );
    }
    let system = if leading_systems == 0 {
        None
    } else {
        let joined = messages[..leading_systems]
            .iter()
            .map(|m| {
                m["content"].as_str().ok_or_else(|| {
                    anyhow::anyhow!(
                        "invalid Anthropic system message: content must be text before coalescing"
                    )
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .join("\n\n");
        Some(joined)
    };

    let mut wire: Vec<serde_json::Value> = Vec::new();
    let push_blocks =
        |role: &str, blocks: Vec<serde_json::Value>, wire: &mut Vec<serde_json::Value>| {
            if blocks.is_empty() {
                return;
            }
            // Merge into a preceding same-role message (alternation safety).
            if let Some(last) = wire.last_mut() {
                if last["role"].as_str() == Some(role) {
                    if let Some(arr) = last["content"].as_array_mut() {
                        arr.extend(blocks);
                        return;
                    }
                }
            }
            wire.push(serde_json::json!({ "role": role, "content": blocks }));
        };

    for message in &messages[leading_systems..] {
        match message["role"].as_str() {
            Some("assistant") => {
                let mut blocks = Vec::new();
                if let Some(text) = message["content"].as_str() {
                    if !text.is_empty() {
                        blocks.push(serde_json::json!({ "type": "text", "text": text }));
                    }
                }
                if let Some(calls) = message["tool_calls"].as_array() {
                    for tc in calls {
                        blocks.push(tool_use_block(tc));
                    }
                }
                push_blocks("assistant", blocks, &mut wire);
            }
            Some("tool") => {
                let content = message["content"].as_str().unwrap_or_default();
                let block = serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": message["tool_call_id"].as_str().unwrap_or_default(),
                    "content": content,
                });
                // Consecutive tool results merge into ONE user message via the
                // same-role merge in push_blocks.
                push_blocks("user", vec![block], &mut wire);
            }
            // "user" and anything unrecognized (defensive) carry as user text.
            _ => {
                let text = message["content"].as_str().unwrap_or_default();
                if !text.is_empty() {
                    push_blocks(
                        "user",
                        vec![serde_json::json!({ "type": "text", "text": text })],
                        &mut wire,
                    );
                }
            }
        }
    }

    // A trailing assistant text block must not end in whitespace (prefill
    // validation). The loop always dispatches after a user/tool append, so
    // this is defensive only.
    if let Some(last) = wire.last_mut() {
        if last["role"].as_str() == Some("assistant") {
            if let Some(arr) = last["content"].as_array_mut() {
                if let Some(text) = arr
                    .last_mut()
                    .and_then(|b| {
                        (b["type"].as_str() == Some("text"))
                            .then(|| b["text"].as_str().map(str::to_string))
                    })
                    .flatten()
                {
                    let trimmed = text.trim_end().to_string();
                    if trimmed != text {
                        arr.last_mut().expect("non-empty")["text"] =
                            serde_json::Value::String(trimmed);
                    }
                }
            }
        }
    }

    Ok((system.filter(|s| !s.is_empty()), wire))
}

/// One internal `tool_calls` element → an Anthropic `tool_use` block. The
/// `id` is replayed verbatim (within a turn these are the `toolu_…` ids the
/// server itself issued). OpenAI-style string `arguments` are parsed to an
/// object (Anthropic requires `input` to be one); unparseable arguments
/// degrade to `{}` — the paired `tool_result` already told the model what
/// happened.
fn tool_use_block(tc: &serde_json::Value) -> serde_json::Value {
    // Accept both the OpenAI nesting (`function.{name,arguments}`) and the
    // Anthropic-native flat shape (`name`/`input`) the recovery path emits.
    let f = if tc["function"].is_object() {
        &tc["function"]
    } else {
        tc
    };
    let name = f["name"].as_str().unwrap_or_default();
    let args = if !f["arguments"].is_null() {
        &f["arguments"]
    } else {
        &f["input"]
    };
    let input = match args {
        serde_json::Value::Object(_) => args.clone(),
        serde_json::Value::String(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}))
        }
        _ => serde_json::json!({}),
    };
    serde_json::json!({
        "type": "tool_use",
        "id": tc["id"].as_str().unwrap_or_default(),
        "name": name,
        "input": input,
    })
}

/// Convert the loop's tool catalog (OpenAI Chat Completions shape:
/// `{"type":"function","function":{name,description,parameters}}`) to
/// Anthropic tool definitions. `parameters` IS JSON Schema and is copied
/// wholesale into `input_schema`, so `required`/`enum`/nested schemas keep
/// their exact validation semantics (same copy-wholesale law as
/// `tools_to_responses`). Entries without a `function` object are skipped —
/// there is no faithful Anthropic rendering to invent for them.
pub fn tools_to_anthropic(tools: &serde_json::Value) -> Vec<serde_json::Value> {
    tools
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let f = &t["function"];
                    f.is_object().then(|| {
                        serde_json::json!({
                            "name": f["name"],
                            "description": f["description"],
                            "input_schema": f["parameters"],
                        })
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Assemble a `/v1/messages` request body. `system`/`tools` are omitted when
/// absent; with tools, `tool_choice` is the OBJECT form `{"type":"auto"}`
/// (the OpenAI string `"auto"` is invalid on this wire).
pub fn build_messages_body(
    model: &str,
    max_tokens: u32,
    system: Option<&str>,
    messages: &[serde_json::Value],
    tools: Option<&[serde_json::Value]>,
    stream: bool,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": stream,
    });
    if let Some(system) = system.filter(|s| !s.is_empty()) {
        body["system"] = serde_json::Value::String(system.to_string());
    }
    if let Some(tools) = tools.filter(|t| !t.is_empty()) {
        body["tools"] = serde_json::json!(tools);
        body["tool_choice"] = serde_json::json!({ "type": "auto" });
    }
    body
}

/// One decoded model round — the same shape whether it arrived as a single
/// non-streaming JSON reply ([`parse_messages_reply`]) or was accumulated
/// from SSE events ([`SseAccumulator`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnthropicRound {
    /// Concatenated `text` block content (never `thinking`).
    pub text: String,
    /// Concatenated `thinking` block content — spinner/trace only.
    pub thinking: String,
    /// Tool calls in the INTERNAL shape (`{"id", "type":"function",
    /// "function":{"name","arguments":<object>}}`) so the loop's extractor,
    /// batch validation, and history replay work unchanged.
    pub tool_uses: Vec<serde_json::Value>,
    /// `end_turn` | `tool_use` | `max_tokens` | `refusal` | `stop_sequence`
    /// | `pause_turn`.
    pub stop_reason: Option<String>,
    /// Round usage (`usage.input_tokens` / `usage.output_tokens`).
    pub usage: Option<crate::TokenUsage>,
    /// The served model id echo, when present.
    pub model: Option<String>,
    /// A mid-stream `error` event's rendering (streaming only) — the round
    /// may still carry partial text (#640 recovery policy).
    pub error: Option<String>,
}

/// Internal-shape tool call from decoded tool_use parts.
fn internal_tool_call(id: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": input },
    })
}

/// Parse a usage object (`input_tokens`/`output_tokens`). Pure; `None` when
/// either count is absent (mirrors `ollama_usage`/`openai_usage`).
pub fn anthropic_usage(usage: &serde_json::Value) -> Option<crate::TokenUsage> {
    Some(crate::TokenUsage {
        input_tokens: usage["input_tokens"].as_u64()? as u32,
        output_tokens: usage["output_tokens"].as_u64()? as u32,
    })
}

/// Decode a NON-STREAMING `/v1/messages` 200 body into a round. Fail-closed
/// on shape: a body without a `content` array yields an empty round whose
/// caller-visible symptom is the loop's empty-response diagnostic, never a
/// fabricated reply.
pub fn parse_messages_reply(json: &serde_json::Value) -> AnthropicRound {
    let mut round = AnthropicRound {
        stop_reason: json["stop_reason"].as_str().map(str::to_string),
        usage: anthropic_usage(&json["usage"]),
        model: json["model"].as_str().map(str::to_string),
        ..Default::default()
    };
    if let Some(blocks) = json["content"].as_array() {
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => {
                    round
                        .text
                        .push_str(block["text"].as_str().unwrap_or_default());
                }
                Some("thinking") => {
                    round
                        .thinking
                        .push_str(block["thinking"].as_str().unwrap_or_default());
                }
                Some("tool_use") => {
                    round.tool_uses.push(internal_tool_call(
                        block["id"].as_str().unwrap_or_default(),
                        block["name"].as_str().unwrap_or_default(),
                        block["input"].clone(),
                    ));
                }
                // redacted_thinking, server_tool_use, citations, … — ignored
                // (out of scope for this wire's v1).
                _ => {}
            }
        }
    }
    round
}

/// What the SSE consumer should surface live while a round streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamAction {
    /// Visible answer text — print it.
    TextDelta(String),
    /// Thinking text — spinner detail only, never the transcript.
    ThinkingDelta(String),
}

/// One in-flight content block (by SSE `index`).
#[derive(Debug, Clone)]
enum OpenBlock {
    Text,
    Thinking,
    ToolUse {
        id: String,
        name: String,
        /// `input_json_delta.partial_json` accumulates here and is parsed
        /// ONLY at `content_block_stop` — partial JSON is never parsed.
        partial: String,
    },
}

/// Event-machine for the `/v1/messages` SSE stream.
///
/// Feed raw HTTP chunks with [`feed`]; it maintains a rolling line buffer
/// (an SSE `data:` line routinely splits across chunks — the known weakness
/// of per-chunk `lines()` splitting), dispatches complete events keyed on
/// the payload's `"type"`, and returns display actions. When the stream
/// ends, [`finish`] flushes the tail and returns the completed round.
#[derive(Debug, Default)]
pub struct SseAccumulator {
    line_buf: String,
    open: std::collections::HashMap<u64, OpenBlock>,
    round: AnthropicRound,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    done: bool,
}

impl SseAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once `message_stop` (or an `error` event) arrived.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Feed one raw HTTP chunk; returns the display actions it produced.
    pub fn feed(&mut self, chunk: &str) -> Vec<StreamAction> {
        let mut actions = Vec::new();
        self.line_buf.push_str(chunk);
        while let Some(pos) = self.line_buf.find('\n') {
            let line: String = self.line_buf.drain(..=pos).collect();
            self.apply_line(line.trim_end_matches(['\n', '\r']), &mut actions);
        }
        actions
    }

    /// Flush any unterminated final line and return the completed round.
    pub fn finish(mut self) -> AnthropicRound {
        if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            let mut actions = Vec::new();
            self.apply_line(line.trim_end_matches(['\n', '\r']), &mut actions);
        }
        if self.round.usage.is_none() {
            self.round.usage = match (self.input_tokens, self.output_tokens) {
                (Some(input), Some(output)) => Some(crate::TokenUsage {
                    input_tokens: input,
                    output_tokens: output,
                }),
                _ => None,
            };
        }
        self.round
    }

    fn apply_line(&mut self, line: &str, actions: &mut Vec<StreamAction>) {
        // `event:` lines are redundant (the data payload carries `type`) and
        // blank lines merely terminate an event — each `data:` line here is a
        // complete JSON payload per the Messages streaming contract.
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(data.trim()) else {
            return;
        };
        match json["type"].as_str() {
            Some("message_start") => {
                self.input_tokens = json["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .map(|v| v as u32);
                if self.round.model.is_none() {
                    self.round.model = json["message"]["model"].as_str().map(str::to_string);
                }
            }
            Some("content_block_start") => {
                let Some(index) = json["index"].as_u64() else {
                    return;
                };
                let block = &json["content_block"];
                let open = match block["type"].as_str() {
                    Some("tool_use") => OpenBlock::ToolUse {
                        id: block["id"].as_str().unwrap_or_default().to_string(),
                        name: block["name"].as_str().unwrap_or_default().to_string(),
                        partial: String::new(),
                    },
                    Some("thinking") => OpenBlock::Thinking,
                    _ => OpenBlock::Text,
                };
                self.open.insert(index, open);
            }
            Some("content_block_delta") => {
                let Some(index) = json["index"].as_u64() else {
                    return;
                };
                let delta = &json["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let t = delta["text"].as_str().unwrap_or_default();
                        if !t.is_empty() {
                            self.round.text.push_str(t);
                            actions.push(StreamAction::TextDelta(t.to_string()));
                        }
                    }
                    Some("thinking_delta") => {
                        let t = delta["thinking"].as_str().unwrap_or_default();
                        if !t.is_empty() {
                            self.round.thinking.push_str(t);
                            actions.push(StreamAction::ThinkingDelta(t.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(OpenBlock::ToolUse { partial, .. }) = self.open.get_mut(&index)
                        {
                            partial.push_str(delta["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let Some(index) = json["index"].as_u64() else {
                    return;
                };
                if let Some(OpenBlock::ToolUse { id, name, partial }) = self.open.remove(&index) {
                    // A zero-argument tool streams no input_json_delta at all
                    // → `{}`. Unparseable JSON stays a STRING argument so the
                    // loop's batch validation rejects it with a keyed error
                    // tool_result (the model gets to retry) instead of this
                    // decoder inventing arguments.
                    let input: serde_json::Value = if partial.trim().is_empty() {
                        serde_json::json!({})
                    } else {
                        serde_json::from_str(&partial).unwrap_or(serde_json::Value::String(partial))
                    };
                    self.round
                        .tool_uses
                        .push(internal_tool_call(&id, &name, input));
                }
            }
            Some("message_delta") => {
                if let Some(reason) = json["delta"]["stop_reason"].as_str() {
                    self.round.stop_reason = Some(reason.to_string());
                }
                // Cumulative — take the latest, never sum.
                if let Some(out) = json["usage"]["output_tokens"].as_u64() {
                    self.output_tokens = Some(out as u32);
                }
            }
            Some("message_stop") => {
                self.done = true;
            }
            Some("error") => {
                self.round.error = Some(json["error"].to_string());
                self.done = true;
            }
            // ping and unknown event types: ignore (forward compatibility).
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- anthropic_wire_messages ---

    #[test]
    fn leading_system_run_coalesces_into_top_level_system() {
        let messages = vec![
            json!({"role": "system", "content": "base prompt"}),
            json!({"role": "system", "content": "active card"}),
            json!({"role": "user", "content": "hi"}),
        ];
        let (system, wire) = anthropic_wire_messages(&messages).unwrap();
        assert_eq!(system.as_deref(), Some("base prompt\n\nactive card"));
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn late_system_message_is_rejected_not_promoted() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "sneaky"}),
        ];
        assert!(anthropic_wire_messages(&messages).is_err());
    }

    #[test]
    fn assistant_tool_calls_become_tool_use_blocks_with_verbatim_ids() {
        let messages = vec![
            json!({"role": "user", "content": "list files"}),
            json!({"role": "assistant", "content": "Let me look.", "tool_calls": [
                {"id": "toolu_01", "type": "function",
                 "function": {"name": "list_dir", "arguments": {"path": "."}}},
            ]}),
            json!({"role": "tool", "tool_call_id": "toolu_01", "content": "src/ docs/"}),
        ];
        let (_, wire) = anthropic_wire_messages(&messages).unwrap();
        assert_eq!(wire.len(), 3);
        let assistant = &wire[1];
        assert_eq!(assistant["content"][0]["type"], "text");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert_eq!(assistant["content"][1]["id"], "toolu_01");
        assert_eq!(assistant["content"][1]["name"], "list_dir");
        assert_eq!(assistant["content"][1]["input"]["path"], ".");
        let result = &wire[2];
        assert_eq!(result["role"], "user");
        assert_eq!(result["content"][0]["type"], "tool_result");
        assert_eq!(result["content"][0]["tool_use_id"], "toolu_01");
        assert_eq!(result["content"][0]["content"], "src/ docs/");
    }

    #[test]
    fn string_arguments_parse_to_objects_and_garbage_degrades_to_empty() {
        let with_string = json!({"id": "t1", "type": "function",
            "function": {"name": "f", "arguments": "{\"k\": 1}"}});
        let block = tool_use_block(&with_string);
        assert_eq!(block["input"]["k"], 1);
        let garbage = json!({"id": "t2", "type": "function",
            "function": {"name": "f", "arguments": "{not json"}});
        let block = tool_use_block(&garbage);
        assert_eq!(block["input"], json!({}));
        // Anthropic-native flat shape (recovery path) converts too.
        let flat = json!({"id": "t3", "name": "f", "input": {"k": 2}});
        let block = tool_use_block(&flat);
        assert_eq!(block["name"], "f");
        assert_eq!(block["input"]["k"], 2);
    }

    #[test]
    fn consecutive_tool_results_land_in_one_user_message() {
        // Two parallel calls → both results in the single next user message
        // (a hard Anthropic requirement).
        let messages = vec![
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "f", "arguments": {}}},
                {"id": "b", "type": "function", "function": {"name": "g", "arguments": {}}},
            ]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "ra"}),
            json!({"role": "tool", "tool_call_id": "b", "content": "rb"}),
        ];
        let (_, wire) = anthropic_wire_messages(&messages).unwrap();
        assert_eq!(wire.len(), 3);
        let results = &wire[2];
        assert_eq!(results["role"], "user");
        let blocks = results["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
    }

    #[test]
    fn adjacent_same_role_messages_merge_for_alternation() {
        // A tool-result user message followed by a user nudge must merge —
        // strict proxies 400 on consecutive same-role messages.
        let messages = vec![
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "f", "arguments": {}}},
            ]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "result"}),
            json!({"role": "user", "content": "nudge: narrate before acting"}),
        ];
        let (_, wire) = anthropic_wire_messages(&messages).unwrap();
        assert_eq!(
            wire.len(),
            3,
            "tool result + nudge merged into one user turn"
        );
        let merged = wire[2]["content"].as_array().unwrap();
        assert_eq!(merged[0]["type"], "tool_result");
        assert_eq!(merged[1]["type"], "text");
        // Roles strictly alternate.
        let roles: Vec<_> = wire.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
    }

    #[test]
    fn empty_text_and_empty_messages_are_dropped() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "user", "content": "again"}),
        ];
        let (_, wire) = anthropic_wire_messages(&messages).unwrap();
        // The empty assistant vanished; the two user turns then merged.
        assert_eq!(wire.len(), 1);
        let blocks = wire[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
    }

    // --- tools_to_anthropic ---

    #[test]
    fn tools_map_to_input_schema_wholesale() {
        let tools = json!([{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                    "additionalProperties": false
                }
            }
        }]);
        let out = tools_to_anthropic(&tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "read_file");
        assert_eq!(out[0]["input_schema"]["required"][0], "path");
        assert_eq!(out[0]["input_schema"]["additionalProperties"], false);
        assert!(out[0].get("function").is_none(), "no OpenAI nesting");
        assert!(out[0].get("type").is_none(), "no type:function wrapper");
    }

    // --- build_messages_body ---

    #[test]
    fn body_carries_required_max_tokens_and_object_tool_choice() {
        let messages = vec![json!({"role": "user", "content": [{"type":"text","text":"hi"}]})];
        let tools = vec![json!({"name": "f", "description": "", "input_schema": {}})];
        let body =
            build_messages_body("claude-x", 4096, Some("sys"), &messages, Some(&tools), true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"], "sys");
        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
        assert_eq!(body["stream"], true);
        // Toolless summary request: no tools key at all.
        let body = build_messages_body("claude-x", 4096, None, &messages, None, false);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("system").is_none());
        assert_eq!(body["stream"], false);
    }

    // --- parse_messages_reply ---

    #[test]
    fn reply_decodes_text_tools_usage_and_stop_reason() {
        let json = json!({
            "model": "claude-sonnet-4-5",
            "stop_reason": "tool_use",
            "content": [
                {"type": "text", "text": "Checking. "},
                {"type": "thinking", "thinking": "hmm"},
                {"type": "tool_use", "id": "toolu_9", "name": "read_file",
                 "input": {"path": "a.rs"}}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 25}
        });
        let round = parse_messages_reply(&json);
        assert_eq!(round.text, "Checking. ");
        assert_eq!(round.thinking, "hmm");
        assert_eq!(round.tool_uses.len(), 1);
        assert_eq!(round.tool_uses[0]["id"], "toolu_9");
        assert_eq!(round.tool_uses[0]["function"]["name"], "read_file");
        assert_eq!(round.tool_uses[0]["function"]["arguments"]["path"], "a.rs");
        assert_eq!(round.stop_reason.as_deref(), Some("tool_use"));
        let usage = round.usage.unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 25);
        assert_eq!(round.model.as_deref(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn usage_parses_or_none() {
        let u = anthropic_usage(&json!({"input_tokens": 7, "output_tokens": 3})).unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 3);
        assert!(anthropic_usage(&json!({"input_tokens": 7})).is_none());
        assert!(anthropic_usage(&json!({})).is_none());
    }

    // --- SseAccumulator ---

    fn frame(json: serde_json::Value) -> String {
        format!("data: {json}\n\n")
    }

    #[test]
    fn sse_streams_text_and_merges_usage() {
        let mut acc = SseAccumulator::new();
        let mut actions = Vec::new();
        actions.extend(acc.feed(&frame(json!({"type": "message_start",
            "message": {"model": "claude-x", "usage": {"input_tokens": 7}}}))));
        actions.extend(acc.feed(&frame(json!({"type": "content_block_start",
            "index": 0, "content_block": {"type": "text"}}))));
        actions.extend(acc.feed(&frame(json!({"type": "content_block_delta",
            "index": 0, "delta": {"type": "text_delta", "text": "Hello "}}))));
        actions.extend(acc.feed(&frame(json!({"type": "content_block_delta",
            "index": 0, "delta": {"type": "text_delta", "text": "world"}}))));
        actions.extend(acc.feed(&frame(json!({"type": "content_block_stop", "index": 0}))));
        actions.extend(acc.feed(&frame(json!({"type": "message_delta",
            "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}))));
        actions.extend(acc.feed(&frame(json!({"type": "message_stop"}))));
        assert!(acc.is_done());
        assert_eq!(
            actions,
            vec![
                StreamAction::TextDelta("Hello ".into()),
                StreamAction::TextDelta("world".into())
            ]
        );
        let round = acc.finish();
        assert_eq!(round.text, "Hello world");
        assert_eq!(round.stop_reason.as_deref(), Some("end_turn"));
        let usage = round.usage.unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 3);
    }

    #[test]
    fn sse_lines_split_across_chunks_reassemble() {
        // The known transport reality: a data: line routinely splits across
        // HTTP chunks — the rolling buffer must reassemble it.
        let mut acc = SseAccumulator::new();
        let whole = frame(json!({"type": "content_block_start",
            "index": 0, "content_block": {"type": "text"}}))
            + &frame(json!({"type": "content_block_delta",
                "index": 0, "delta": {"type": "text_delta", "text": "reassembled"}}));
        let mut actions = Vec::new();
        for chunk in whole.as_bytes().chunks(7) {
            actions.extend(acc.feed(std::str::from_utf8(chunk).unwrap()));
        }
        assert_eq!(actions, vec![StreamAction::TextDelta("reassembled".into())]);
    }

    #[test]
    fn sse_input_json_delta_accumulates_and_parses_at_stop() {
        let mut acc = SseAccumulator::new();
        acc.feed(&frame(json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_5", "name": "write_file"}})));
        acc.feed(&frame(json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\": \"a"}})));
        acc.feed(&frame(json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": ".rs\", \"content\": \"x\"}"}})));
        acc.feed(&frame(json!({"type": "content_block_stop", "index": 0})));
        acc.feed(&frame(json!({"type": "message_delta",
            "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 9}})));
        acc.feed(&frame(json!({"type": "message_stop"})));
        let round = acc.finish();
        assert_eq!(round.tool_uses.len(), 1);
        assert_eq!(round.tool_uses[0]["function"]["arguments"]["path"], "a.rs");
        assert_eq!(round.tool_uses[0]["function"]["arguments"]["content"], "x");
        assert_eq!(round.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn sse_zero_argument_tool_use_yields_empty_object() {
        let mut acc = SseAccumulator::new();
        acc.feed(&frame(json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_z", "name": "list_dir"}})));
        acc.feed(&frame(json!({"type": "content_block_stop", "index": 0})));
        acc.feed(&frame(json!({"type": "message_stop"})));
        let round = acc.finish();
        assert_eq!(round.tool_uses[0]["function"]["arguments"], json!({}));
    }

    #[test]
    fn sse_malformed_tool_json_stays_a_string_for_validation() {
        // The decoder must not invent arguments — a string survives to batch
        // validation, which rejects it with a keyed error the model can fix.
        let mut acc = SseAccumulator::new();
        acc.feed(&frame(json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_m", "name": "f"}})));
        acc.feed(&frame(json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{broken"}})));
        acc.feed(&frame(json!({"type": "content_block_stop", "index": 0})));
        let round = acc.finish();
        assert_eq!(
            round.tool_uses[0]["function"]["arguments"],
            json!("{broken")
        );
    }

    #[test]
    fn sse_thinking_routes_to_spinner_not_text() {
        let mut acc = SseAccumulator::new();
        acc.feed(&frame(json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "thinking"}})));
        let actions = acc.feed(&frame(json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "thinking_delta", "thinking": "pondering"}})));
        assert_eq!(
            actions,
            vec![StreamAction::ThinkingDelta("pondering".into())]
        );
        let round = acc.finish();
        assert_eq!(round.text, "");
        assert_eq!(round.thinking, "pondering");
    }

    #[test]
    fn sse_error_event_marks_round_and_keeps_partial_text() {
        // #640 policy input: a mid-stream overloaded error after visible
        // output — the round keeps the partial text and carries the error.
        let mut acc = SseAccumulator::new();
        acc.feed(&frame(json!({"type": "content_block_start",
            "index": 0, "content_block": {"type": "text"}})));
        acc.feed(&frame(json!({"type": "content_block_delta",
            "index": 0, "delta": {"type": "text_delta", "text": "partial"}})));
        acc.feed(&frame(json!({"type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}})));
        assert!(acc.is_done());
        let round = acc.finish();
        assert_eq!(round.text, "partial");
        assert!(round.error.as_deref().unwrap().contains("overloaded_error"));
    }

    #[test]
    fn sse_ping_and_unknown_events_are_ignored() {
        let mut acc = SseAccumulator::new();
        let actions = acc.feed(&frame(json!({"type": "ping"})));
        assert!(actions.is_empty());
        let actions = acc.feed(&frame(json!({"type": "brand_new_event"})));
        assert!(actions.is_empty());
        assert!(!acc.is_done());
    }

    // --- misc ---

    #[test]
    fn messages_url_appends_v1_messages() {
        assert_eq!(
            messages_url("https://api.anthropic.com/"),
            "https://api.anthropic.com/v1/messages"
        );
    }
}
