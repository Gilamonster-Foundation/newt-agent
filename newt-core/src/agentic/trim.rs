//! Context-window management for the agentic loop: token estimation,
//! mid-loop trimming, pre-send budget enforcement, and the tool-call/result
//! pairing repair that keeps strict backends (Anthropic/Bedrock via LiteLLM)
//! from rejecting a trimmed history. Moved verbatim from `newt-tui` (Step 9.7).

/// Trim a message list for the cap-exit summary: keep the first `head` messages
/// (system prompt + original task) and the last `tail` messages (recent rounds).
/// Inserts a single placeholder when the middle is dropped so the model knows
/// context was omitted rather than assuming the task was simpler than it is.
pub(crate) fn trim_for_summary(
    messages: &[serde_json::Value],
    head: usize,
    tail: usize,
) -> Vec<serde_json::Value> {
    if messages.len() <= head + tail {
        return messages.to_vec();
    }
    let dropped = messages.len() - head - tail;
    let mut result = Vec::with_capacity(head + 1 + tail);
    result.extend_from_slice(&messages[..head]);
    result.push(serde_json::json!({
        "role": "user",
        "content": format!(
            "[{dropped} earlier tool-call messages omitted to keep context within model limits]"
        ),
    }));
    result.extend_from_slice(&messages[messages.len() - tail..]);
    // Anthropic/Bedrock requires every tool_use block to be followed by its
    // tool_result. Trimming can orphan tool_calls — remove them so strict
    // backends don't reject the whole request with 400 Bad Request.
    repair_orphaned_tool_calls(&mut result);
    result
}

/// Estimate the input token count of a serialized message list.
///
/// Uses the standard `chars / 4` heuristic over the JSON serialization of each
/// message. This is deliberately cheap (no tokenizer) and runs before every
/// dispatch, so the cost must stay negligible even for large histories. The
/// estimate only needs to be good enough to fire trimming *before* a request
/// would blow past the model's context window — see [`trim_to_token_budget`]
/// and issue #223.
pub(crate) fn estimate_tokens(messages: &[serde_json::Value]) -> usize {
    messages
        .iter()
        .map(|m| m.to_string().chars().count())
        .sum::<usize>()
        / 4
}

/// Trim `messages` until the estimated token count fits within `budget`,
/// returning the (possibly unchanged) list. Trimming is progressive: it keeps
/// the system + first `head` messages and shrinks the retained tail, halving it
/// each pass until the estimate fits or the tail can shrink no further.
///
/// `head` is the number of leading messages to always preserve (system prompt
/// plus the original task). Returns `(trimmed, fired)` where `fired` is `true`
/// if any messages were dropped — the caller uses it to decide whether to emit
/// a notice. See issue #223.
pub(crate) fn trim_to_token_budget(
    messages: &[serde_json::Value],
    budget: usize,
    head: usize,
) -> (Vec<serde_json::Value>, bool) {
    if budget == 0 || estimate_tokens(messages) <= budget {
        return (messages.to_vec(), false);
    }
    let mut tail = messages.len().saturating_sub(head) / 2;
    loop {
        let candidate = trim_for_summary(messages, head, tail);
        if estimate_tokens(&candidate) <= budget || tail == 0 {
            return (candidate, true);
        }
        tail /= 2;
    }
}

/// Mid-loop trim trigger that fires on EITHER message count OR estimated tokens.
///
/// The message-count threshold (`count_threshold`) is the original VRAM guard.
/// `token_threshold` is the issue #223 addition: a single tool round can return
/// a multi-KB payload that stays well under the message-count threshold while
/// blowing past the model's context window. When set and exceeded, the list is
/// further trimmed to fit the token budget. Returns `(trimmed, fired)`.
pub(crate) fn mid_loop_trim(
    messages: &[serde_json::Value],
    count_threshold: usize,
    token_threshold: Option<usize>,
) -> (Vec<serde_json::Value>, bool) {
    let mut out = messages.to_vec();
    let mut fired = false;
    if out.len() > count_threshold {
        out = trim_for_summary(&out, 2, count_threshold / 2);
        fired = true;
    }
    if let Some(budget) = token_threshold {
        if estimate_tokens(&out) > budget {
            let (trimmed, t) = trim_to_token_budget(&out, budget, 2);
            out = trimmed;
            fired = fired || t;
        }
    }
    (out, fired)
}

