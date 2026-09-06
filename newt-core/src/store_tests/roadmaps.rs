use super::*;

// ── #1086: roadmap import must not steal another workspace's row ──────────

/// Two workspaces sharing one `conversations.db` (different workspace
/// keys, same store root) must own their same-id roadmaps independently.
/// Reproduces the steal: before the composite PK, `create_roadmap` in
/// workspace B `INSERT OR REPLACE`d workspace A's row out from under it.
#[test]
fn create_roadmap_is_workspace_fenced_and_never_steals() {
    let root = tempfile::TempDir::new().unwrap();
    let ws_a = tempfile::TempDir::new().unwrap();
    let ws_b = tempfile::TempDir::new().unwrap();
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

    // Same roadmap id in both workspaces (exactly what /roadmap import of a
    // shared file into an unrelated workspace does).
    let id = "1783727322129749288-shared";
    store_a
        .create_roadmap(id, "A's roadmap", &crate::plan::Plan::default())
        .unwrap();
    store_b
        .create_roadmap(id, "B's roadmap", &crate::plan::Plan::default())
        .unwrap();

    // Neither clobbered the other: each workspace still sees its own.
    assert_eq!(
        store_a.load_roadmap(id).unwrap().unwrap().title,
        "A's roadmap",
        "workspace A's roadmap must survive B's import of the same id"
    );
    assert_eq!(
        store_b.load_roadmap(id).unwrap().unwrap().title,
        "B's roadmap"
    );
    // Each workspace lists exactly one.
    assert_eq!(store_a.list_roadmaps().unwrap().len(), 1);
    assert_eq!(store_b.list_roadmaps().unwrap().len(), 1);
}

/// Re-creating a roadmap with the SAME id in the SAME workspace still
/// overwrites in place (the intended `INSERT OR REPLACE` semantics), so the
/// fence does not break same-repo re-import.
#[test]
fn create_roadmap_overwrites_within_the_same_workspace() {
    let root = tempfile::TempDir::new().unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let id = "rm-1";
    store
        .create_roadmap(id, "first", &crate::plan::Plan::default())
        .unwrap();
    store
        .create_roadmap(id, "second", &crate::plan::Plan::default())
        .unwrap();
    assert_eq!(store.load_roadmap(id).unwrap().unwrap().title, "second");
    assert_eq!(store.list_roadmaps().unwrap().len(), 1);
}

/// The migration rebuilds a legacy id-only-PK `roadmaps` table into the
/// composite key, preserving rows, and is idempotent.
#[test]
fn migrate_roadmaps_pk_rebuilds_legacy_table_losslessly() {
    let dir = tempfile::TempDir::new().unwrap();
    let conn = Connection::open(dir.path().join("t.db")).unwrap();
    // Stand up the OLD schema (id-only PK) and a row.
    conn.execute_batch(
        "CREATE TABLE roadmaps (
                 id TEXT PRIMARY KEY,
                 workspace_key TEXT NOT NULL,
                 title TEXT NOT NULL DEFAULT '',
                 tree TEXT NOT NULL DEFAULT '',
                 schema_version INTEGER NOT NULL DEFAULT 1,
                 created_at_claim INTEGER NOT NULL DEFAULT 0,
                 updated_at_claim INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO roadmaps (id, workspace_key, title) VALUES ('x', 'wsA', 'kept');",
    )
    .unwrap();

    migrate_roadmaps_pk(&conn).unwrap();

    // The row survived and the PK is now composite.
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='roadmaps'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sql.to_ascii_lowercase()
            .contains("primary key (id, workspace_key)"),
        "PK must be composite after migration: {sql}"
    );
    let title: String = conn
        .query_row("SELECT title FROM roadmaps WHERE id='x'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "kept");

    // Composite key now admits the same id under a second workspace…
    conn.execute(
        "INSERT INTO roadmaps (id, workspace_key, title) VALUES ('x', 'wsB', 'other')",
        [],
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM roadmaps WHERE id='x'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 2, "same id can coexist across workspaces");

    // …and a second run is a no-op (idempotent).
    migrate_roadmaps_pk(&conn).unwrap();
    let n2: i64 = conn
        .query_row("SELECT COUNT(*) FROM roadmaps", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n2, 2);
}

/// A store opened on a db that already went through the migration (or was
/// created fresh, hence composite) leaves the table untouched.
#[test]
fn fresh_store_roadmaps_table_is_already_composite() {
    let root = tempfile::TempDir::new().unwrap();
    let ws = tempfile::TempDir::new().unwrap();
    let _store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let conn = Connection::open(root.path().join(DB_FILE)).unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='roadmaps'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql
        .to_ascii_lowercase()
        .contains("primary key (id, workspace_key)"));
}
