//! The one owner of the OpenAI **Responses API** wire contract (`POST
//! /v1/responses`), in **both** directions, shared by the newt-inference
//! transport (the [`InferenceBackend`](../../newt_inference/backend/trait.InferenceBackend.html)
//! `ChatReply` seam) and the newt-core agentic loop:
//!
//! - **encode** — [`build_responses_input`] splits chat-style messages into the
//!   `(instructions, input)` the request body carries.
//! - **decode** — [`decode_response`] parses the reply payload into a typed
//!   [`DecodedResponse`].
//!
//! Before this module there were TWO hand-rolled copies of *each* direction —
//! one pair in `newt-inference/src/responses.rs`, one in
//! `newt-core/src/agentic/mod.rs` — that had drifted (the inference decoder
//! gated on `part.type == "output_text"`; the agentic one pulled `part.text`
//! from any part and also extracted `function_call` / `reasoning`; the two
//! request builders joined `instructions` with different separators). Two
//! implementations of one wire shape is the sprawl this workspace treats as a
//! bug class, so this is the single owner. (Backend-neutral *policy* — tools,
//! reasoning effort, budgeting — stays in the agentic loop; only the wire
//! shaping lives here.)
//!
//! ## Invariant: HTTP `2xx` is NOT a completed response
//!
//! A `200 OK` transport status says only "the request was accepted". Whether the
//! *turn* finished lives in the body. [`decode_response`] is **fail-closed**: it
//! returns `Ok(DecodedResponse)` ONLY when the body carries affirmative success
//! output, and a typed [`ResponseDecodeError`] for every other case — a
//! top-level error (which always wins), a `failed` / `incomplete` / non-terminal
//! status, an explicit `refusal`, or a malformed/empty body. No consumer can
//! mistake a truncated, failed, refused, or empty body for an empty-but-fine
//! reply.

use serde_json::Value;

/// Whether a Responses request opts into OpenAI **server-side storage**
/// (`store: true`) of the response payload.
///
/// Newt's policy is `false` — **stateless** (#1526, invariant #5; the
/// sovereign-data doctrine). The harness replays the full turn history on every
/// request and never uses `previous_response_id`, so server-side retention buys
/// nothing — it would only leave an implicit, unaudited copy of the operator's
/// prompts, source, and reasoning on the provider. The Responses API defaults
/// `store` to **`true`**, so this MUST be set on every request; relying on the
/// default silently retains data. Both Responses request builders (the agentic
/// loop and the inference transport) send this one value, so the policy is
/// explicit and identical on every surface.
pub const STORE_RESPONSE_SERVER_SIDE: bool = false;

/// Split chat-style messages into the Responses API's `(instructions, input)`:
/// `system`/`developer` messages concatenate into top-level `instructions`;
/// `user`/`assistant` become `input` message items with plain string content.
/// Any item already shaped as a Responses item (carrying a `type` field, e.g.
/// `function_call` / `function_call_output` / `reasoning`) passes through
/// untouched, preserving output order — the reasoning-echo contract the agentic
/// loop relies on.
///
/// This is the single request-shaper both the agentic loop and the inference
/// transport call, so the two can never drift on the instructions/input split.
#[must_use]
pub fn build_responses_input(messages: &[Value]) -> (Option<String>, Vec<Value>) {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();
    for m in messages {
        if m.get("type").is_some() {
            input.push(m.clone());
            continue;
        }
        let role = m["role"].as_str().unwrap_or("user");
        let content = m["content"].as_str().unwrap_or("");
        match role {
            "system" | "developer" => instructions.push(content.to_string()),
            _ => input.push(serde_json::json!({ "role": role, "content": content })),
        }
    }
    let ins = (!instructions.is_empty()).then(|| instructions.join("\n\n"));
    (ins, input)
}

/// A successfully decoded, **completed** Responses turn. Produced ONLY when the
/// body carries affirmative evidence of success (assistant text and/or tool
/// calls) and no terminal error. The `echo` items stay as raw [`Value`]s because
/// the agentic loop replays them **verbatim** in the next request's `input` (the
/// Responses API requires the exact `function_call` + preceding `reasoning`).
#[derive(Debug, Clone)]
pub struct DecodedResponse {
    /// Concatenated assistant text: every `message` item's `output_text` part,
    /// with a flat top-level `output_text` fallback.
    pub text: String,
    /// Raw `function_call` output items, in output order — the tool calls the
    /// model requested.
    pub tool_calls: Vec<Value>,
    /// Raw items to ECHO back with the tool calls, in output order: every
    /// `function_call` AND the `reasoning` items that precede them.
    pub echo: Vec<Value>,
    /// The model id the server reports (`model`), if present.
    pub model: Option<String>,
    /// Token usage (`input_tokens`/`output_tokens`), if present.
    pub usage: Option<crate::TokenUsage>,
}

