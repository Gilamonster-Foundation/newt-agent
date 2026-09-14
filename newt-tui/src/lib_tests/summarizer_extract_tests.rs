use super::extract_summary;

#[test]
fn strips_inline_think_and_flags_thinking_only() {
    // Inline <think> is stripped → clean summary (Ollama shape).
    let j = serde_json::json!({"message": {"content": "<think>let me reason</think>Active task: X. Done."}});
    assert_eq!(
        extract_summary(&j, false).unwrap().0,
        "Active task: X. Done."
    );
    // OpenAI shape.
    let o =
        serde_json::json!({"choices": [{"message": {"content": "<think>hmm</think>Summary."}}]});
    assert_eq!(extract_summary(&o, true).unwrap().0, "Summary.");
    // Thinking-only reply (empty content, reasoning in a separate field) →
    // Err, so the caller degrades to the static marker instead of treating
    // an empty string as a valid summary (silent context loss).
    let empty =
        serde_json::json!({"message": {"content": "", "thinking": "all reasoning, no text"}});
    assert!(extract_summary(&empty, false).is_err());
}

/// #2313: a compaction summary keeps the usage its backend reported, on both
/// wire shapes; a reply without counts stays `None` (unknown, never zero).
#[test]
fn summary_reply_keeps_backend_reported_usage() {
    let usage = |input_tokens, output_tokens| {
        Some(newt_core::TokenUsage {
            input_tokens,
            output_tokens,
        })
    };
    let ollama = serde_json::json!({"message": {"content": "S."},
        "prompt_eval_count": 900, "eval_count": 40});
    assert_eq!(extract_summary(&ollama, false).unwrap().1, usage(900, 40));
    let openai = serde_json::json!({"choices": [{"message": {"content": "S."}}],
        "usage": {"prompt_tokens": 800, "completion_tokens": 30}});
    assert_eq!(extract_summary(&openai, true).unwrap().1, usage(800, 30));
    let unreported = serde_json::json!({"message": {"content": "S."}});
    assert_eq!(extract_summary(&unreported, false).unwrap().1, None);
}
