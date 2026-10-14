//! Diagnostics for an Ollama reply that spent output tokens but carried no
//! assistant-visible content or tool call: response-shape tracing, the retry
//! nudge, and the final diagnostic.
//!
//! Moved verbatim from `agentic/mod.rs`.

pub(super) fn ollama_non_content_fields(json: &serde_json::Value) -> Vec<&'static str> {
    let message = &json["message"];
    ["reasoning", "reasoning_content", "thinking"]
        .into_iter()
        .filter(|field| {
            message[*field]
                .as_str()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        })
        .collect()
}

pub(super) fn ollama_response_shape(json: &serde_json::Value) -> String {
    let message = &json["message"];
    let message_keys = message
        .as_object()
        .map(|obj| obj.keys().cloned().collect::<Vec<_>>().join(","))
        .unwrap_or_else(|| "<missing>".to_string());
    let content_chars = message["content"]
        .as_str()
        .map(|content| content.chars().count())
        .unwrap_or(0);
    let tool_calls = message["tool_calls"]
        .as_array()
        .map(|calls| calls.len())
        .unwrap_or(0);
    let non_content = ollama_non_content_fields(json);
    let non_content = if non_content.is_empty() {
        "none".to_string()
    } else {
        non_content.join(",")
    };
    format!(
        "ollama response shape: message_keys=[{message_keys}] content_chars={content_chars} tool_calls={tool_calls} non_content_fields=[{non_content}] prompt_eval_count={} eval_count={}",
        json["prompt_eval_count"]
            .as_u64()
            .map_or("missing".to_string(), |n| n.to_string()),
        json["eval_count"]
            .as_u64()
            .map_or("missing".to_string(), |n| n.to_string())
    )
}

pub(super) fn suspicious_empty_retry_nudge(retry_index: u32, json: &serde_json::Value) -> String {
    if retry_index == 0 {
        return "Your previous response produced generated tokens but no assistant-visible content \
                and no tool call. Reply with either a tool call or final assistant content."
            .to_string();
    }
    let fields = ollama_non_content_fields(json);
    let field_note = if fields.is_empty() {
        "hidden/non-content fields".to_string()
    } else {
        format!("hidden/non-content field(s): {}", fields.join(", "))
    };
    format!(
        "Your previous response again produced generated tokens only in {field_note}, \
         with no assistant-visible content and no tool call. Hidden thinking is not an \
         action. If you intend to act, emit the exact tool call now; otherwise reply \
         with final assistant-visible content. Do not continue with hidden-only reasoning."
    )
}

pub(super) fn suspicious_empty_ollama_diagnostic(json: &serde_json::Value) -> String {
    let fields = ollama_non_content_fields(json);
    let field_note = if fields.is_empty() {
        "no known non-content fields were present".to_string()
    } else {
        format!("non-content field(s) present: {}", fields.join(", "))
    };
    format!(
        "(model generated output tokens but returned no assistant-visible content or tool calls; {field_note}; rerun with `newt --trace` to capture the response shape)"
    )
}
