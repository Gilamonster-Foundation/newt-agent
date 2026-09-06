use super::*;

#[tokio::test]
async fn memory_manager_routes_to_provider() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    let msgs = mgr.build_messages("sys", "hello");
    assert_eq!(msgs[0].role, Role::System);
    assert_eq!(msgs.last().unwrap().content, "hello");
}
/// The `MemoryManager::set_summarizer` fan-out only builds when a provider
/// actually consumes a summarizer — a `token_budget` / `rolling` session
/// never pays to construct one (the embedded engine may load a GGUF).
#[test]
fn manager_set_summarizer_does_not_build_without_a_summarizing_provider() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(TokenBudget::new(4_096, 0.80));
    // The `FnOnce` MUST NOT run.
    mgr.set_summarizer(|| panic!("no Summarizing provider → must not build a summarizer"));
}
/// …and it builds EXACTLY once (no per-provider fan-out cost) when a
/// `Summarizing` provider is present.
#[test]
fn manager_set_summarizer_builds_exactly_once_for_a_summarizing_provider() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let builds = std::sync::Arc::new(AtomicUsize::new(0));
    let mut mgr = MemoryManager::new();
    mgr.add_provider(TokenBudget::new(4_096, 0.80)); // ignores the summarizer
    mgr.add_provider(Summarizing::new(512).with_summarizer(stub_summarizer("A")));
    let b = builds.clone();
    mgr.set_summarizer(move || {
        b.fetch_add(1, Ordering::SeqCst);
        Box::new(stub_summarizer("B")) as crate::agentic::Summarizer
    });
    assert_eq!(builds.load(Ordering::SeqCst), 1);
}
/// The manager drains the record from whichever provider minted it.
#[tokio::test]
async fn memory_manager_routes_take_compaction_record() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(50)); // mints nothing
    mgr.add_provider(Summarizing::new(512).with_summarizer(stub_summarizer("SUMMARY")));
    assert!(mgr.take_compaction_record().is_none());
    let big = "x".repeat(200);
    for i in 0..5u32 {
        mgr.sync_all_with_active_task(&big, &big, &metrics_with_input(10 + i), &big)
            .await;
    }
    mgr.sync_all_with_active_task(&big, &big, &metrics_with_input(600), &big)
        .await;
    let record = mgr
        .take_compaction_record()
        .expect("manager must surface the Summarizing provider's record");
    assert!(record.starts_with(crate::agentic::SUMMARY_PREFIX));
    assert!(mgr.take_compaction_record().is_none());
}
#[tokio::test]
async fn memory_manager_on_pre_compress() {
    let mgr = MemoryManager::new();
    let result = mgr.on_pre_compress(&[]).await;
    assert!(result.is_empty());
}
#[tokio::test]
async fn memory_manager_on_session_end() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    mgr.on_session_end(&[]).await; // must not panic
}
#[tokio::test]
async fn memory_manager_prefetch_all_empty() {
    let mgr = MemoryManager::new();
    let result = mgr.prefetch_all("query").await;
    assert!(result.is_empty());
}
#[tokio::test]
async fn memory_manager_build_system_prompt_additions_from_note_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    std::fs::write(&path, "fact one\nfact two").unwrap();
    let mut ns = NoteStore::new(path, 2200);
    let ctx = SessionContext {
        workspace: "/ws".into(),
        session_id: "s".into(),
    };
    ns.initialize(&ctx).await.unwrap();

    let mut mgr = MemoryManager::new();
    mgr.add_provider(ns);
    let additions = mgr.build_system_prompt_additions();
    assert!(additions.contains("fact one"));
}
#[tokio::test]
async fn memory_manager_sync_all_with_active_task() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    mgr.sync_all_with_active_task("q", "a", &dummy_metrics(), "q")
        .await;
    let usage = mgr.usage();
    assert_eq!(usage[0].1, 1); // 1 turn stored
}
#[tokio::test]
async fn memory_manager_reset_all_clears_conversation_history() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    mgr.sync_all_with_active_task("old task", "old reply", &dummy_metrics(), "old task")
        .await;

    let before = mgr.build_messages("system", "new task");
    assert!(before.iter().any(|m| m.content == "old task"));
    assert!(before.iter().any(|m| m.content == "old reply"));

    mgr.reset_all();

    let after = mgr.build_messages("system", "new task");
    assert!(!after.iter().any(|m| m.content == "old task"));
    assert!(!after.iter().any(|m| m.content == "old reply"));
    assert!(after.iter().any(|m| m.content == "new task"));
}
#[tokio::test]
async fn memory_manager_restore_turns_replaces_conversation_history() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    mgr.sync_all_with_active_task("old task", "old reply", &dummy_metrics(), "old task")
        .await;

    mgr.restore_turns(&[
        crate::ConversationTurn::new("restored task", "restored reply"),
        crate::ConversationTurn::new("follow up", "followed up"),
    ]);

    let messages = mgr.build_messages("system", "new task");
    assert!(!messages.iter().any(|m| m.content == "old task"));
    assert!(!messages.iter().any(|m| m.content == "old reply"));
    assert!(messages.iter().any(|m| m.content == "restored task"));
    assert!(messages.iter().any(|m| m.content == "restored reply"));
    assert!(messages.iter().any(|m| m.content == "follow up"));
    assert!(messages.iter().any(|m| m.content == "followed up"));
    assert!(messages.iter().any(|m| m.content == "new task"));
}
#[tokio::test]
async fn memory_manager_fallback_with_no_providers() {
    let mgr = MemoryManager::new();
    let msgs = mgr.build_messages("sys", "task");
    assert_eq!(msgs.len(), 2);
}
