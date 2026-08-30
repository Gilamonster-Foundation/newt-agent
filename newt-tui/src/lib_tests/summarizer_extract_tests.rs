use super::extract_summary;

#[test]
fn strips_inline_think_and_flags_thinking_only() {
    // Inline <think> is stripped → clean summary (Ollama shape).
    let j = serde_json::json!({"message": {"content": "<think>let me reason</think>Active task: X. Done."}});
    assert_eq!(extract_summary(&j, false).unwrap(), "Active task: X. Done.");
    // OpenAI shape.
    let o =
        serde_json::json!({"choices": [{"message": {"content": "<think>hmm</think>Summary."}}]});
    assert_eq!(extract_summary(&o, true).unwrap(), "Summary.");
    // Thinking-only reply (empty content, reasoning in a separate field) →
    // Err, so the caller degrades to the static marker instead of treating
    // an empty string as a valid summary (silent context loss).
    let empty =
        serde_json::json!({"message": {"content": "", "thinking": "all reasoning, no text"}});
    assert!(extract_summary(&empty, false).is_err());
}