/// Why a Responses body is NOT a usable completed turn. Kept distinct so a caller
/// cannot collapse a failure, a refusal, or a malformed body into an empty
/// "success". A `200 OK` transport status only means the request was accepted;
/// [`decode_response`] returns `Ok` ONLY with affirmative success output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseDecodeError {
    /// The model explicitly declined (a `refusal` content part) with no other
    /// usable output. Represented separately from a provider/turn failure.
    Refused {
        message: String,
        usage: Option<crate::TokenUsage>,
    },
    /// A top-level `error` object/string is present. This ALWAYS wins, regardless
    /// of `status` — a status-less error body is still a failure, not a success.
    ProviderError(String),
    /// `status == "failed"` (with no top-level error object).
    Failed(String),
    /// `status == "incomplete"` — the turn was truncated (e.g. `max_output_tokens`).
    Incomplete { reason: Option<String> },
    /// `status` present but not a recognized terminal value (`in_progress`,
    /// `cancelled`, or anything unknown).
    NonTerminal(String),
    /// No terminal error and a completed/absent status, but NO affirmative
    /// success output (no text, no tool calls, no recognized output item). A
    /// `2xx` body with nothing usable is malformed, not an empty success.
    Malformed(String),
}

impl std::fmt::Display for ResponseDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { message, .. } => write!(f, "the model refused the request: {message}"),
            Self::ProviderError(m) => write!(f, "the Responses provider reported an error: {m}"),
            Self::Failed(m) => write!(f, "Responses turn failed: {m}"),
            Self::Incomplete { reason } => write!(
                f,
                "Responses turn did not complete ({})",
                reason.as_deref().unwrap_or("incomplete")
            ),
            Self::NonTerminal(s) => {
                write!(f, "Responses turn ended with non-terminal status {s:?}")
            }
            Self::Malformed(m) => write!(f, "malformed Responses body: {m}"),
        }
    }
}

impl std::error::Error for ResponseDecodeError {}

/// Decode a Responses-API JSON body into a **completed** [`DecodedResponse`], or a
/// typed [`ResponseDecodeError`] for every body that is not a trustworthy success.
///
/// Fail-CLOSED. In order: a top-level `error` always wins; an explicit `failed`
/// / non-terminal status is an error; a truncated (`incomplete`) turn is an
/// error; a `refusal`-only body is [`Refused`](ResponseDecodeError::Refused); a
/// completed-or-absent status is `Ok` ONLY if it carries affirmative output
/// (text or tool calls) — an empty or shape-unrecognized body is
/// [`Malformed`](ResponseDecodeError::Malformed), never an empty success. A
/// missing `status` (lenient/older servers) is accepted only with such output.
pub fn decode_response(json: &Value) -> Result<DecodedResponse, ResponseDecodeError> {
    let usage = decode_usage(&json["usage"]);
    let model = json["model"].as_str().map(str::to_string);

    // 1. A top-level error ALWAYS wins, whatever the status says.
    if let Some(message) = top_level_error(json) {
        return Err(ResponseDecodeError::ProviderError(message));
    }
    // 2. Explicit terminal / non-terminal statuses.
    match json["status"].as_str() {
        Some("failed") => {
            return Err(ResponseDecodeError::Failed(
                "the provider reported status \"failed\" with no error detail".to_string(),
            ));
        }
        Some("incomplete") => {
            return Err(ResponseDecodeError::Incomplete {
                reason: json["incomplete_details"]["reason"]
                    .as_str()
                    .map(str::to_string),
            });
        }
        // Only `completed` / absent may proceed to the success check below.
        Some(other) if other != "completed" => {
            return Err(ResponseDecodeError::NonTerminal(other.to_string()));
        }
        _ => {}
    }
    // 3. Parse the output; a refusal is represented explicitly.
    let (text, tool_calls, echo, refusal) = decode_output(json);
    let has_output = !text.is_empty() || !tool_calls.is_empty();
    if let Some(message) = refusal {
        if !has_output {
            return Err(ResponseDecodeError::Refused { message, usage });
        }
    }
    // 4. A completed/absent status REQUIRES affirmative success output.
    if !has_output {
        return Err(ResponseDecodeError::Malformed(
            "no assistant text, tool calls, or recognized output".to_string(),
        ));
    }
    Ok(DecodedResponse {
        text,
        tool_calls,
        echo,
        model,
        usage,
    })
}

