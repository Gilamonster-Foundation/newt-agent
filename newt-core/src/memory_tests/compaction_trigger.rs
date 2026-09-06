use super::*;

/// SEMANTICS CHANGED in Step 18.5: compression delegates to the shared
/// 18.4 pipeline, whose boundary protects the head (the original task)
/// and a ≥3-message tail — so a conversation needs enough turns to leave
/// a summarizable middle (the old provider summarized the oldest 50%
/// unconditionally). The summary in history is the pipeline's marked
/// compaction message.
#[tokio::test]
async fn summarizing_compresses_when_over_budget() {
    let mut s = Summarizing::new(512) // budget = 409 tokens
        .with_summarizer(stub_summarizer("SUMMARY"));

    let big = "x".repeat(200);
    // Five turns whose anchored fullness stays at/under the budget.
    for i in 0..5u32 {
        s.sync_turn(&big, &big, &metrics_with_input(10 + i)).await;
    }
    assert_eq!(s.compress_count, 0);
    // The reported prompt crosses the budget → delegate to the pipeline.
    s.sync_turn(&big, &big, &metrics_with_input(600)).await;

    assert!(s.compress_count >= 1, "compress_count={}", s.compress_count);
    // The chain head is the pipeline's marked compaction message.
    assert!(
        s.prev_summary.starts_with(crate::agentic::SUMMARY_PREFIX),
        "prev_summary must be the marked compaction message"
    );
    assert!(s.prev_summary.contains("SUMMARY"));
    assert!(s.prev_summary.contains(crate::agentic::SUMMARY_END_MARKER));
    assert!(
        s.history
            .iter()
            .any(|t| t.user.starts_with(crate::agentic::SUMMARY_PREFIX) && t.assistant.is_empty()),
        "the compaction message must live in history as a lone user entry"
    );
}
/// `[context] manager = append-only` must reach the memory provider too.
///
/// The compaction trigger in the agentic loop is not the only path that
/// rewrites recorded turns — `Summarizing` replaces `history` with a
/// summarized form of its own accord. Left ungoverned, an operator who
/// selected append-only would believe their transcript was untouched while
/// this provider rewrote it underneath them, which is the exact harm the
/// preset exists to prevent.
///
/// Same input, same budget crossing, two policies: one summarizes, the other
/// hands history back unchanged and never mints a compaction.
#[tokio::test]
async fn summarizing_append_only_declines_to_rewrite_history() {
    let big = "x".repeat(200);
    let drive = |rewrites_history: bool| async move {
        let big = "x".repeat(200);
        let mut s = Summarizing::new(512)
            .with_summarizer(stub_summarizer("SUMMARY"))
            .with_rewrites_history(rewrites_history);
        for i in 0..5u32 {
            s.sync_turn(&big, &big, &metrics_with_input(10 + i)).await;
        }
        let before = s.history.clone();
        s.sync_turn(&big, &big, &metrics_with_input(600)).await;
        (s, before)
    };

    let (standard, _) = drive(true).await;
    assert!(
        standard.compress_count >= 1,
        "the standard preset still compacts"
    );

    let (append, before) = drive(false).await;
    assert_eq!(
        append.compress_count, 0,
        "append-only must mint no compaction"
    );
    assert_eq!(
        append.prev_summary, "",
        "append-only must not start a summary chain"
    );
    assert!(
        append.pending_record.is_none(),
        "append-only must not stage a compaction record"
    );
    // The turns recorded before the budget crossing survive verbatim; only
    // the newly appended turn is added.
    assert_eq!(append.history.len(), before.len() + 1);
    for (kept, original) in append.history.iter().zip(before.iter()) {
        assert_eq!(kept.user, original.user);
        assert_eq!(kept.assistant, original.assistant);
    }
    let _ = big;
}
/// SEMANTICS CHANGED in Step 18.1: the reported prompt sizes must
/// actually grow past the budget for compression to fire (the old test's
/// flat 40-token turns only crossed it because the running sum inflated).
#[tokio::test]
async fn summarizing_compresses_repeatedly() {
    // Verify that the provider compresses across many turns and doesn't
    // panic. The exact compress_count depends on savings/anti-thrash.
    let mut s = Summarizing::new(512).with_summarizer(stub_summarizer("SUMMARY"));
    let text = "x".repeat(120);
    for i in 0..20u32 {
        // Backend-reported prompt grows 50, 100, 150, … — repeatedly
        // crossing the 409-token budget.
        s.sync_turn(&text, &text, &metrics_with_input(50 * (i + 1)))
            .await;
    }
    // Should have compressed at least once without panicking.
    assert!(s.compress_count >= 1);
}
/// FOLLOW-SESSION summarizer (Issue 2, the split-brain fix): after a live
/// `/backend` switch the TUI calls `MemoryManager::set_summarizer`, which
/// reaches `Summarizing::set_summarizer`. The NEXT compaction must use the
/// NEW backend's summarizer — not the one captured at session start.
#[tokio::test]
async fn a_session_inheriting_summarizer_rebinds_to_the_new_backend() {
    let mut s = Summarizing::new(512).with_summarizer(stub_summarizer("A-SUMMARY"));
    let big = "x".repeat(200);
    for i in 0..5u32 {
        s.sync_turn(&big, &big, &metrics_with_input(10 + i)).await;
    }
    s.sync_turn(&big, &big, &metrics_with_input(600)).await; // over budget → compact
    assert!(
        s.prev_summary.contains("A-SUMMARY"),
        "backend A summarizer must run first: {}",
        s.prev_summary
    );

    // The route switched to backend B — rebind in place (history untouched).
    MemoryProvider::set_summarizer(&mut s, Box::new(stub_summarizer("B-SUMMARY")));

    for i in 0..5u32 {
        s.sync_turn(&big, &big, &metrics_with_input(10 + i)).await;
    }
    s.sync_turn(&big, &big, &metrics_with_input(600)).await; // over budget → compact
    assert!(
        s.prev_summary.contains("B-SUMMARY"),
        "the rebound backend-B summarizer must be used: {}",
        s.prev_summary
    );
    assert!(
        !s.prev_summary.contains("A-SUMMARY"),
        "the stale backend-A summarizer must be gone"
    );
}
/// SEMANTICS CHANGED in Step 18.5: with no summarizer the shared
/// pipeline inserts its STATIC fallback marker (the only surviving form
/// of the old placeholder-discard) — the provider's own "turns
/// summarised" placeholder is deleted with the rest of the legacy path.
#[tokio::test]
async fn summarizing_fallback_placeholder_when_no_summarizer() {
    let policy_tokens = crate::agentic::response_repository_policy_tokens() as u32;
    // Preserve the original 204-token history budget after accounting for
    // the standing policy carried by the transient protected prompt card.
    let mut s = Summarizing::new(256 + policy_tokens * 5 / 4);
    for i in 0..6u32 {
        s.sync_turn(
            &format!("question {i}"),
            &format!("answer {i}"),
            &metrics_with_input(if i == 5 { 300 + policy_tokens } else { 10 }),
        )
        .await;
    }
    let compaction = s
        .history
        .iter()
        .find(|t| t.user.starts_with(crate::agentic::SUMMARY_PREFIX))
        .expect("static fallback marker should be inserted");
    assert!(
        compaction
            .user
            .contains("Summary generation was unavailable"),
        "got: {}",
        compaction.user
    );
}
