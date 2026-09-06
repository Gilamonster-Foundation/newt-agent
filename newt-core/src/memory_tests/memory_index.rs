use super::*;

fn index_ctx(dir: &std::path::Path) -> SessionContext {
    SessionContext {
        workspace: dir.to_string_lossy().into(),
        session_id: "s".into(),
    }
}
/// Seed a NOTES file with `n` entries (using NoteStore's own write path so
/// the §-delimited on-disk format is exactly what `MemoryIndex` reads).
async fn seed_notes(path: &std::path::Path, n: usize) {
    let mut ns = NoteStore::new(path, NoteStore::DEFAULT_CHAR_LIMIT);
    ns.initialize(&index_ctx(path.parent().unwrap()))
        .await
        .unwrap();
    for i in 0..n {
        ns.add(&format!("note number {i}\nbody line for {i}"))
            .unwrap();
    }
}
/// CI-PINNED BUDGET (the modulex `DEFAULT_TOOL_BUDGET` pattern, design
/// §2.3/§3.3): the frozen memory index lists at most `MEMORY_INDEX_BUDGET`
/// items, no matter how many notes exist. Growing the budget is a
/// deliberate edit to the constant — this test fails if a feature grows the
/// default surface as a side effect.
#[tokio::test]
async fn memory_index_stays_under_pinned_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    // Seed MANY more notes than the budget.
    seed_notes(&path, MEMORY_INDEX_BUDGET + 25).await;

    let mut idx = MemoryIndex::new(&path);
    idx.initialize(&index_ctx(dir.path())).await.unwrap();

    assert!(
        idx.rows().len() <= MEMORY_INDEX_BUDGET,
        "index surface ({}) exceeds the pinned budget ({MEMORY_INDEX_BUDGET})",
        idx.rows().len()
    );
    assert_eq!(idx.rows().len(), MEMORY_INDEX_BUDGET, "fills to the cap");
    // The block names the overflow recovery (recall), never silently drops.
    let block = idx.system_prompt_block().unwrap();
    assert!(block.contains("use `recall`"), "overflow hint: {block}");
}
#[tokio::test]
async fn memory_index_lists_ids_and_titles_not_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    seed_notes(&path, 2).await;

    let mut idx = MemoryIndex::new(&path);
    idx.initialize(&index_ctx(dir.path())).await.unwrap();
    let block = idx.system_prompt_block().unwrap();

    // Ids + first-line titles are listed …
    assert!(block.contains("note:1  note number 0"), "got: {block}");
    assert!(block.contains("note:2  note number 1"), "got: {block}");
    // … but NOT the bodies (those are fetched via memory_fetch).
    assert!(!block.contains("body line for 0"), "body leaked: {block}");
    assert!(
        block.contains("call `memory_fetch`"),
        "names the fetch tool: {block}"
    );
}
#[tokio::test]
async fn memory_index_is_system_prompt_only() {
    // Like NoteStore / SoulProvider — never competes for the
    // first-non-empty build_messages slot.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    seed_notes(&path, 1).await;
    let mut idx = MemoryIndex::new(&path);
    idx.initialize(&index_ctx(dir.path())).await.unwrap();
    assert!(idx.build_messages("sys", "task").is_empty());
}
#[tokio::test]
async fn memory_index_empty_notes_contributes_no_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    let mut idx = MemoryIndex::new(&path); // no notes file at all
    idx.initialize(&index_ctx(dir.path())).await.unwrap();
    assert!(idx.system_prompt_block().is_none());
}
/// INERT BY DEFAULT (#319 acceptance): with no MemoryIndex registered (the
/// `disclosure = "frozen"` default), the manager's system-prompt additions
/// and messages are byte-identical to a manager that also omits it — and
/// crucially they carry NO "Memory index" block. Opting in (index mode)
/// ADDS the block, proving the frozen path is genuinely the no-op branch.
/// The MVP changes nothing unless opted in.
#[tokio::test]
async fn disclosure_frozen_default_is_bit_for_bit_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    seed_notes(&path, 3).await;

    // Today's shape: RollingWindow + NoteStore, NO MemoryIndex (frozen).
    async fn build(
        path: &std::path::Path,
        ws: &std::path::Path,
        with_index: bool,
    ) -> (String, Vec<MemMessage>) {
        let mut mgr = MemoryManager::new();
        mgr.add_provider(RollingWindow::new(20));
        mgr.add_provider(NoteStore::new(path, NoteStore::DEFAULT_CHAR_LIMIT));
        if with_index {
            mgr.add_provider(MemoryIndex::new(path));
        }
        mgr.initialize_all(&SessionContext {
            workspace: ws.to_string_lossy().into(),
            session_id: "s".into(),
        })
        .await;
        (
            mgr.build_system_prompt_additions(),
            mgr.build_messages("sys", "task"),
        )
    }

    let (frozen_sys, frozen_msgs) = build(&path, dir.path(), false).await;
    // Registration is the only difference; omitting MemoryIndex is a no-op.
    let (frozen_sys2, frozen_msgs2) = build(&path, dir.path(), false).await;
    assert_eq!(frozen_sys, frozen_sys2);
    assert_eq!(frozen_msgs, frozen_msgs2);
    assert!(
        !frozen_sys.contains("Memory index"),
        "frozen mode must NOT add the block: {frozen_sys}"
    );

    // Opting in (index mode) ADDS the index block.
    let (index_sys, _index_msgs) = build(&path, dir.path(), true).await;
    assert!(
        index_sys.contains("Memory index"),
        "index mode adds the block: {index_sys}"
    );
}
