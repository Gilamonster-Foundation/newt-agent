//! Context-window management for the agentic loop: token estimation, the
//! cap-exit trim, and the tool-call/result pairing repair that keeps strict
//! backends (Anthropic/Bedrock via LiteLLM) from rejecting a rewritten
//! history. Moved verbatim from `newt-tui` (Step 9.7).
//!
//! Step 18.4 (#247): the mid-loop trim and pre-send budget enforcement that
//! used to live here (`mid_loop_trim` / `trim_to_token_budget` — the
//! amputate-the-middle helpers measured failing in baseline B6) are replaced
//! by the [`super::compress`] pipeline. `trim_for_summary` survives for the
//! cap-exit summary request only.

use crate::tokens::TokenEstimation;

/// Return the compression-immune head length for a transcript carrying a
/// harness-owned active-prompt card.
///
/// Every leading system message is protected. When the final leading system
/// message is the active-prompt card, its immediately following user message
/// is protected as well. Keeping this calculation in one place prevents the
/// normal compressor and the round-cap summarizer from disagreeing about the
/// card/prompt adjacency contract.
pub(crate) fn protected_prompt_head_len(
    messages: &[serde_json::Value],
    active_prompt_prefix: &str,
) -> usize {
    let mut head = messages
        .iter()
        .take_while(|message| message["role"].as_str() == Some("system"))
        .count();
    if head > 0
        && messages[head - 1]["content"]
            .as_str()
            .is_some_and(|content| content.starts_with(active_prompt_prefix))
        && messages
            .get(head)
            .and_then(|message| message["role"].as_str())
            == Some("user")
    {
        head += 1;
    }
    head
}

