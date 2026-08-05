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
/// * `ceiling` — the effective input-token ceiling implied by a known context
///   window, the configured percentage, and any reserved output budget. It is
///   `None` while the window is unknown; numbered 400 recovery can establish a
///   ceiling mid-turn even when the session began without `num_ctx`.
/// * `num_ctx` — reported verbatim so the model can see the window it derives
///   from.
/// * `input_ceiling_pct` — `[context] input_ceiling_pct`; shown as the configured
///   percentage bound without claiming it alone produced the effective ceiling.
/// * `low_budget_pct` — `[context] low_budget_pct`; at or below this fraction of
///   the ceiling *remaining*, the report appends the "compact or wrap up" hint.
///
/// When `ceiling` is `None` we say so honestly rather than invent a remaining
/// budget out of a window we don't have.
pub(crate) fn render_context_budget(
    used: usize,
    ceiling: Option<usize>,
    num_ctx: Option<u32>,
    input_ceiling_pct: u32,
    low_budget_pct: usize,
) -> String {
    let input_ceiling_pct = crate::config::normalize_input_ceiling_pct(input_ceiling_pct);
    match ceiling {
        Some(ceiling) => {
            let remaining = ceiling.saturating_sub(used);
            let nc = num_ctx
                .map(|c| c.to_string())
                .unwrap_or_else(|| "unset".to_string());
            let mut out = format!(
                "Context budget: ~{used} tokens used of an input ceiling of ~{ceiling} \
                 (configured num_ctx {nc}; percentage bound {input_ceiling_pct}%; output \
                 and recovery reserves may tighten it). ~{remaining} tokens remaining."
            );
            // Integer form of `remaining / ceiling < low_budget_pct / 100`,
            // with an explicit zero-ceiling arm so an impossible request is
            // reported as low instead of looking healthy.
            if ceiling == 0
                || remaining.saturating_mul(100) < ceiling.saturating_mul(low_budget_pct)
            {
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
        let out = render_context_budget(1000, Some(8000), Some(10_000), 80, 15);
        assert!(out.contains("1000 tokens used"), "{out}");
        assert!(out.contains("ceiling of ~8000"), "{out}");
        assert!(out.contains("7000 tokens remaining"), "{out}");
        assert!(out.contains("num_ctx 10000"), "{out}");
        assert!(out.contains("percentage bound 80%"), "{out}");
    }

    #[test]
    fn ample_budget_omits_low_hint() {
        // 5000 / 10000 = 50% remaining — well above the 15% threshold.
        let out = render_context_budget(5000, Some(10_000), Some(12_500), 80, 15);
        assert!(!out.contains("LOW"), "ample budget must not nudge: {out}");
    }

    #[test]
    fn low_budget_hint_fires_below_threshold() {
        // 9000 / 10000 used → 1000 (10%) remaining, below the 15% threshold.
        let out = render_context_budget(9000, Some(10_000), Some(12_500), 80, 15);
        assert!(out.contains("1000 tokens remaining"), "{out}");
        assert!(out.contains("LOW"), "low budget must nudge: {out}");
    }

    #[test]
    fn threshold_boundary_at_15_percent_does_not_nudge() {
        // Exactly 15% remaining (1500 / 10000) is NOT "below" the threshold.
        let out = render_context_budget(8500, Some(10_000), Some(12_500), 80, 15);
        assert!(out.contains("1500 tokens remaining"), "{out}");
        assert!(!out.contains("LOW"), "15% is not below 15%: {out}");
    }

    #[test]
    fn used_over_ceiling_saturates_remaining_to_zero() {
        // Over budget: remaining floors at 0 (never underflows) and nudges.
        let out = render_context_budget(12_000, Some(10_000), Some(12_500), 80, 15);
        assert!(out.contains("0 tokens remaining"), "{out}");
        assert!(out.contains("LOW"), "{out}");
    }

    #[test]
    fn zero_effective_ceiling_reports_low_instead_of_failing_open() {
        let out = render_context_budget(0, Some(0), Some(8_000), 80, 15);
        assert!(out.contains("ceiling of ~0"), "{out}");
        assert!(out.contains("0 tokens remaining"), "{out}");
        assert!(out.contains("LOW"), "{out}");
    }

    #[test]
    fn tunable_low_budget_pct_shifts_the_nudge_threshold() {
        // 8000/10000 used → 2000 (20%) remaining. Default 15% would NOT nudge,
        // but a tuned 25% threshold DOES — proving the knob is live.
        let default = render_context_budget(8000, Some(10_000), Some(12_500), 80, 15);
        assert!(!default.contains("LOW"), "20% is above 15%: {default}");
        let tuned = render_context_budget(8000, Some(10_000), Some(12_500), 80, 25);
        assert!(tuned.contains("LOW"), "20% is below 25%: {tuned}");
    }

    #[test]
    fn tunable_input_ceiling_pct_shows_in_report() {
        // The configured percentage bound is echoed verbatim (90% here, not
        // the default 80%) without claiming it is necessarily the active bound.
        let out = render_context_budget(1000, Some(9000), Some(10_000), 90, 15);
        assert!(out.contains("percentage bound 90%"), "{out}");
    }

    #[test]
    fn programmatic_invalid_percentage_is_normalized_in_the_report() {
        let out = render_context_budget(1000, Some(8000), Some(10_000), 0, 15);
        assert!(out.contains("percentage bound 80%"), "{out}");
        assert!(!out.contains("percentage bound 0%"), "{out}");
    }

    #[test]
    fn output_reserve_ceiling_is_not_misreported_as_percentage_derived() {
        // Contemplating at 32K reserves 16K for output, so the effective input
        // ceiling (16,768) is tighter than the configured 80% ceiling (26,214).
        let out = render_context_budget(1000, Some(16_768), Some(32_768), 80, 15);
        assert!(out.contains("configured num_ctx 32768"), "{out}");
        assert!(out.contains("percentage bound 80%"), "{out}");
        assert!(
            out.contains("output and recovery reserves may tighten it"),
            "{out}"
        );
        assert!(!out.contains("80% of num_ctx"), "{out}");
    }

    #[test]
    fn no_ceiling_is_honest_about_no_budget() {
        // num_ctx unset → no ceiling: report usage but admit there is no
        // fixed remaining budget rather than guessing one.
        let out = render_context_budget(4321, None, None, 80, 15);
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
