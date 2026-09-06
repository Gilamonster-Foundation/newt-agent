use super::*;

#[tokio::test]
async fn memory_manager_add_note_fails_with_no_note_store() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    let err = mgr.add_note("fact").unwrap_err().to_string();
    assert!(
        err.contains("no note-capable memory provider"),
        "guidance error expected: {err}"
    );
}
#[tokio::test]
async fn rolling_window_add_note_returns_notes_unsupported() {
    let mut rw = RollingWindow::new(5);
    let err = rw.add_note("fact").unwrap_err();
    assert!(err.is::<NotesUnsupported>());
}
#[tokio::test]
async fn rolling_window_replace_and_remove_note_return_notes_unsupported() {
    // The trait defaults (Step 19.3) mirror add_note so the manager's
    // routing can skip note-less providers for every mutation kind.
    let mut rw = RollingWindow::new(5);
    assert!(rw
        .replace_note("old", "new")
        .unwrap_err()
        .is::<NotesUnsupported>());
    assert!(rw.remove_note("old").unwrap_err().is::<NotesUnsupported>());
}
#[tokio::test]
async fn memory_manager_replace_and_remove_route_to_note_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    let mut mgr = MemoryManager::new();
    // A note-less provider first — routing must skip it (NotesUnsupported).
    mgr.add_provider(RollingWindow::new(5));
    mgr.add_provider(NoteStore::new(path.clone(), 2200));
    let ctx = SessionContext {
        workspace: "/ws".into(),
        session_id: "s".into(),
    };
    mgr.initialize_all(&ctx).await;

    mgr.add_note("model alpha is the fast tier").unwrap();
    mgr.add_note("workspace uses just check").unwrap();

    mgr.replace_note("alpha", "model beta is the fast tier")
        .unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("model beta"), "{raw}");
    assert!(!raw.contains("model alpha"), "{raw}");

    mgr.remove_note("just check").unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("just check"), "{raw}");
    assert!(raw.contains("model beta"), "other entry untouched: {raw}");

    // Real rejections surface: ambiguity / zero-match errors come back.
    let err = mgr.remove_note("nonexistent").unwrap_err().to_string();
    assert!(err.contains("no entry contains"), "{err}");
}
#[tokio::test]
async fn memory_manager_replace_and_remove_fail_with_no_note_store() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    let err = mgr.replace_note("a", "b").unwrap_err().to_string();
    assert!(err.contains("no note-capable memory provider"), "{err}");
    let err = mgr.remove_note("a").unwrap_err().to_string();
    assert!(err.contains("no note-capable memory provider"), "{err}");
}
#[tokio::test]
async fn memory_manager_add_note_routes_to_note_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    mgr.add_provider(NoteStore::new(path.clone(), 2200));
    let ctx = SessionContext {
        workspace: "/ws".into(),
        session_id: "s".into(),
    };
    mgr.initialize_all(&ctx).await;
    mgr.add_note("the answer is 42").unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("the answer is 42"));
}
/// A provider with a non-`note_store` name that accepts notes — proves
/// the manager no longer special-cases `name() == "note_store"`.
struct CustomNotes {
    notes: Vec<String>,
}

#[async_trait]
impl MemoryProvider for CustomNotes {
    fn name(&self) -> &str {
        "custom_notes"
    }
    fn build_messages(&self, _system_prompt: &str, _new_task: &str) -> Vec<MemMessage> {
        Vec::new()
    }
    async fn sync_turn(&mut self, _user: &str, _assistant: &str, _metrics: &TurnMetrics) {}
    fn add_note(&mut self, fact: &str) -> anyhow::Result<()> {
        self.notes.push(fact.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn memory_manager_add_note_first_ok_wins_regardless_of_name() {
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5)); // unsupported — skipped
    mgr.add_provider(CustomNotes { notes: Vec::new() });
    mgr.add_note("routed by capability, not by name").unwrap();
}
#[tokio::test]
async fn memory_manager_add_note_surfaces_curator_error() {
    // A real rejection from a note-capable provider (the over-budget
    // curator error) must reach the caller, not be swallowed by the
    // generic "no provider" message.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("NOTES.md");
    let mut mgr = MemoryManager::new();
    mgr.add_provider(RollingWindow::new(5));
    mgr.add_provider(NoteStore::new(path, 40));
    let ctx = SessionContext {
        workspace: "/ws".into(),
        session_id: "s".into(),
    };
    mgr.initialize_all(&ctx).await;
    mgr.add_note("an entry that fits").unwrap();
    let err = mgr.add_note(&"x".repeat(80)).unwrap_err().to_string();
    assert!(
        err.contains("Replace or remove existing entries first"),
        "curator error must propagate: {err}"
    );
    assert!(err.contains("an entry that fits"), "{err}");
}