/// A top-level `error` (object with `message`, or a bare string). Returns `None`
/// only when `error` is absent/null.
fn top_level_error(json: &Value) -> Option<String> {
    match &json["error"] {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => Some(
            o.get("message")
                .and_then(Value::as_str)
                .unwrap_or("the provider returned an error object with no message")
                .to_string(),
        ),
        other => Some(other.to_string()),
    }
}

/// Extract `(text, tool_calls, echo, refusal)`. `refusal` is `Some` when a
/// `message` item carries a `refusal` content part (the model declining).
fn decode_output(json: &Value) -> (String, Vec<Value>, Vec<Value>, Option<String>) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut echo = Vec::new();
    let mut refusal = String::new();
    if let Some(items) = json["output"].as_array() {
        for item in items {
            match item["type"].as_str() {
                Some("message") => {
                    if let Some(parts) = item["content"].as_array() {
                        for p in parts {
                            // A `refusal` part carries a `refusal` field; an
                            // `output_text` part carries `text`. Capture both.
                            if p["type"] == "refusal" {
                                if let Some(r) = p["refusal"].as_str() {
                                    refusal.push_str(r);
                                }
                            } else if let Some(t) = p["text"].as_str() {
                                text.push_str(t);
                            }
                        }
                    }
                }
                Some("function_call") => {
                    tool_calls.push(item.clone());
                    echo.push(item.clone());
                }
                // Reasoning items (`rs_…`) carry the chain that produced the
                // following function_call; the Responses API requires them echoed
                // back alongside the call, so preserve them in output order.
                Some("reasoning") => echo.push(item.clone()),
                _ => {}
            }
        }
    }
    if text.is_empty() {
        if let Some(t) = json["output_text"].as_str() {
            text.push_str(t);
        }
    }
    let refusal = (!refusal.is_empty()).then_some(refusal);
    (text, tool_calls, echo, refusal)
}

