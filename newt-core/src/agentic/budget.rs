//! #727: read-only context-budget introspection — the `get_context_remaining`
//! tool.
//!
//! A budget-aware model can SEE how full its context is and read in pages /
//! compact / wrap up BEFORE saturating, instead of blindly issuing one giant
//! read (the #719 flood, averted at the source). Codex exposes the same
//! affordance (`get_context_remaining`); this is newt's read-only equivalent,
//! riding the existing `[context.estimation]` / `num_ctx` plumbing.
//!
//! The remaining budget is *dynamic* per-turn loop state — it shrinks as the
//! conversation grows — so it cannot be resolved once at config time (unlike a
//! static tool). It is computed at the agentic-loop dispatch site, where the
//! request's `num_ctx` ceiling ([`super::num_ctx_input_ceiling`]) and the
//! conversation's estimated token count ([`super::trim::PromptTracker::current`])
//! are both in scope, and rendered by the pure [`render_context_budget`] below
//! (Option A, loop-intercept: no new `execute_tool` parameter). Keeping the
//! renderer pure makes the budget math and the low-budget hint unit-testable
//! without a live loop.

use serde_json::{json, Value};

/// At or below this fraction of the ceiling *remaining*, the report appends a
/// "compact or wrap up soon" hint (issue #727 — the budget-low nudge).
const LOW_BUDGET_PCT: usize = 15;

/// Self-teaching description: when to call it, and what to do with the answer.
const DESCRIPTION: &str = "Report your remaining context budget (tokens used / \
    ceiling / remaining). Call this before a large read; if remaining is low, \
    read in pages (read_file offset+limit) or wrap up. No args; read-only.";

/// Always-advertised tool definition for `get_context_remaining` (#727). No
/// args, read-only — mirrors `resume_context` / `plan_get`.
pub fn get_context_remaining_tool_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "get_context_remaining",
            "description": DESCRIPTION,
            "parameters": { "type": "object", "properties": {}, "required": [] }
        }
    })
}

/// Render the budget report (pure — no loop / fs / clock, unit-testable).
///
/// * `used` — estimated tokens of the conversation/request so far, in the same
///   (real-token) currency the `ceiling` is in (the loop passes
///   `PromptTracker::current`, which anchors on the backend-reported prompt size
///   and calibrates the chars/4 tail up to real tokens).
/// * `ceiling` — the input-token ceiling implied by `num_ctx`
///   ([`super::num_ctx_input_ceiling`] = 80% of `num_ctx`), or `None` when no
///   `num_ctx` is configured this session.
/// * `num_ctx` — reported verbatim so the model can see the window it derives
///   from.
///
/// When `ceiling` is `None` we say so honestly rather than invent a remaining
/// budget out of a window we don't have.
pub(crate) fn render_context_budget(
    used: usize,
    ceiling: Option<usize>,
    num_ctx: Option<u32>,
) -> String {
    match ceiling {
        Some(ceiling) => {
            let remaining = ceiling.saturating_sub(used);
            let nc = num_ctx
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unset".to_string());
            let mut out = format!(
                "Context budget: ~{used} tokens used of an input ceiling of ~{ceiling} \
                 (80% of num_ctx {nc}). ~{remaining} tokens remaining."
            );
            // Integer form of `remaining / ceiling < LOW_BUDGET_PCT / 100`,
            // so no float and no division-by-zero (ceiling is always > 0 here:
            // num_ctx_input_ceiling filters out zero).
            if remaining.saturating_mul(100) < ceiling.saturating_mul(LOW_BUDGET_PCT) {
                out.push_str(
                    " Budget is LOW — compact or wrap up soon: read in pages \
                     (read_file with offset+limit), avoid large reads, and finish with \
                     what you have.",
                );
            }
            out
        }
        None => format!(
            "Context budget: ~{used} tokens used so far. No input-token ceiling is \
             configured this session (num_ctx is not set), so there is no fixed \
             remaining budget to report — keep reads modest and read large files in \
             pages (read_file with offset+limit) to avoid overflowing."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definition_is_no_arg_get_context_remaining() {
        let def = get_context_remaining_tool_definition();
        assert_eq!(def["function"]["name"], "get_context_remaining");
        // No args: an empty properties map and an empty required list.
        assert_eq!(
            def["function"]["parameters"]["properties"],
            serde_json::json!({})
        );
        assert_eq!(
            def["function"]["parameters"]["required"],
            serde_json::json!([])
        );
    }

    #[test]
    fn ceiling_minus_used_is_remaining() {
        // 8000 ceiling, 1000 used → 7000 remaining, surfaced verbatim.
        let out = render_context_budget(1000, Some(8000), Some(10_000));
        assert!(out.contains("1000 tokens used"), "{out}");
        assert!(out.contains("ceiling of ~8000"), "{out}");
        assert!(out.contains("7000 tokens remaining"), "{out}");
        assert!(out.contains("num_ctx 10000"), "{out}");
    }

    #[test]
    fn ample_budget_omits_low_hint() {
        // 5000 / 10000 = 50% remaining — well above the 15% threshold.
        let out = render_context_budget(5000, Some(10_000), Some(12_500));
        assert!(!out.contains("LOW"), "ample budget must not nudge: {out}");
    }

    #[test]
    fn low_budget_hint_fires_below_threshold() {
        // 9000 / 10000 used → 1000 (10%) remaining, below the 15% threshold.
        let out = render_context_budget(9000, Some(10_000), Some(12_500));
        assert!(out.contains("1000 tokens remaining"), "{out}");
        assert!(out.contains("LOW"), "low budget must nudge: {out}");
    }

    #[test]
    fn threshold_boundary_at_15_percent_does_not_nudge() {
        // Exactly 15% remaining (1500 / 10000) is NOT "below" the threshold.
        let out = render_context_budget(8500, Some(10_000), Some(12_500));
        assert!(out.contains("1500 tokens remaining"), "{out}");
        assert!(!out.contains("LOW"), "15% is not below 15%: {out}");
    }

    #[test]
    fn used_over_ceiling_saturates_remaining_to_zero() {
        // Over budget: remaining floors at 0 (never underflows) and nudges.
        let out = render_context_budget(12_000, Some(10_000), Some(12_500));
        assert!(out.contains("0 tokens remaining"), "{out}");
        assert!(out.contains("LOW"), "{out}");
    }

    #[test]
    fn no_ceiling_is_honest_about_no_budget() {
        // num_ctx unset → no ceiling: report usage but admit there is no
        // fixed remaining budget rather than guessing one.
        let out = render_context_budget(4321, None, None);
        assert!(out.contains("4321 tokens used"), "{out}");
        assert!(
            out.contains("No input-token ceiling is configured"),
            "{out}"
        );
        assert!(
            !out.contains("remaining."),
            "must not claim a remaining figure: {out}"
        );
    }
}
