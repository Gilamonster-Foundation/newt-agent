use super::*;

// =========================================================================
// Part 4 — new in 17.2: workspace identity v2 (UUIDv5→v2 row migration)
// and the identity.pem writer-fingerprint upgrade. Cross-clone derivation
// against real `git` output lives in tests/workspace_key.rs.
// =========================================================================

/// The 17.2 open-time migration: rows keyed under THIS workspace's retired
/// UUIDv5 key are re-keyed to v2 exactly once; rows belonging to other
/// (unopened) workspaces are untouched; MRU order and the §6 chain survive;
/// and a second open is a no-op.
#[test]
fn uuidv5_rows_are_rekeyed_to_v2_once_and_foreign_rows_untouched() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    #[allow(deprecated)]
    let old_key = ConversationStore::workspace_id_for_path(ws.path()).unwrap();

    // Live history under the v2 key, MRU order: [first, second] by tick,
    // then an append makes `first` the most recent → list = [second, first].
    let (first, second, v2_key) = {
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let first = store.create("first", None).unwrap();
        store.append_turn(&first, "u1", "a1").unwrap();
        let second = store.create("second", None).unwrap();
        store.append_turn(&second, "u2", "a2").unwrap();
        store.append_turn(&first, "u3", "a3").unwrap();
        let conn = raw(root.path());
        let v2_key: String = conn
            .query_row(
                "SELECT workspace_key FROM conversations WHERE id = ?1",
                [&first],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(v2_key, old_key, "the store must key new rows with v2");
        (first, second, v2_key)
    };

    // Simulate a db written by 17.1: rewind this workspace's rows to the
    // UUIDv5 key, and plant a row belonging to ANOTHER workspace (also
    // UUIDv5-keyed — its workspace has not opened a 17.2 store yet).
    let foreign_ws = tempfile::tempdir().unwrap();
    #[allow(deprecated)]
    let foreign_key = ConversationStore::workspace_id_for_path(foreign_ws.path()).unwrap();
    {
        let conn = raw(root.path());
        conn.execute("UPDATE conversations SET workspace_key = ?1", [&old_key])
            .unwrap();
        conn.execute(
            "INSERT INTO conversations
               (id, title, workspace_path, workspace_key, persona, end_reason,
                writer_fingerprint, activity_tick, tip_hash,
                started_at_claim, updated_at_claim)
             VALUES ('foreign-conv', 'not yours', ?1, ?2, NULL, NULL,
                     'foreign-writer', 1, 'foreign-tip', 1, 1)",
            rusqlite::params![foreign_ws.path().to_string_lossy(), foreign_key],
        )
        .unwrap();
    }

    // Reopen: the migration re-keys this workspace's rows — and only them.
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        vec![second.clone(), first.clone()],
        "MRU order (least-recent first) must survive the migration"
    );
    store.verify_chain(&first).unwrap();
    store.verify_chain(&second).unwrap();
    assert!(
        store.load("foreign-conv").is_err(),
        "the foreign workspace's row must not leak into this scope"
    );
    {
        let conn = raw(root.path());
        let old_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE workspace_key = ?1",
                [&old_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            old_left, 0,
            "no row may still carry this workspace's old key"
        );
        let foreign_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE workspace_key = ?1",
                [&foreign_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            foreign_left, 1,
            "other workspaces migrate on THEIR open, not ours"
        );
    }
    drop(store);

    // Idempotence: a second open changes nothing (no old-key rows remain).
    let again = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let ids: Vec<String> = again.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![second.clone(), first.clone()]);
    again.verify_chain(&first).unwrap();
    {
        let conn = raw(root.path());
        let v2_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE workspace_key = ?1",
                [&v2_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v2_count, 2);
    }

    // And the foreign workspace's rows migrate when IT opens.
    let foreign_store = ConversationStore::new(root.path(), foreign_ws.path(), 100).unwrap();
    assert!(foreign_store.exists("foreign-conv").unwrap());
}

/// Legacy JSON records (whose bodies carry UUIDv5 keys) imported during the
/// same open are re-keyed too: import runs first, migration second.
#[test]
fn legacy_import_then_migration_rekeys_imported_rows_in_one_open() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let rec = legacy_record("8000-legacy", "from json", ws.path(), &[("u", "a")], 1, 2);
    write_legacy_record(root.path(), &rec);

    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert!(store.exists(&rec.id).unwrap(), "imported AND re-keyed");
    store.verify_chain(&rec.id).unwrap();
    let conn = raw(root.path());
    let stored_key: String = conn
        .query_row(
            "SELECT workspace_key FROM conversations WHERE id = ?1",
            [&rec.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(
        stored_key, rec.workspace_id,
        "the imported row must carry the v2 key, not the legacy UUIDv5"
    );
}

/// 17.2 writer identity: with `<root>/identity.pem` present, the writer
/// fingerprint IS the operator's mesh-key fingerprint — stable across
/// installs sharing that key — and history written under the old nonce
/// fingerprint still verifies (a writer handoff, which §6 supports).
#[test]
fn identity_pem_upgrades_writer_fingerprint_and_old_rows_still_verify() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();

    // Era 1: no identity — nonce-derived fingerprint writes some history.
    let (id, nonce_fp) = {
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let id = store.create("spans the upgrade", None).unwrap();
        store.append_turn(&id, "before", "upgrade").unwrap();
        (id, store.writer_fingerprint().to_string())
    };

    // Era 2: the operator mints an identity (newt-identity writes the same
    // file to ~/.newt/identity.pem in production).
    let user = agent_mesh_protocol::UserKey::generate();
    user.save(&root.path().join("identity.pem")).unwrap();

    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(
        store.writer_fingerprint(),
        user.fingerprint().hex(),
        "fingerprint must be the identity key's, not the nonce's"
    );
    assert_ne!(store.writer_fingerprint(), nonce_fp);

    // Old rows keep their recorded writer; appending as the new writer
    // extends the conversation with a second per-writer chain — both verify.
    store.append_turn(&id, "after", "upgrade").unwrap();
    store.verify_chain(&id).unwrap();
    assert_eq!(store.load(&id).unwrap().turns.len(), 2);
    let summaries = store.list().unwrap();
    assert_eq!(summaries.last().unwrap().id, id, "append still bumps MRU");

    // Stable per operator: a different install (root) with the same key
    // derives the same fingerprint.
    let other_root = tempfile::tempdir().unwrap();
    user.save(&other_root.path().join("identity.pem")).unwrap();
    let other = ConversationStore::new(other_root.path(), ws.path(), 100).unwrap();
    assert_eq!(other.writer_fingerprint(), store.writer_fingerprint());
}
