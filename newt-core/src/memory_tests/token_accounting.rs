use super::*;

/// SEMANTICS CHANGED in Step 18.1: pruning fires when the BACKEND-reported
/// prompt size exceeds the budget — the prompt already contains all prior
/// turns, so the provider no longer manufactures overflow by summing
/// per-turn readings. Here the reported prompt genuinely grows past the
/// 100-token budget, so the oldest turns must be dropped.
#[tokio::test]
async fn token_budget_prunes_oldest_when_over_budget() {
    // max_tokens floors at 512; threshold caps at 0.99 → budget = 506.
    let mut tb = TokenBudget::new(512, 0.99);
    let budget = 506;
    let big = "x".repeat(200); // each turn adds 100 est tokens (2×200/4)
    tb.sync_turn(&big, &big, &metrics_with_input(200)).await;
    // Turn 1: used = 200 (reported) + 50 (reply est) = 250 ≤ 506 → kept.
    assert_eq!(tb.history.len(), 1);
    tb.sync_turn(&big, &big, &metrics_with_input(520)).await;
    // Turn 2: used = 520 (reported, includes both turns) + 50 = 570 > 506
    // → prune the oldest turn, reclaiming its 100-token estimate.
    assert!(tb.used_tokens() <= budget, "used = {}", tb.used_tokens());
    assert_eq!(tb.history.len(), 1, "the oldest turn must have been pruned");
}
/// SEMANTICS CHANGED in Step 18.1: with a backend report, fullness =
/// reported prompt size (30 — already includes system + history + the
/// user message) + chars/4 ceiling of the reply ("a" → 1). The old
/// assertion (50 = input 30 + output 20) double-counted: output tokens
/// were added on top of every FUTURE turn's input that re-contains them.
#[tokio::test]
async fn token_budget_uses_metrics_when_available() {
    let mut tb = TokenBudget::new(1000, 1.0);
    let mut m = dummy_metrics();
    m.usage = Some(crate::metrics::TokenUsage {
        input_tokens: 30,
        output_tokens: 20,
    });
    tb.sync_turn("q", "a", &m).await;
    assert_eq!(tb.used_tokens(), 31); // 30 (reported prompt) + 1 (reply est)
}
/// Regression for the B3 baseline's runaway drift (the 5.4× scenario):
/// a 20-turn session whose backend-evaluated prompts grow 2,582 → 4,748
/// tokens. The old running sum tracked 25,602 "used" tokens by the end —
/// 5.4× the largest prompt the backend ever evaluated — and would have
/// pruned the entire history against an 8,192 budget. The anchored math
/// must stay pinned to the last real prompt (+ reply estimate) and never
/// prune a conversation that genuinely fits.
#[tokio::test]
async fn token_budget_no_runaway_drift_b3_regression() {
    let mut tb = TokenBudget::new(8_192, 0.80); // budget = 6,553
    let mut old_running_sum: u64 = 0;
    let turns = 20u32;
    for i in 0..turns {
        // Backend-reported prompt grows linearly 2,582 → 4,748 (B3 drift
        // table endpoints); output fixed at 20 tokens per turn.
        let input = 2_582 + (4_748 - 2_582) * i / (turns - 1);
        old_running_sum += u64::from(input) + 20;
        tb.sync_turn("reply ok", "ok", &metrics_with_input(input))
            .await;
    }
    // Truthful fullness: the last real prompt (4,748) + the tiny reply
    // estimate — comfortably inside the budget, so nothing was pruned.
    let used = u64::from(tb.used_tokens());
    assert!(
        (4_748..4_800).contains(&used),
        "used must track the last real prompt, got {used}"
    );
    assert_eq!(tb.history.len(), turns as usize, "no spurious pruning");
    assert_eq!(tb.pruned_count, 0);
    // And it must be nowhere near the old inflating sum (≥ 5× smaller —
    // the B3 baseline measured 5.4× on the real session).
    assert!(
        used * 5 < old_running_sum,
        "anchored used ({used}) must be at least 5× below the old \
         running sum ({old_running_sum})"
    );
}
/// A turn carrying 17.6 token columns, for restore tests.
fn turn_with_tokens(
    user: &str,
    assistant: &str,
    tokens_in: Option<u32>,
    tokens_out: Option<u32>,
) -> crate::ConversationTurn {
    let mut t = crate::ConversationTurn::new(user, assistant);
    t.tokens_in = tokens_in;
    t.tokens_out = tokens_out;
    t
}
/// Same B3 drift scenario through `Summarizing`: the old running sum
/// blew past an 8,192-token budget by turn ~2 and burned summarizer
/// calls on a conversation whose real prompts never exceeded 4,748
/// tokens. The anchored math must never trigger compression here.
#[tokio::test]
async fn summarizing_no_runaway_compression_b3_regression() {
    let mut s = Summarizing::new(8_192) // budget = 6,553
        .with_summarizer(stub_summarizer("SUMMARY"));
    for i in 0..20u32 {
        let input = 2_582 + (4_748 - 2_582) * i / 19;
        s.sync_turn("reply ok", "ok", &metrics_with_input(input))
            .await;
    }
    assert_eq!(
        s.compress_count, 0,
        "prompts never exceeded the budget — compression must not fire"
    );
    assert!(s.prev_summary.is_empty());
    assert_eq!(s.history.len(), 20);
}
/// 18.5 token restore: the anchor comes from the persisted 17.6 column
/// of the LAST measured turn — not a chars/4 re-estimation of history.
#[test]
fn token_budget_restore_anchors_on_column_tokens() {
    let mut tb = TokenBudget::new(100_000, 0.80);
    let turns = vec![
        turn_with_tokens("u1", "a1", Some(900), Some(10)),
        turn_with_tokens("u2", "aa", Some(1_000), Some(12)),
    ];
    tb.restore_turns(&turns);
    // 1,000 (measured prompt — already contains everything before it)
    // + ceil(2/4) = 1 for the reply not yet inside any prompt.
    assert_eq!(tb.last_prompt_tokens, Some(1_000));
    assert_eq!(tb.used_tokens(), 1_001);
}
/// NULL columns (pre-17.6 rows, silent backends) fall back to the
/// estimate WITHOUT ever becoming the anchor — an estimate is never
/// presented as a measurement (18.1 honesty).
#[test]
fn token_budget_restore_null_columns_fall_back_to_estimate() {
    let mut tb = TokenBudget::new(100_000, 0.80);
    let text = "x".repeat(40); // 10 est tokens per side
    let turns = vec![
        crate::ConversationTurn::new(text.clone(), text.clone()),
        crate::ConversationTurn::new(text.clone(), text.clone()),
    ];
    tb.restore_turns(&turns);
    assert_eq!(
        tb.last_prompt_tokens, None,
        "no measurement in the store → no anchor, only estimates"
    );
    assert_eq!(tb.used_tokens(), 40, "2 turns × (10 + 10) est tokens");
}
/// Turns recorded after the last measured one (silent backend) extend
/// the delta with estimates while the anchor stays the real measurement.
#[test]
fn token_budget_restore_unmeasured_tail_extends_delta() {
    let mut tb = TokenBudget::new(100_000, 0.80);
    let turns = vec![
        turn_with_tokens("u1", "aaaa", Some(1_000), Some(8)), // reply est 1
        crate::ConversationTurn::new("xxxx", "yyyy"),         // est 2
    ];
    tb.restore_turns(&turns);
    assert_eq!(tb.last_prompt_tokens, Some(1_000));
    assert_eq!(tb.used_tokens(), 1_003);
}
/// Same column-first restore through `Summarizing`.
#[test]
fn summarizing_restore_anchors_on_column_tokens() {
    let mut s = Summarizing::new(100_000);
    let turns = vec![
        turn_with_tokens("u1", "a1", Some(700), Some(9)),
        turn_with_tokens("u2", "aaaa", Some(2_000), Some(11)),
    ];
    s.restore_turns(&turns);
    assert_eq!(s.last_prompt_tokens, Some(2_000));
    assert_eq!(s.used_tokens(), 2_001); // 2,000 + ceil(4/4)
}
/// Measurements taken BEFORE the compaction cut describe prompts of a
/// pre-compression shape — they must not anchor the restored (smaller)
/// working set.
#[test]
fn summarizing_restore_ignores_measurements_before_the_cut() {
    let marked = format!(
        "{}\nolder work compressed\n{}",
        crate::agentic::SUMMARY_PREFIX,
        crate::agentic::SUMMARY_END_MARKER
    );
    let mut s = Summarizing::new(100_000);
    let turns = vec![
        turn_with_tokens("old", "old", Some(50_000), Some(10)),
        crate::ConversationTurn::new(marked, ""),
        crate::ConversationTurn::new("after", "compaction"), // unmeasured
    ];
    s.restore_turns(&turns);
    assert_eq!(
        s.last_prompt_tokens, None,
        "a pre-compression measurement must not anchor the cut working set"
    );
}
#[tokio::test]
async fn token_budget_does_not_prune_within_budget() {
    let mut tb = TokenBudget::new(200, 1.0); // budget = 200
    let mut m = dummy_metrics();
    m.usage = Some(crate::metrics::TokenUsage {
        input_tokens: 50,
        output_tokens: 50,
    });
    tb.sync_turn("q", "a", &m).await; // 50 + 1 reply-est — within budget
    assert_eq!(tb.history.len(), 1);
}
/// Without any backend report the provider falls back to summing per-turn
/// content estimates — each turn counted once (no double-count).
#[tokio::test]
async fn token_budget_estimate_fallback_counts_each_turn_once() {
    let mut tb = TokenBudget::new(1000, 1.0);
    let mut m = dummy_metrics();
    m.usage = None; // backend reported nothing
    let text = "x".repeat(40); // 40 chars → 10 est tokens per side
    tb.sync_turn(&text, &text, &m).await;
    tb.sync_turn(&text, &text, &m).await;
    assert_eq!(tb.used_tokens(), 40, "2 turns × (10 + 10) est tokens");
}
