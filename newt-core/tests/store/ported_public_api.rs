use super::*;

// =========================================================================
// Part 1 — the ported public-API suite (semantics identical to the JSON
// backend's tests/conversation_store.rs) + the shared free-function tests
// that moved here when 17.1b deleted that suite.
// =========================================================================

// --- Per-session plan files (issue #220) ---

#[test]
fn session_plan_path_is_workspace_relative_under_sessions() {
    assert_eq!(
        session_plan_path("abc-123"),
        std::path::Path::new(".scratch/sessions/abc-123/plan.md"),
    );
    assert_eq!(
        session_plan_dir("abc-123"),
        std::path::Path::new(".scratch/sessions/abc-123"),
    );
}

#[test]
fn new_conversation_id_is_unique_and_record_id_valid() {
    let a = new_conversation_id();
    let b = new_conversation_id();
    assert_ne!(a, b, "two ids must differ (collision fix relies on this)");
    // Must be a valid record id (alphanumeric + '-') so create_with_id accepts it.
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    assert!(a.contains('-'));
}

#[test]
fn create_with_id_adopts_the_supplied_id() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = new_conversation_id();
    assert!(!store.exists(&id).unwrap());
    store
        .create_with_id(&id, "pre-assigned title", Some("coder"))
        .unwrap();
    assert!(store.exists(&id).unwrap());

    let record = store.load(&id).unwrap();
    assert_eq!(record.id, id);
    assert_eq!(record.title, "pre-assigned title");
    assert_eq!(record.persona.as_deref(), Some("coder"));
}

#[test]
fn delete_removes_the_per_session_plan_dir() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("task", None).unwrap();
    // Use the canonicalized workspace, matching what the store stores/cleans up.
    let ws = std::fs::canonicalize(workspace.path()).unwrap();
    // Simulate the model having written a plan into the session's dir.
    let plan = ws.join(session_plan_path(&id));
    std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
    std::fs::write(&plan, "# plan\n- [ ] step 1\n").unwrap();
    assert!(plan.exists());

    store.delete(&id).unwrap();
    assert!(
        !ws.join(session_plan_dir(&id)).exists(),
        "deleting a conversation must remove its plan dir (issue #220)"
    );
}

#[test]
fn conversation_store_roundtrips_user_assistant_turns_by_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let other_workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("Initial task", Some("coder")).unwrap();
    store
        .append_turn(&id, "write the parser", "parser written")
        .unwrap();
    store.append_turn(&id, "run tests", "tests passed").unwrap();

    let restored = store.load(&id).unwrap();
    assert_eq!(restored.id, id);
    assert_eq!(restored.title, "Initial task");
    assert_eq!(restored.persona.as_deref(), Some("coder"));
    assert_eq!(
        restored.turns,
        vec![
            ConversationTurn::new("write the parser", "parser written"),
            ConversationTurn::new("run tests", "tests passed"),
        ]
    );

    assert_eq!(store.list().unwrap().len(), 1);
    let other_store = ConversationStore::new(root.path(), other_workspace.path(), 100).unwrap();
    assert!(
        other_store.list().unwrap().is_empty(),
        "conversations must be namespaced by workspace"
    );
}

#[test]
fn conversation_store_prunes_oldest_records_by_configured_cap() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 2).unwrap();

    let first = store.create("one", None).unwrap();
    let second = store.create("two", None).unwrap();
    let third = store.create("three", None).unwrap();

    let summaries = store.list().unwrap();
    let ids: Vec<_> = summaries.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![second.as_str(), third.as_str()]);
    assert!(
        store.load(&first).is_err(),
        "oldest record should be pruned"
    );
}

#[test]
fn conversation_store_prunes_by_last_update() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 2).unwrap();

    let first = store.create("one", None).unwrap();
    let second = store.create("two", None).unwrap();
    store.append_turn(&first, "resume one", "done").unwrap();
    let third = store.create("three", None).unwrap();

    let summaries = store.list().unwrap();
    let ids: Vec<_> = summaries.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![first.as_str(), third.as_str()]);
    assert!(
        store.load(&second).is_err(),
        "least recently active record should be pruned"
    );
}

/// The RETIRED v1 (UUIDv5) derivation must stay stable: the 17.2 migration
/// and the legacy-import dir names both depend on it reproducing the keys
/// historical rows were written under.
#[test]
#[allow(deprecated)]
fn workspace_id_is_stable_for_the_same_canonical_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let first = ConversationStore::workspace_id_for_path(workspace.path()).unwrap();
    let second = ConversationStore::workspace_id_for_path(workspace.path()).unwrap();

    assert_eq!(first, second);
}

