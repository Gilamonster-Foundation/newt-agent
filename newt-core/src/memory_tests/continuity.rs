use super::*;

/// The compaction record is offered for persistence exactly once.
#[tokio::test]
async fn summarizing_compaction_record_is_minted_once_and_drained() {
    let mut s = Summarizing::new(512).with_summarizer(stub_summarizer("SUMMARY"));
    assert!(s.take_compaction_record().is_none(), "nothing minted yet");
    let big = "x".repeat(200);
    for i in 0..5u32 {
        s.sync_turn(&big, &big, &metrics_with_input(10 + i)).await;
    }
    s.sync_turn(&big, &big, &metrics_with_input(600)).await;
    let record = s
        .take_compaction_record()
        .expect("compression must mint a record");
    assert!(record.starts_with(crate::agentic::SUMMARY_PREFIX));
    assert_eq!(record, s.prev_summary, "the record IS the chain head");
    assert!(
        s.take_compaction_record().is_none(),
        "the record is drained on take — never persisted twice"
    );
}
/// The memory.rs:919-class bug (#247 / 18.5): restoring a compressed
/// conversation must rehydrate the summary message and the prev-summary
/// chain instead of rebuilding raw history and silently dropping both.
#[tokio::test]
async fn summarizing_restore_rehydrates_compaction_summary() {
    let marked = format!(
        "{}\nsummary of earlier work\n{}",
        crate::agentic::SUMMARY_PREFIX,
        crate::agentic::SUMMARY_END_MARKER
    );
    let turns = vec![
        crate::ConversationTurn::new("old task", "old reply"), // covered by the summary
        crate::ConversationTurn::new(marked.clone(), ""),      // the persisted record
        crate::ConversationTurn::new("recent task", "recent reply"),
    ];
    let mut s = Summarizing::new(8_192);
    s.restore_turns(&turns);

    // The chain is back: the next compression sees the previous summary.
    let pre = s.on_pre_compress(&[]).await;
    assert!(pre.contains("summary of earlier work"), "got: {pre:?}");

    // Working set = [compaction message] + turns recorded after it; the
    // summarized turn is not duplicated alongside its own summary.
    let msgs = s.build_messages("sys", "next");
    assert!(msgs
        .iter()
        .any(|m| m.content.starts_with(crate::agentic::SUMMARY_PREFIX)));
    assert!(!msgs.iter().any(|m| m.content == "old task"));
    assert!(msgs.iter().any(|m| m.content == "recent task"));
    assert!(msgs.iter().any(|m| m.content == "recent reply"));
    // The lone-sided compaction entry never dispatches an empty message.
    assert!(!msgs.iter().any(|m| m.content.is_empty()));
}
/// Restore must NOT burn a summarizer call: re-summarizing from scratch
/// on restore is exactly the behavior 18.5 removes. The next live turn
/// compresses if genuinely over budget.
#[tokio::test]
async fn summarizing_restore_never_resummarizes() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut s =
        Summarizing::new(10).with_summarizer(capturing_summarizer("SUMMARY", calls.clone()));
    let big = "x".repeat(400);
    let turns: Vec<crate::ConversationTurn> = (0..6)
        .map(|i| crate::ConversationTurn::new(format!("q{i} {big}"), format!("a{i} {big}")))
        .collect();
    s.restore_turns(&turns); // far over the 8-token budget
    assert!(
        calls.lock().unwrap().is_empty(),
        "restore must never call the summarizer"
    );
    assert_eq!(s.history.len(), 6, "restored history intact");
}
/// A compaction record restored into the default provider stays in the
/// window as a user-side summary — never dispatched with an empty
/// assistant half.
#[test]
fn rolling_window_restores_compaction_record_without_empty_assistant() {
    let marked = format!(
        "{}\nearlier work\n{}",
        crate::agentic::SUMMARY_PREFIX,
        crate::agentic::SUMMARY_END_MARKER
    );
    let mut rw = RollingWindow::new(10);
    rw.restore_turns(&[
        crate::ConversationTurn::new(marked.clone(), ""),
        crate::ConversationTurn::new("next task", "next reply"),
    ]);
    let msgs = rw.build_messages("sys", "go");
    assert!(msgs.iter().any(|m| m.content == marked));
    assert!(!msgs.iter().any(|m| m.content.is_empty()));
}
#[tokio::test]
async fn summarizing_on_pre_compress_returns_prev_summary() {
    let policy_tokens = crate::agentic::response_repository_policy_tokens() as u32;
    let mut s = Summarizing::new(256 + policy_tokens * 5 / 4)
        .with_summarizer(stub_summarizer("PRIOR SUMMARY"));
    // Build up history UNDER budget first: the pipeline's boundary needs
    // enough turns to leave a summarizable middle (Step 18.5 — head +
    // ≥3-message tail are protected), and a too-early trigger would burn
    // anti-thrash slots on nothing-to-summarize passes.
    for _ in 0..5u32 {
        s.sync_turn("question text", "answer text", &metrics_with_input(2))
            .await;
    }
    // Now cross the 204-token budget → compression sets prev_summary.
    s.sync_turn(
        "question text",
        "answer text",
        &metrics_with_input(300 + policy_tokens),
    )
    .await;
    let pre = s.on_pre_compress(&[]).await;
    assert!(
        pre.contains("PRIOR SUMMARY"),
        "compression must have run and set prev_summary, got: {pre:?}"
    );
}
