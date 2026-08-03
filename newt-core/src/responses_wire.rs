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
//! *turn* finished lives in the body's `status` field. [`decode_response`]
//! surfaces that as a [`Completion`] so no consumer can mistake an `incomplete`
//! (the model hit `max_output_tokens`) or `failed` body for success — the defect
//! that let a truncated/failed turn read as an empty-but-fine reply.

use serde_json::Value;

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

/// Whether a Responses turn actually completed. Only [`Completed`](Self::Completed)
/// is a success; every other variant is a `2xx` body that did **not** finish and
/// must not be treated as a valid reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// `status == "completed"`, or a lenient server that omits `status` entirely
    /// while returning output.
    Completed,
    /// `status == "incomplete"` — the model stopped early. `reason` mirrors
    /// `incomplete_details.reason` (e.g. `max_output_tokens`, `content_filter`).
    Incomplete { reason: Option<String> },
    /// `status == "failed"` — a turn-level error, message drawn from `error`.
    Failed { message: String },
    /// `status` present but not a recognized terminal value (`in_progress`,
    /// `cancelled`, or anything unknown). Treated as non-success.
    Other { status: String },
}

impl Completion {
    /// `true` only for [`Completed`](Self::Completed).
    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
    }
}

/// A typed view of a decoded Responses payload. The `output`/`echo` items stay as
/// raw [`Value`]s because the agentic loop echoes them back **verbatim** in the
/// next request's `input` (the Responses API requires the exact `function_call`
/// and preceding `reasoning` items replayed).
#[derive(Debug, Clone)]
pub struct DecodedResponse {
    /// Did the turn complete? (The 2xx-≠-completed verdict.)
    pub completion: Completion,
    /// Concatenated assistant text: every `message` item's content-part `text`,
    /// with a flat top-level `output_text` fallback.
    pub text: String,
    /// Raw `function_call` output items, in output order — the tool calls the
    /// model requested.
    pub tool_calls: Vec<Value>,
    /// Raw items to ECHO back alongside the tool calls, in output order: every
    /// `function_call` AND the `reasoning` items that precede them (the Responses
    /// API requires the reasoning chain replayed with its call).
    pub echo: Vec<Value>,
    /// The model id the server reports (`model`), if present.
    pub model: Option<String>,
    /// Token usage (`input_tokens`/`output_tokens`), if present.
    pub usage: Option<crate::TokenUsage>,
}

/// Decode a Responses-API JSON body into a typed [`DecodedResponse`].
///
/// This never fails to *parse*: a malformed body yields empty text/calls with a
/// best-effort [`Completion`]. The completion verdict is where "2xx ≠ completed"
/// is enforced — callers MUST inspect [`DecodedResponse::completion`] (via
/// [`Completion::is_completed`] or a `match`) rather than assuming a 200 body is
/// a finished turn.
#[must_use]
pub fn decode_response(json: &Value) -> DecodedResponse {
    let completion = decode_completion(json);
    let (text, tool_calls, echo) = decode_output(json);
    let model = json["model"].as_str().map(str::to_string);
    let usage = decode_usage(&json["usage"]);
    DecodedResponse {
        completion,
        text,
        tool_calls,
        echo,
        model,
        usage,
    }
}

fn decode_completion(json: &Value) -> Completion {
    match json["status"].as_str() {
        // A lenient/older server may omit `status` while returning output; treat
        // that as completed (the historical behaviour of both old parsers).
        None | Some("completed") => Completion::Completed,
        Some("incomplete") => Completion::Incomplete {
            reason: json["incomplete_details"]["reason"]
                .as_str()
                .map(str::to_string),
        },
        Some("failed") => {
            let message = json["error"]["message"]
                .as_str()
                .or_else(|| json["error"].as_str())
                .unwrap_or("Responses turn reported status \"failed\" with no error detail")
                .to_string();
            Completion::Failed { message }
        }
        Some(other) => Completion::Other {
            status: other.to_string(),
        },
    }
}

fn decode_output(json: &Value) -> (String, Vec<Value>, Vec<Value>) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut echo = Vec::new();
    if let Some(items) = json["output"].as_array() {
        for item in items {
            match item["type"].as_str() {
                Some("message") => {
                    if let Some(parts) = item["content"].as_array() {
                        for p in parts {
                            // Within a `message` item only the `output_text` part
                            // carries a `text` field; pulling `text` from any part
                            // gets it and skips a `refusal` part (which has none).
                            if let Some(t) = p["text"].as_str() {
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
    (text, tool_calls, echo)
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
        let d = decode_response(&body);
        assert_eq!(d.completion, Completion::Completed);
        assert!(d.completion.is_completed());
        assert_eq!(d.text, "the answer");
        assert_eq!(d.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(d.usage.unwrap().input_tokens, 11);
        assert!(d.tool_calls.is_empty());
    }

    #[test]
    fn flat_output_text_is_the_fallback() {
        let d = decode_response(&json!({"output_text": "ok"}));
        assert_eq!(d.text, "ok");
        // A missing `status` is a lenient-server completed turn.
        assert_eq!(d.completion, Completion::Completed);
    }

    #[test]
    fn incomplete_status_is_not_completed_and_carries_the_reason() {
        // A 200 body whose turn hit the output cap. Invariant: NOT a success.
        let body = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "partial…"}],
            }],
        });
        let d = decode_response(&body);
        assert!(!d.completion.is_completed());
        assert_eq!(
            d.completion,
            Completion::Incomplete {
                reason: Some("max_output_tokens".into())
            }
        );
        // The partial text is still decoded, but the caller MUST NOT treat the
        // turn as complete — the `completion` verdict is what gates that.
        assert_eq!(d.text, "partial…");
    }

    #[test]
    fn failed_status_carries_the_error_message() {
        let body = json!({
            "status": "failed",
            "error": {"message": "the model burst into flames"},
        });
        let d = decode_response(&body);
        assert_eq!(
            d.completion,
            Completion::Failed {
                message: "the model burst into flames".into()
            }
        );
        assert!(!d.completion.is_completed());
    }

    #[test]
    fn failed_status_without_error_detail_still_reports_failed() {
        let d = decode_response(&json!({"status": "failed"}));
        assert!(matches!(d.completion, Completion::Failed { .. }));
    }

    #[test]
    fn unknown_status_is_other_not_completed() {
        let d = decode_response(&json!({"status": "in_progress"}));
        assert_eq!(
            d.completion,
            Completion::Other {
                status: "in_progress".into()
            }
        );
        assert!(!d.completion.is_completed());
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
        let d = decode_response(&body);
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
            "usage": {"prompt_tokens": 3, "completion_tokens": 5},
        }));
        let u = d.usage.expect("usage");
        assert_eq!((u.input_tokens, u.output_tokens), (3, 5));
    }
}
