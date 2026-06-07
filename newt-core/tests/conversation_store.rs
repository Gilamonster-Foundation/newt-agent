use newt_core::{ConversationStore, ConversationTurn};

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
        "least recently updated record should be pruned"
    );
}

#[test]
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

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let len = a
        .bytes()
        .zip(b.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    assert!(len > 0, "test ids should share the unix timestamp prefix");
    &a[..len]
}