/// Trim a message list for the cap-exit summary: keep the first `head` messages
/// (system prompt + original task) and the last `tail` messages (recent rounds).
/// Inserts a single placeholder when the middle is dropped so the model knows
/// context was omitted rather than assuming the task was simpler than it is.
///
/// `pub` since Step 19.4 (#248): the TUI's close-time note extraction bounds
/// its transcript input with the SAME helper the cap-exit summary uses, so
/// neither tools-disabled request ever ships an unbounded history.
pub fn trim_for_summary(
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

/// Ceiling-divide token estimate of one serialized JSON value under the
/// configured [`TokenEstimation`] heuristic.
///
/// Ceiling (not floor) so a 1-char fragment never estimates to zero tokens —
/// the fallback must err on the side of counting, not undercounting (18.1).
pub(crate) fn estimate_value_tokens(v: &serde_json::Value, est: TokenEstimation) -> usize {
    est.tokens_for_chars(v.to_string().chars().count())
}

/// Estimate the input token count of a serialized message list.
///
/// Uses the standard chars/4 heuristic (ceiling-divide per message) over the
/// JSON serialization of each message. This is deliberately cheap (no
/// tokenizer) and runs before every dispatch, so the cost must stay negligible
/// even for large histories. It is the **fallback** figure only: when the
/// backend has reported a prompt token count for the current conversation,
/// [`PromptTracker::current`] anchors on that instead (Step 18.1). The
/// estimate only needs to be good enough to fire compression *before* a
/// request would blow past the model's context window — see
/// [`super::compress`] and issue #223.
pub(crate) fn estimate_tokens(messages: &[serde_json::Value], est: TokenEstimation) -> usize {
    messages.iter().map(|m| estimate_value_tokens(m, est)).sum()
}

/// Estimate the token count of a full request: messages **plus** the
/// serialized tool definitions, which ride along in the request body on every
/// call and were previously uncounted (~705–1,065 tokens/request measured in
/// the B3 baseline — `docs/testing/results/context-baseline-f0f4f6e.md`).
/// `tools` is `None` for tools-disabled requests (e.g. the cap-exit summary).
pub(crate) fn estimate_request_tokens(
    messages: &[serde_json::Value],
    tools: Option<&serde_json::Value>,
    est: TokenEstimation,
) -> usize {
    estimate_tokens(messages, est) + tools.map(|t| estimate_value_tokens(t, est)).unwrap_or(0)
}

/// Tracks the truthful "how full is the context" figure across the rounds of
/// one agentic turn (Step 18.1).
///
/// Prefers the backend's last-reported prompt token count (Ollama
/// `prompt_eval_count` / OpenAI `usage.prompt_tokens`): the report covers the
/// *entire* request — system prompt, history, tool schemas, chat template —
/// with zero estimation error. Messages appended since that report (assistant
/// tool-call turns and tool results) are added with the chars/4 estimate.
/// Falls back to [`estimate_request_tokens`] when no report exists yet or a
/// trim has rewritten the message list out from under the anchor.
pub(crate) struct PromptTracker {
    /// `(reported prompt tokens, messages.len() at that dispatch)`.
    anchor: Option<(u32, usize)>,
}

impl PromptTracker {
    pub(crate) fn new() -> Self {
        Self { anchor: None }
    }

    /// Record the backend-reported prompt size of a dispatched request and the
    /// number of messages that request contained.
    pub(crate) fn record(&mut self, prompt_tokens: u32, message_count: usize) {
        self.anchor = Some((prompt_tokens, message_count));
    }

    /// Drop the anchor. Must be called whenever the message list is rewritten
    /// non-append-only (any trim), because the anchored prefix no longer
    /// matches what the backend evaluated.
    pub(crate) fn invalidate(&mut self) {
        self.anchor = None;
    }

    /// Current context size in REAL tokens: backend-reported prompt tokens
    /// plus an estimate of messages appended since, or
    /// [`estimate_request_tokens`] (messages + serialized tool schemas) when
    /// no valid anchor exists. The schemas are NOT added on the anchored
    /// path — the backend's report already includes them.
    ///
    /// `ratio` is the per-model estimate calibration (Phase 20,
    /// `docs/design/model-self-tuning.md` §2.3): every chars/4 leg is scaled
    /// estimate→real so the figure compares honestly against backend-derived
    /// budgets. The anchored backend report is already real tokens and is
    /// never rescaled. At `ratio == 1.0` behavior is identical to the
    /// pre-Phase-20 contract.
    pub(crate) fn current(
        &self,
        messages: &[serde_json::Value],
        tools: Option<&serde_json::Value>,
        ratio: f32,
        est: TokenEstimation,
    ) -> usize {
        match self.anchor {
            Some((tokens, count)) if count <= messages.len() => {
                tokens as usize
                    + super::calibrate_up(estimate_tokens(&messages[count..], est), ratio)
            }
            _ => super::calibrate_up(estimate_request_tokens(messages, tools, est), ratio),
        }
    }
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

/// Merge token usage readings across the rounds of ONE agentic turn.
///
/// **Input is the max, not the sum** (Step 18.1): every round's prompt
/// re-includes the entire history, so summing `prompt_eval_count` across
/// rounds double-counts — the B3 baseline measured a turn tracked at 22,451
/// input tokens whose largest real prompt was 4,136 (5.4×), and that inflated
/// figure was ratcheted into `model-capabilities.json` as a proven-safe
/// `max_ok_input`. The turn-level input is the largest single prompt the
/// backend evaluated — the truthful "how full did the context get" figure.
/// **Output sums**: each round's completion tokens are genuinely new.
pub(crate) fn merge_round_usage(
    acc: Option<crate::TokenUsage>,
    new: Option<crate::TokenUsage>,
) -> Option<crate::TokenUsage> {
    match (acc, new) {
        (Some(a), Some(b)) => Some(crate::TokenUsage {
            input_tokens: a.input_tokens.max(b.input_tokens),
            output_tokens: a.output_tokens.saturating_add(b.output_tokens),
        }),
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

    /// Default estimation (chars_per_token = 4) for the unit tests.
    const EST: TokenEstimation = TokenEstimation { chars_per_token: 4 };
    use serde_json::json;

    #[test]
    fn protected_head_keeps_active_card_and_exact_prompt_adjacent() {
        let prefix = "[ACTIVE]";
        let messages = vec![
            json!({"role": "system", "content": "base"}),
            json!({"role": "system", "content": "[ACTIVE]\naddress: prompt:1"}),
            json!({"role": "user", "content": "exact operator prompt"}),
            json!({"role": "user", "content": "historical message"}),
        ];
        assert_eq!(protected_prompt_head_len(&messages, prefix), 3);
        let trimmed = trim_for_summary(&messages, protected_prompt_head_len(&messages, prefix), 0);
        assert_eq!(trimmed[1], messages[1]);
        assert_eq!(trimmed[2], messages[2]);
    }

    #[test]
    fn protected_head_never_promotes_arbitrary_first_user() {
        let messages = vec![
            json!({"role": "system", "content": "base"}),
            json!({"role": "user", "content": "old ask"}),
        ];
        assert_eq!(protected_prompt_head_len(&messages, "[ACTIVE]"), 1);
    }

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
        let s = estimate_tokens(&small, EST);
        let b = estimate_tokens(&big, EST);
        // ~4000 chars / 4 ≈ 1000 tokens for the big message.
        assert!(
            b >= 900,
            "big message should estimate ~1000 tokens, got {b}"
        );
        assert!(b > s * 10, "big must dwarf small ({b} vs {s})");
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

    /// SEMANTICS CHANGED in Step 18.1: turn-level input is the max single
    /// prompt across rounds (each round's prompt re-includes all history, so
    /// the old sum double-counted — 5.4× in the B3 baseline); output still
    /// sums (each round's completion is new generation).
    #[test]
    fn merge_round_usage_takes_max_input_and_sums_output() {
        let a = crate::TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
        };
        let b = crate::TokenUsage {
            input_tokens: 5,
            output_tokens: 1,
        };
        let merged = merge_round_usage(Some(a), Some(b)).unwrap();
        assert_eq!(merged.input_tokens, 10, "max of (10, 5), NOT the sum 15");
        assert_eq!(merged.output_tokens, 3, "outputs sum: 2 + 1");
        assert_eq!(merge_round_usage(Some(a), None).unwrap().input_tokens, 10);
        assert_eq!(merge_round_usage(None, Some(b)).unwrap().output_tokens, 1);
        assert!(merge_round_usage(None, None).is_none());
    }

    /// Regression encoding the B3 drift measurement: a turn whose six rounds
    /// reported 2,582 + 3,765 + 3,849 + 3,983 + 4,136 + 4,136 prompt tokens
    /// was tracked as 22,451 input by the old sum, while the largest prompt
    /// the backend ever evaluated was 4,136 (5.4×). The merged turn input
    /// must be 4,136.
    #[test]
    fn merge_round_usage_does_not_inflate_like_b3_baseline() {
        let rounds = [2_582u32, 3_765, 3_849, 3_983, 4_136, 4_136];
        let mut acc = None;
        for input in rounds {
            acc = merge_round_usage(
                acc,
                Some(crate::TokenUsage {
                    input_tokens: input,
                    output_tokens: 10,
                }),
            );
        }
        let u = acc.unwrap();
        assert_eq!(
            u.input_tokens, 4_136,
            "turn input = largest single prompt, not the 22,451 sum"
        );
        assert_eq!(u.output_tokens, 60);
    }

    // --- estimate_value_tokens / estimate_request_tokens (Step 18.1) ---

    #[test]
    fn estimate_value_tokens_ceiling_divides() {
        // json!("x") serializes to `"x"` — 3 chars → ceil(3/4) = 1, floor = 0.
        assert_eq!(estimate_value_tokens(&serde_json::json!("x"), EST), 1);
        // `"xxxxx"` — 7 chars → ceil(7/4) = 2.
        assert_eq!(estimate_value_tokens(&serde_json::json!("xxxxx"), EST), 2);
        // Exactly divisible: `"xx"` — 4 chars → 1.
        assert_eq!(estimate_value_tokens(&serde_json::json!("xx"), EST), 1);
    }

    #[test]
    fn estimate_request_tokens_counts_tool_schemas() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "hi"})];
        let tools = serde_json::json!([{
            "type": "function",
            "function": {"name": "list_dir", "description": "x".repeat(400)}
        }]);
        let without = estimate_request_tokens(&msgs, None, EST);
        let with = estimate_request_tokens(&msgs, Some(&tools), EST);
        assert_eq!(without, estimate_tokens(&msgs, EST));
        assert_eq!(with, without + estimate_value_tokens(&tools, EST));
        assert!(
            with - without >= 100,
            "the ~400-char schema must add ~100 tokens, got {}",
            with - without
        );
    }

    // --- PromptTracker (Step 18.1) ---

    fn fixture_tools() -> serde_json::Value {
        serde_json::json!([{
            "type": "function",
            "function": {"name": "list_dir", "description": "d".repeat(1_000)}
        }])
    }

    #[test]
    fn prompt_tracker_falls_back_to_request_estimate_without_anchor() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let tools = fixture_tools();
        let tracker = PromptTracker::new();
        assert_eq!(
            tracker.current(&msgs, Some(&tools), 1.0, EST),
            estimate_request_tokens(&msgs, Some(&tools), EST),
            "no report yet → chars/4 of messages + tool-schema tokens"
        );
    }

    #[test]
    fn prompt_tracker_anchors_on_reported_prompt_plus_appended_delta() {
        let mut msgs = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "task"}),
        ];
        let tools = fixture_tools();
        let mut tracker = PromptTracker::new();
        // Backend evaluated the 2-message request at 1,000 prompt tokens
        // (schemas + template included — far above any chars/4 guess).
        tracker.record(1_000, msgs.len());
        assert_eq!(
            tracker.current(&msgs, Some(&tools), 1.0, EST),
            1_000,
            "anchored: schema tokens NOT re-added — the report includes them"
        );
        // Two messages appended since the report add their chars/4 estimate.
        msgs.push(serde_json::json!({"role": "assistant", "content": "a".repeat(400)}));
        msgs.push(serde_json::json!({"role": "tool", "content": "b".repeat(400)}));
        let appended = estimate_tokens(&msgs[2..], EST);
        assert_eq!(
            tracker.current(&msgs, Some(&tools), 1.0, EST),
            1_000 + appended
        );
    }

    /// Phase 20 §2.3: a non-1.0 calibration ratio scales only the chars/4
    /// legs — the anchored backend report is already real tokens. Both the
    /// anchored and unanchored paths must apply it.
    #[test]
    fn prompt_tracker_calibrates_estimate_legs_with_ratio() {
        let mut msgs = vec![
            serde_json::json!({"role": "system", "content": "sys"}),
            serde_json::json!({"role": "user", "content": "task"}),
        ];
        let tools = fixture_tools();
        let ratio = 1.3_f32;
        // Unanchored: the whole request estimate scales up.
        let tracker = PromptTracker::new();
        assert_eq!(
            tracker.current(&msgs, Some(&tools), ratio, EST),
            super::super::calibrate_up(estimate_request_tokens(&msgs, Some(&tools), EST), ratio)
        );
        // Anchored: the 1,000-token report stays as-is; only the appended
        // messages' estimate scales.
        let mut tracker = PromptTracker::new();
        tracker.record(1_000, msgs.len());
        assert_eq!(
            tracker.current(&msgs, Some(&tools), ratio, EST),
            1_000,
            "no appended messages → the real-token anchor alone, unscaled"
        );
        msgs.push(serde_json::json!({"role": "tool", "content": "b".repeat(400)}));
        let appended = estimate_tokens(&msgs[2..], EST);
        assert_eq!(
            tracker.current(&msgs, Some(&tools), ratio, EST),
            1_000 + super::super::calibrate_up(appended, ratio)
        );
    }

    #[test]
    fn prompt_tracker_invalidate_and_stale_anchor_fall_back() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "hello"})];
        let mut tracker = PromptTracker::new();
        tracker.record(1_000, 1);
        tracker.invalidate();
        assert_eq!(
            tracker.current(&msgs, None, 1.0, EST),
            estimate_tokens(&msgs, EST),
            "invalidated anchor → fallback estimate"
        );
        // A stale anchor (more messages at report time than exist now — a trim
        // shrank the list) must also fall back, never index out of bounds.
        tracker.record(1_000, 5);
        assert_eq!(
            tracker.current(&msgs, None, 1.0, EST),
            estimate_tokens(&msgs, EST)
        );
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
