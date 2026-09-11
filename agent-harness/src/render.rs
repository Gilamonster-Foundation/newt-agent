//! Pure provider rendering shared by live dispatch and cold replay.

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
) -> crate::Result<(Option<String>, Vec<serde_json::Value>)> {
    let leading_systems = messages
        .iter()
        .take_while(|m| m["role"].as_str() == Some("system"))
        .count();
    if messages[leading_systems..]
        .iter()
        .any(|m| m["role"].as_str() == Some("system"))
    {
        return Err(crate::Error::Proposal(
            "invalid Anthropic message order: system messages must precede conversation history"
                .into(),
        ));
    }
    let system = if leading_systems == 0 {
        None
    } else {
        let joined = messages[..leading_systems]
            .iter()
            .map(|m| {
                m["content"].as_str().ok_or_else(|| {
                    crate::Error::Proposal(
                        "invalid Anthropic system message: content must be text before coalescing"
                            .into(),
                    )
                })
            })
            .collect::<crate::Result<Vec<_>>>()?
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
pub fn tool_use_block(tc: &serde_json::Value) -> serde_json::Value {
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