/// Responses API usage (`input_tokens`/`output_tokens`), accepting the Chat
/// Completions names (`prompt_tokens`/`completion_tokens`) from lenient servers.
/// Pass the `usage` sub-object, not the whole body.
fn decode_usage(usage: &Value) -> Option<crate::TokenUsage> {
    let input = usage["input_tokens"]
        .as_u64()
        .or_else(|| usage["prompt_tokens"].as_u64())
        .map(|n| n as u32);
    let output = usage["output_tokens"]
        .as_u64()
        .or_else(|| usage["completion_tokens"].as_u64())
        .map(|n| n as u32);
    input.zip(output).map(|(i, o)| crate::TokenUsage {
        input_tokens: i,
        output_tokens: o,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_input_splits_system_to_instructions_and_passes_typed_items() {
        let msgs = vec![
            json!({"role": "system", "content": "be terse"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
            // an already-typed Responses item passes through untouched, in order
            json!({"type": "function_call_output", "call_id": "c1", "output": "ok"}),
        ];
        let (instructions, input) = build_responses_input(&msgs);
        assert_eq!(instructions.as_deref(), Some("be terse"));
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "hi");
        assert_eq!(input[2]["type"], "function_call_output");
    }

    #[test]
    fn build_input_joins_multiple_system_messages_and_treats_developer_as_system() {
        let msgs = vec![
            json!({"role": "system", "content": "one"}),
            json!({"role": "developer", "content": "two"}),
            json!({"role": "user", "content": "go"}),
        ];
        let (instructions, input) = build_responses_input(&msgs);
        // system + developer concatenate into instructions, joined with a blank line.
        assert_eq!(instructions.as_deref(), Some("one\n\ntwo"));
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn build_input_has_no_instructions_when_no_system_message() {
        let (instructions, input) =
            build_responses_input(&[json!({"role": "user", "content": "x"})]);
        assert_eq!(instructions, None);
        assert_eq!(input.len(), 1);
    }

    #[test]
    fn completed_message_extracts_text_and_usage() {
        let body = json!({
            "status": "completed",
            "model": "gpt-5.6-sol",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "the answer"}],
            }],
            "usage": {"input_tokens": 11, "output_tokens": 7},
        });
        let d = decode_response(&body).expect("completed success");
        assert_eq!(d.text, "the answer");
        assert_eq!(d.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(d.usage.unwrap().input_tokens, 11);
        assert!(d.tool_calls.is_empty());
    }

    #[test]
    fn missing_status_with_output_text_succeeds() {
        // Legacy/lenient compatibility: absent status + affirmative output is Ok.
        let d = decode_response(&json!({"output_text": "ok"})).expect("legacy success");
        assert_eq!(d.text, "ok");
    }

    #[test]
    fn missing_status_with_structured_message_succeeds() {
        let d = decode_response(&json!({
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}]
        }))
        .expect("structured success");
        assert_eq!(d.text, "hi");
    }

    #[test]
    fn missing_status_with_top_level_error_fails() {
        let err = decode_response(&json!({"error": {"message": "provider failure"}}))
            .expect_err("a status-less error body is a failure, not empty success");
        assert!(matches!(err, ResponseDecodeError::ProviderError(m) if m == "provider failure"));
    }

    #[test]
    fn top_level_error_wins_over_a_completed_status() {
        // Even a `completed` status with output must fail if `error` is present.
        let err = decode_response(&json!({
            "status": "completed",
            "error": {"message": "late failure"},
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "x"}]}]
        }))
        .expect_err("top-level error always wins");
        assert!(matches!(err, ResponseDecodeError::ProviderError(_)));
    }

    #[test]
    fn empty_object_body_is_malformed() {
        assert!(matches!(
            decode_response(&json!({})),
            Err(ResponseDecodeError::Malformed(_))
        ));
    }

    #[test]
    fn completed_but_empty_body_is_malformed_not_empty_success() {
        assert!(matches!(
            decode_response(&json!({"status": "completed", "output": []})),
            Err(ResponseDecodeError::Malformed(_))
        ));
    }

    #[test]
    fn refusal_only_response_returns_refused() {
        let body = json!({
            "output": [{
                "type": "message",
                "content": [{"type": "refusal", "refusal": "I cannot comply"}]
            }],
            "usage": {"input_tokens": 4, "output_tokens": 6}
        });
        match decode_response(&body) {
            Err(ResponseDecodeError::Refused { message, usage }) => {
                assert_eq!(message, "I cannot comply");
                assert_eq!(usage.unwrap().output_tokens, 6);
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_status_is_an_error_carrying_the_reason() {
        let body = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message",
                "content": [{"type": "output_text", "text": "partial…"}]}],
        });
        assert!(matches!(
            decode_response(&body),
            Err(ResponseDecodeError::Incomplete { reason: Some(r) }) if r == "max_output_tokens"
        ));
    }

    #[test]
    fn failed_status_is_an_error() {
        let err = decode_response(&json!({"status": "failed"}))
            .expect_err("failed status must be an error");
        assert!(matches!(err, ResponseDecodeError::Failed(_)));
    }

    #[test]
    fn unknown_status_is_non_terminal_error() {
        assert!(matches!(
            decode_response(&json!({"status": "in_progress"})),
            Err(ResponseDecodeError::NonTerminal(s)) if s == "in_progress"
        ));
    }

    #[test]
    fn unknown_output_item_with_no_usable_content_is_malformed() {
        // A shape we don't recognize, no text, no calls → malformed, not success.
        assert!(matches!(
            decode_response(&json!({
                "status": "completed",
                "output": [{"type": "web_search_call", "id": "ws_1"}]
            })),
            Err(ResponseDecodeError::Malformed(_))
        ));
    }

    #[test]
    fn function_calls_and_reasoning_are_extracted_in_order() {
        let call = json!({
            "type": "function_call",
            "name": "write_file",
            "arguments": "{\"path\":\"a\"}",
            "call_id": "call_1",
        });
        let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": []});
        let body = json!({
            "status": "completed",
            "output": [reasoning.clone(), call.clone()],
        });
        let d = decode_response(&body).expect("tool-call turn is a success");
        // tool_calls is just the function_call; echo preserves reasoning+call
        // in output order so the loop can replay them verbatim.
        assert_eq!(d.tool_calls, vec![call.clone()]);
        assert_eq!(d.echo, vec![reasoning, call]);
        assert_eq!(d.text, "");
    }

    #[test]
    fn chat_completions_usage_names_are_accepted() {
        let d = decode_response(&json!({
            "status": "completed",
            "output_text": "hi",
            "usage": {"prompt_tokens": 3, "completion_tokens": 5},
        }))
        .expect("success");
        let u = d.usage.expect("usage");
        assert_eq!((u.input_tokens, u.output_tokens), (3, 5));
    }
}