#[test]
fn conversation_store_rejects_path_like_ids() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let load_err = store.load("../outside").unwrap_err().to_string();
    let delete_err = store.delete("..\\outside").unwrap_err().to_string();

    assert!(load_err.contains("invalid conversation id"));
    assert!(delete_err.contains("invalid conversation id"));
}

#[test]
fn conversation_store_accepts_unique_id_prefixes() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("Initial task", None).unwrap();
    store
        .append_turn(&id, "write docs", "docs written")
        .unwrap();
    let prefix = &id[..12];

    let restored = store.load(prefix).unwrap();
    assert_eq!(restored.id, id);

    store.rename(prefix, "Renamed").unwrap();
    assert_eq!(store.load(&id).unwrap().title, "Renamed");

    store.delete(prefix).unwrap();
    assert!(store.load(&id).is_err());
}

#[test]
fn conversation_store_rejects_ambiguous_id_prefixes() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let first = store.create("one", None).unwrap();
    let second = store.create("two", None).unwrap();
    let shared_prefix = common_prefix(&first, &second);

    let err = store.load(shared_prefix).unwrap_err().to_string();
    assert!(err.contains("ambiguous conversation id prefix"));
}

/// SQLite analogue of `corrupt_record_does_not_poison_the_workspace`: a
/// legacy JSON tree that appears AFTER the store is opened (the one-time
/// import runs at open only) must not affect the live store in any way —
/// it just waits for the next open to be imported.
#[test]
fn legacy_json_records_beside_the_db_do_not_poison_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    // Legacy dirs are named by the retired v1 derivation.
    #[allow(deprecated)]
    let workspace_id = ConversationStore::workspace_id_for_path(workspace.path()).unwrap();
    let legacy_dir = root.path().join("conversations").join(&workspace_id);
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(
        legacy_dir.join("9999999999-corrupt.json"),
        "{not json at all",
    )
    .unwrap();

    let id = store.create("good", None).unwrap();
    store.append_turn(&id, "hello", "world").unwrap();

    let summaries = store.list().unwrap();
    assert_eq!(summaries.len(), 1, "legacy JSON files must be invisible");
    assert_eq!(summaries[0].id, id);

    store
        .append_turn(&id, "still", "works")
        .expect("append must survive legacy JSON siblings");
    assert_eq!(store.load(&id).unwrap().turns.len(), 2);
}

#[test]
fn append_turn_does_not_prune() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();

    // Two records created under a permissive cap…
    let wide = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let first = wide.create("one", None).unwrap();
    let second = wide.create("two", None).unwrap();

    // …then a store with a tighter cap appends a turn. Appending never
    // changes the record count, so it must not trigger pruning — only
    // `create` prunes.
    let tight = ConversationStore::new(root.path(), workspace.path(), 1).unwrap();
    tight.append_turn(&second, "more", "turns").unwrap();

    assert_eq!(tight.list().unwrap().len(), 2);
    assert!(tight.load(&first).is_ok(), "append must not prune siblings");

    // The next create DOES prune back to the cap.
    let third = tight.create("three", None).unwrap();
    let ids: Vec<_> = tight.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![third]);
}

/// SQLite analogue of `save_is_atomic_and_leaves_no_temp_files`: writes are
/// transactional (a failed append leaves nothing behind) and the root
/// contains only the database artifacts — no stray temp files.
#[test]
fn writes_are_transactional_and_leave_no_stray_files() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("atomic", None).unwrap();
    store.append_turn(&id, "a", "b").unwrap();
    store.rename(&id, "renamed").unwrap();

    // An append to a nonexistent conversation fails cleanly…
    assert!(store.append_turn("no-such-conversation", "x", "y").is_err());
    // …without leaving partial state (the failed transaction rolled back).
    assert_eq!(store.load(&id).unwrap().turns.len(), 1);
    assert_eq!(store.list().unwrap().len(), 1);

    let allowed = [
        "conversations.db",
        "conversations.db-wal",
        "conversations.db-shm",
        "install-nonce",
    ];
    let strays: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !allowed.contains(&name.as_str()))
        .collect();
    assert!(strays.is_empty(), "no stray files after saves: {strays:?}");

    let restored = store.load(&id).unwrap();
    assert_eq!(restored.title, "renamed");
    assert_eq!(restored.turns.len(), 1);
}