/// Remove or neutralise tool-call/result messages that form an incomplete pair
/// after `trim_for_summary` cuts the middle of a conversation.
///
/// Two failure modes that Anthropic/Bedrock reject with 400:
///
/// 1. **Partial results** — an assistant message has `tool_calls: [tc1, tc2]` but
///    only `tc1`'s `role="tool"` result survived trimming.  LiteLLM converts
///    *both* IDs to Bedrock `tool_use` blocks; Bedrock then complains that
///    `tc2` has no matching `tool_result`.  The previous check (`next message
///    is role="tool"`) was not sufficient — it didn't verify every ID.
///
/// 2. **Orphaned results** — a `role="tool"` message lands at the start of the
///    tail with no preceding assistant `tool_calls` (its assistant turn was
///    dropped).  Some LiteLLM/Bedrock versions reject unmatched results too.
///
/// Strategy:
///   Pass 1 — for each assistant with `tool_calls`, verify every ID has a
///             `role="tool"` result anywhere in the list; if any are missing,
///             strip **all** `tool_calls` from that assistant turn.
///   Pass 2 — remove every `role="tool"` message whose `tool_call_id` is not
///             referenced by any remaining assistant `tool_calls`.
pub(crate) fn repair_orphaned_tool_calls(messages: &mut Vec<serde_json::Value>) {
    // Build the set of tool_call IDs for which a role="tool" result exists.
    let result_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .filter_map(|m| m["tool_call_id"].as_str().map(|s| s.to_string()))
        .collect();

    // Pass 1: determine which assistant messages need their tool_calls stripped,
    // then apply the changes in a second pass to avoid conflicting borrows.
    let roles: Vec<Option<String>> = messages
        .iter()
        .map(|m| m["role"].as_str().map(|s| s.to_string()))
        .collect();

    let strip_indices: std::collections::HashSet<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, msg)| {
            if msg["role"].as_str() != Some("assistant") {
                return None;
            }
            let tool_calls = msg["tool_calls"].as_array()?;
            if tool_calls.is_empty() {
                return None;
            }
            let ids: Vec<String> = tool_calls
                .iter()
                .filter_map(|tc| tc["id"].as_str().map(|s| s.to_string()))
                .collect();
            let should_strip = if ids.is_empty() {
                // No IDs: fall back to positional check.
                roles.get(i + 1).and_then(|r| r.as_deref()) != Some("tool")
            } else {
                !ids.iter().all(|id| result_ids.contains(id))
            };
            should_strip.then_some(i)
        })
        .collect();

    for i in strip_indices {
        if let Some(obj) = messages[i].as_object_mut() {
            obj.remove("tool_calls");
            obj.entry("content")
                .or_insert_with(|| serde_json::json!("[tool calls omitted]"));
        }
    }

    // Pass 2: remove role="tool" messages with no matching assistant tool_calls.
    let live_call_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("assistant"))
        .filter_map(|m| m["tool_calls"].as_array())
        .flat_map(|tc| tc.iter())
        .filter_map(|tc| tc["id"].as_str().map(|s| s.to_string()))
        .collect();

    messages.retain(|m| {
        if m["role"].as_str() != Some("tool") {
            return true;
        }
        // Keep tool results with no ID (malformed but harmless).
        // Only drop results whose explicit ID has no matching live tool_call.
        match m["tool_call_id"].as_str() {
            Some(id) if !id.is_empty() => live_call_ids.contains(id),
            _ => true,
        }
    });
}

/// Merge two optional token usage readings (e.g. accumulated across rounds).
pub(crate) fn merge_usage(
    acc: Option<crate::TokenUsage>,
    new: Option<crate::TokenUsage>,
) -> Option<crate::TokenUsage> {
    match (acc, new) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// Extract token usage from an Ollama non-streaming response (top-level
/// `prompt_eval_count` / `eval_count` fields).
pub(crate) fn ollama_usage(json: &serde_json::Value) -> Option<crate::TokenUsage> {
    let input = json["prompt_eval_count"].as_u64()? as u32;
    let output = json["eval_count"].as_u64()? as u32;
    Some(crate::TokenUsage {
        input_tokens: input,
        output_tokens: output,
    })
}

/// Parse an OpenAI `usage` object (`prompt_tokens` / `completion_tokens`).
pub(crate) fn openai_usage(usage: &serde_json::Value) -> Option<crate::TokenUsage> {
    let input = usage["prompt_tokens"].as_u64()? as u32;
    let output = usage["completion_tokens"].as_u64()? as u32;
    Some(crate::TokenUsage {
        input_tokens: input,
        output_tokens: output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `trim_for_summary` keeps head + tail and inserts a placeholder for
    /// the dropped middle section.
    #[test]
    fn trim_for_summary_drops_middle_and_inserts_placeholder() {
        let msgs: Vec<serde_json::Value> = (0..10)
            .map(|i| serde_json::json!({"role": "user", "content": format!("msg {i}")}))
            .collect();

        let trimmed = trim_for_summary(&msgs, 2, 3);
        // head(2) + placeholder(1) + tail(3) = 6
        assert_eq!(
            trimmed.len(),
            6,
            "expected 6 messages, got {}",
            trimmed.len()
        );
        // First two are the original head
        assert_eq!(trimmed[0]["content"], "msg 0");
        assert_eq!(trimmed[1]["content"], "msg 1");
        // Placeholder in the middle
        let placeholder = trimmed[2]["content"].as_str().unwrap();
        assert!(
            placeholder.contains("omitted"),
            "placeholder must mention omitted messages: {placeholder}"
        );
        // Last three are the original tail
        assert_eq!(trimmed[3]["content"], "msg 7");
        assert_eq!(trimmed[4]["content"], "msg 8");
        assert_eq!(trimmed[5]["content"], "msg 9");
    }

    #[test]
    fn trim_for_summary_passthrough_when_short_enough() {
        let msgs: Vec<serde_json::Value> = (0..4)
            .map(|i| serde_json::json!({"role": "user", "content": format!("msg {i}")}))
            .collect();
        // head=2, tail=3 → total=5, msgs.len()=4 → no trimming needed
        let trimmed = trim_for_summary(&msgs, 2, 3);
        assert_eq!(trimmed.len(), 4);
    }

    /// `estimate_tokens` uses the chars/4 heuristic over serialized messages.
    #[test]
    fn estimate_tokens_scales_with_content_size() {
        let small = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let big = vec![serde_json::json!({"role": "user", "content": "x".repeat(4000)})];
        let s = estimate_tokens(&small);
        let b = estimate_tokens(&big);
        // ~4000 chars / 4 ≈ 1000 tokens for the big message.
        assert!(b >= 900, "big message should estimate ~1000 tokens, got {b}");
        assert!(b > s * 10, "big must dwarf small ({b} vs {s})");
    }

    /// The crux of issue #223: a SINGLE huge tool message stays well under the
    /// message-count threshold yet must still trigger a trim by token budget.
    #[test]
    fn token_trim_fires_on_one_huge_message_under_count_threshold() {
        let mut msgs = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "task"}),
        ];
        // One tool round returns a ~1M-char payload → ~250k tokens.
        msgs.push(serde_json::json!({"role": "tool", "content": "z".repeat(1_000_000)}));
        msgs.push(serde_json::json!({"role": "user", "content": "next"}));

        // Message count (4) is far below the threshold (40), so the old
        // count-only trigger would NOT fire. The token trigger must.
        let (out, fired) = mid_loop_trim(&msgs, 40, Some(50_000));
        assert!(fired, "token-based trim must fire on the huge payload");
        assert!(
            estimate_tokens(&out) <= 50_000,
            "trim must bring estimate under budget, got {}",
            estimate_tokens(&out)
        );
    }

    /// With no token threshold configured, behaviour matches the legacy
    /// count-only trigger (no trim while under the message count).
    #[test]
    fn mid_loop_trim_count_only_when_no_token_threshold() {
        let msgs: Vec<serde_json::Value> = (0..5)
            .map(|i| serde_json::json!({"role": "user", "content": format!("m{i}")}))
            .collect();
        let (out, fired) = mid_loop_trim(&msgs, 40, None);
        assert!(!fired);
        assert_eq!(out.len(), 5);
    }

    /// `trim_to_token_budget` shrinks an over-budget list and leaves a small
    /// one untouched.
    #[test]
    fn trim_to_token_budget_respects_budget() {
        let mut msgs = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "task"}),
        ];
        for i in 0..20 {
            msgs.push(
                serde_json::json!({"role": "tool", "content": "q".repeat(5_000) + &i.to_string()}),
            );
        }
        let budget = 2_000; // tokens
        let (trimmed, fired) = trim_to_token_budget(&msgs, budget, 2);
        assert!(fired);
        assert!(estimate_tokens(&trimmed) <= budget);

        // A list already under budget is returned untouched.
        let small = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let (out, fired2) = trim_to_token_budget(&small, 10_000, 2);
        assert!(!fired2);
        assert_eq!(out.len(), 1);
    }

    /// A zero budget disables the guard (no panic, no trim).
    #[test]
    fn trim_to_token_budget_zero_is_noop() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "x".repeat(99_999)})];
        let (out, fired) = trim_to_token_budget(&msgs, 0, 2);
        assert!(!fired);
        assert_eq!(out.len(), 1);
    }

    /// A complete tool_calls + tool_result pair is left untouched.
    #[test]
    fn repair_leaves_matched_tool_calls_intact() {
        let mut msgs = vec![
            serde_json::json!({"role": "user", "content": "do it"}),
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"function": {"name": "list_dir", "arguments": {}}}]
            }),
            serde_json::json!({"role": "tool", "content": "file.rs"}),
        ];
        repair_orphaned_tool_calls(&mut msgs);
        // The assistant message must still have tool_calls.
        assert!(
            msgs[1]["tool_calls"].as_array().is_some(),
            "matched tool_calls must be preserved"
        );
    }

    /// An assistant message whose tool_calls have no following tool result
    /// gets tool_calls stripped — Anthropic/Bedrock would 400 otherwise.
    #[test]
    fn repair_strips_orphaned_tool_calls() {
        let mut msgs = vec![
            serde_json::json!({"role": "user", "content": "first"}),
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"function": {"name": "list_dir", "arguments": {}}}]
            }),
            // Placeholder from trim — NOT a tool result.
            serde_json::json!({"role": "user", "content": "[context omitted]"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        repair_orphaned_tool_calls(&mut msgs);
        assert!(
            msgs[1].get("tool_calls").is_none(),
            "orphaned tool_calls must be stripped"
        );
        // Content should be preserved or a placeholder injected.
        assert!(
            msgs[1]["content"].as_str().is_some(),
            "assistant message must still have content after stripping tool_calls"
        );
    }

    /// trim_for_summary followed by repair produces no orphaned tool_calls,
    /// matching the Bedrock/Anthropic requirement.
    #[test]
    fn trim_then_repair_produces_no_orphans() {
        // Build a conversation: user → (assistant+tool_calls → tool_result) × 5
        let mut msgs = vec![serde_json::json!({"role": "user", "content": "task"})];
        for i in 0..5u32 {
            msgs.push(serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": format!("call_{i}"), "function": {"name": "list_dir", "arguments": {}}}]
            }));
            msgs.push(serde_json::json!({"role": "tool", "tool_call_id": format!("call_{i}"), "content": "result"}));
        }
        // Trim aggressively (head=1, tail=2) — cuts through tool pairs.
        let trimmed = trim_for_summary(&msgs, 1, 2);
        // After trim+repair, every remaining tool_calls must have ALL its IDs
        // covered by a role="tool" result present somewhere in the list.
        let result_ids: std::collections::HashSet<String> = trimmed
            .iter()
            .filter(|m| m["role"].as_str() == Some("tool"))
            .filter_map(|m| m["tool_call_id"].as_str().map(|s| s.to_string()))
            .collect();
        for msg in &trimmed {
            if msg["role"].as_str() == Some("assistant") {
                if let Some(tc) = msg["tool_calls"].as_array() {
                    for call in tc {
                        let id = call["id"].as_str().unwrap_or("");
                        assert!(
                            result_ids.contains(id),
                            "after trim+repair, tool_call id={id:?} has no matching tool result"
                        );
                    }
                }
            }
        }
    }

    /// Regression: assistant with TWO tool_calls where only the first result
    /// survives trimming must have ALL tool_calls stripped (not just partially).
    /// The old code checked only "next message is role=tool" — this was enough
    /// for single-call rounds but missed the second ID in a multi-call round,
    /// causing Bedrock to return 400 "Expected toolResult blocks".
    #[test]
    fn repair_strips_partial_tool_call_results() {
        // Simulate trim output: assistant called tc_a + tc_b but only tc_a's
        // result survived — tc_b was dropped in the middle.
        let mut msgs = vec![
            serde_json::json!({"role": "user", "content": "task"}),
            serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": "tc_a", "function": {"name": "read_file", "arguments": {}}},
                    {"id": "tc_b", "function": {"name": "list_dir",  "arguments": {}}}
                ]
            }),
            // Only tc_a's result is present; tc_b's was trimmed.
            serde_json::json!({"role": "tool", "tool_call_id": "tc_a", "content": "file content"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        repair_orphaned_tool_calls(&mut msgs);
        // The incomplete assistant must have tool_calls stripped.
        assert!(
            msgs[1].get("tool_calls").is_none(),
            "partial tool_calls (tc_b missing) must be stripped"
        );
        // The now-orphaned tc_a result must also be removed.
        let has_orphaned_result = msgs.iter().any(|m| {
            m["role"].as_str() == Some("tool") && m["tool_call_id"].as_str() == Some("tc_a")
        });
        assert!(
            !has_orphaned_result,
            "tool_result for stripped tool_call must be removed"
        );
    }

    /// Regression: orphaned role="tool" at the start of the tail (its assistant
    /// was dropped by trimming) must be removed.
    #[test]
    fn repair_removes_orphaned_tool_result() {
        let mut msgs = vec![
            serde_json::json!({"role": "user",      "content": "task"}),
            serde_json::json!({"role": "user",      "content": "[N messages omitted]"}),
            // tc_old's assistant was dropped — this result is now orphaned.
            serde_json::json!({"role": "tool", "tool_call_id": "tc_old", "content": "stale"}),
            serde_json::json!({"role": "assistant", "content": "done"}),
        ];
        repair_orphaned_tool_calls(&mut msgs);
        let has_orphan = msgs.iter().any(|m| {
            m["role"].as_str() == Some("tool") && m["tool_call_id"].as_str() == Some("tc_old")
        });
        assert!(
            !has_orphan,
            "orphaned tool_result with no matching assistant must be removed"
        );
    }

    #[test]
    fn merge_usage_accumulates_or_passes_through() {
        let a = crate::TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
        };
        let b = crate::TokenUsage {
            input_tokens: 5,
            output_tokens: 1,
        };
        let merged = merge_usage(Some(a), Some(b)).unwrap();
        assert_eq!(merged.input_tokens, 15);
        assert_eq!(merged.output_tokens, 3);
        assert_eq!(merge_usage(Some(a), None).unwrap().input_tokens, 10);
        assert_eq!(merge_usage(None, Some(b)).unwrap().output_tokens, 1);
        assert!(merge_usage(None, None).is_none());
    }

    #[test]
    fn ollama_usage_parses_or_none() {
        let u = ollama_usage(&serde_json::json!({
            "prompt_eval_count": 7, "eval_count": 3
        }))
        .unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 3);
        assert!(ollama_usage(&serde_json::json!({"prompt_eval_count": 7})).is_none());
        assert!(ollama_usage(&serde_json::json!({})).is_none());
    }

    #[test]
    fn openai_usage_parses_or_none() {
        let u = openai_usage(&json!({"prompt_tokens": 12, "completion_tokens": 34})).unwrap();
        assert_eq!(u.input_tokens, 12);
        assert_eq!(u.output_tokens, 34);
        // Missing either field → None (no partial/garbage usage).
        assert!(openai_usage(&json!({"prompt_tokens": 12})).is_none());
        assert!(openai_usage(&json!({})).is_none());
    }
}
