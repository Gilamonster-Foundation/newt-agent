use super::*;

// =========================================================================
// Part 3 — new in 17.1b: the one-time legacy JSON import, per-row
// encoding_version (review NIT N1 on #261), and byte-case-exact prefix
// resolution (NIT N5).
// =========================================================================

/// The 17.1b happy path: a multi-workspace legacy tree is imported once at
/// open — all workspaces, ticks assigned in legacy MRU order, the legacy
/// nanos surviving only as display claims, the chain verifying green — and
/// the tree is renamed to `conversations.imported/` as an untouched backup.
#[test]
fn legacy_import_happy_path_multi_workspace() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();

    // Workspace A: three conversations whose legacy MRU order (by
    // updated_at) is a3 < a1 < a2. a3 has no turns (created, never used).
    let a1 = legacy_record(
        "1000-conv-a1",
        "first task",
        ws_a.path(),
        &[("write the parser", "parser written"), ("run tests", "ok")],
        100,
        500,
    );
    let a2 = legacy_record(
        "2000-conv-a2",
        "second task",
        ws_a.path(),
        &[("fix the bug", "fixed")],
        200,
        900,
    );
    let a3 = legacy_record("3000-conv-a3", "empty", ws_a.path(), &[], 300, 300);
    // Workspace B: one conversation, discovered via ITS dir even though the
    // opening store is scoped to workspace A.
    let b1 = legacy_record(
        "4000-conv-b1",
        "b's task",
        ws_b.path(),
        &[("hello", "world")],
        50,
        60,
    );
    for record in [&a1, &a2, &a3, &b1] {
        write_legacy_record(root.path(), record);
    }

    // Opening a store scoped to workspace A imports EVERYTHING under the root.
    let store = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();

    // Workspace A sees its three conversations in legacy MRU order (list is
    // least-recently-active first; ticks were assigned in import order).
    let summaries = store.list().unwrap();
    let ids: Vec<_> = summaries.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![a3.id.as_str(), a1.id.as_str(), a2.id.as_str()]);

    // The legacy nanos came through as display claims…
    let restored = store.load(&a1.id).unwrap();
    assert_eq!(restored.created_at_unix_nanos, 100);
    assert_eq!(restored.updated_at_unix_nanos, 500);
    assert_eq!(restored.title, "first task");
    assert_eq!(restored.persona.as_deref(), Some("coder"));
    assert_eq!(restored.turns, a1.turns, "turn order = legacy vec order");

    // …and ordering is the tick, with the per-turn ts_claim carrying the
    // record-level updated_at (the only claim the legacy format kept).
    let conn = raw(root.path());
    let tick = |id: &str| -> i64 {
        conn.query_row(
            "SELECT activity_tick FROM conversations WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(tick(&a3.id) < tick(&a1.id), "import order assigns ticks");
    assert!(tick(&a1.id) < tick(&a2.id), "import order assigns ticks");
    let ts_claims: Vec<i64> = conn
        .prepare("SELECT ts_claim FROM turns WHERE conversation_id = ?1")
        .unwrap()
        .query_map([&a1.id], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(ts_claims, vec![500, 500]);

    // The chain was built genesis-up during import: every conversation
    // verifies, including the turn-less one.
    for id in [&a1.id, &a2.id, &a3.id] {
        store.verify_chain(id).expect("imported chain must verify");
    }

    // Workspace B's conversation imported into ITS workspace.
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
    let b_list = store_b.list().unwrap();
    assert_eq!(b_list.len(), 1);
    assert_eq!(b_list[0].id, b1.id);
    assert_eq!(store_b.load(&b1.id).unwrap().turns, b1.turns);
    store_b.verify_chain(&b1.id).unwrap();
    // And it is invisible to workspace A (scoping survived the import).
    assert!(store.load(&b1.id).is_err());

    // Non-destructive: the legacy tree was renamed, not deleted — every
    // original file is intact in the backup.
    assert!(!root.path().join("conversations").exists());
    let backup = root.path().join("conversations.imported");
    for record in [&a1, &a2, &a3, &b1] {
        let path = backup
            .join(&record.workspace_id)
            .join(format!("{}.json", record.id));
        assert!(path.is_file(), "backup must keep {}", path.display());
    }

    // Imported history keeps working through the normal API.
    store.append_turn(&a1.id, "post-import", "works").unwrap();
    store.verify_chain(&a1.id).unwrap();
    assert_eq!(store.load(&a1.id).unwrap().turns.len(), 3);
}

/// Corrupt legacy records are skipped with a warning (the legacy store's own
/// semantics) — and survive untouched in the backup dir. A record sitting in
/// the wrong workspace dir (invisible to the legacy store) is skipped too.
#[test]
fn legacy_import_skips_corrupt_and_misfiled_records() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let good = legacy_record("5000-good", "good", ws.path(), &[("a", "b")], 1, 2);
    write_legacy_record(root.path(), &good);

    let ws_dir = root.path().join("conversations").join(&good.workspace_id);
    std::fs::write(ws_dir.join("9999999999-corrupt.json"), "{not json at all").unwrap();
    // Misfiled: body claims another workspace than the dir it sits in. The
    // legacy store could never load this record; the import must not
    // resurrect it.
    let other_ws = tempfile::tempdir().unwrap();
    let misfiled = legacy_record("6000-misfiled", "ghost", other_ws.path(), &[], 1, 2);
    std::fs::write(
        ws_dir.join("6000-misfiled.json"),
        serde_json::to_string_pretty(&misfiled).unwrap(),
    )
    .unwrap();

    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let summaries = store.list().unwrap();
    assert_eq!(summaries.len(), 1, "only the good record imports");
    assert_eq!(summaries[0].id, good.id);
    store.verify_chain(&good.id).unwrap();

    // Both skipped files are preserved in the backup for forensics.
    let backup_ws = root
        .path()
        .join("conversations.imported")
        .join(&good.workspace_id);
    assert!(backup_ws.join("9999999999-corrupt.json").is_file());
    assert!(backup_ws.join("6000-misfiled.json").is_file());
}

/// Idempotence: a second open finds nothing to import (the dir was renamed),
/// and even a manually restored copy of the legacy tree re-imports nothing —
/// existing ids are skipped, never overwritten.
#[test]
fn legacy_import_is_idempotent_and_never_overwrites() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let rec = legacy_record(
        "7000-once",
        "import me once",
        ws.path(),
        &[("u", "a")],
        1,
        2,
    );
    write_legacy_record(root.path(), &rec);

    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(store.list().unwrap().len(), 1);
    drop(store);

    // Second open: legacy dir is gone, backup intact, nothing re-imported.
    let again = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(again.list().unwrap().len(), 1);
    assert_eq!(again.load(&rec.id).unwrap().turns.len(), 1);
    let backup = root.path().join("conversations.imported");
    assert!(backup.is_dir(), "backup must survive subsequent opens");
    drop(again);

    // Someone restores the backup (e.g. out of caution after the migration):
    // the records inside carry already-imported ids, so the import skips
    // every one — no duplicates, no overwrites — and the restored copy is
    // itself retired under a suffixed backup name.
    let restored_ws = root.path().join("conversations").join(&rec.workspace_id);
    std::fs::create_dir_all(&restored_ws).unwrap();
    std::fs::copy(
        backup
            .join(&rec.workspace_id)
            .join(format!("{}.json", rec.id)),
        restored_ws.join(format!("{}.json", rec.id)),
    )
    .unwrap();

    let third = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(third.list().unwrap().len(), 1, "no duplicate conversations");
    assert_eq!(
        third.load(&rec.id).unwrap().turns.len(),
        1,
        "no duplicate turns"
    );
    third.verify_chain(&rec.id).unwrap();
    assert!(
        !root.path().join("conversations").exists(),
        "restored copy retired again"
    );
    assert!(root.path().join("conversations.imported.1").is_dir());
}

/// N1 (#261): every turn row records its encoding version, and
/// `verify_chain` refuses a version it does not understand with a clear
/// error instead of hashing under the wrong rules. The recorded value moved
/// 1 → 2 at the #1786 bump — the one expectation in this suite that
/// legitimately moves with an encoding epoch (the pinned byte vectors in
/// turn_chain.rs are the expectations that never may).
#[test]
fn turns_carry_encoding_version_and_unknown_versions_error_clearly() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("versioned", None).unwrap();
    store.append_turn(&id, "one", "1").unwrap();
    store.append_turn(&id, "two", "2").unwrap();

    let conn = raw(root.path());
    let versions: Vec<i64> = conn
        .prepare("SELECT encoding_version FROM turns WHERE conversation_id = ?1")
        .unwrap()
        .query_map([&id], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        versions,
        vec![2, 2],
        "the current epoch (v2) is recorded per row"
    );

    // A row claiming a future encoding version: verification must error
    // clearly (not report a bogus chain violation from hashing v1-style),
    // and appends must refuse to extend a chain they cannot hash.
    conn.execute(
        "UPDATE turns SET encoding_version = 99 WHERE conversation_id = ?1
           AND seq = (SELECT MAX(seq) FROM turns WHERE conversation_id = ?1)",
        [&id],
    )
    .unwrap();
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("encoding_version 99") && err.contains("known: 1"),
        "unknown version must be named clearly: {err}"
    );
    let append_err = store
        .append_turn(&id, "more", "no")
        .unwrap_err()
        .to_string();
    assert!(append_err.contains("encoding_version 99"), "{append_err}");
}

/// N5 (#261): prefix resolution is byte-case-exact, as the JSON backend's
/// `starts_with` was. (The 17.1a `LIKE` port was ASCII-case-insensitive:
/// these two ids would have been ambiguous for BOTH prefixes below.)
#[test]
fn prefix_resolution_is_byte_case_exact() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    store
        .create_with_id("CaSe-Sensitive-AAA", "upper", None)
        .unwrap();
    store
        .create_with_id("case-sensitive-bbb", "lower", None)
        .unwrap();

    assert_eq!(store.resolve_id("CaSe").unwrap(), "CaSe-Sensitive-AAA");
    assert_eq!(store.resolve_id("case").unwrap(), "case-sensitive-bbb");
    let err = store.resolve_id("CASE").unwrap_err().to_string();
    assert!(
        err.contains("not found"),
        "no id starts with CASE byte-exactly: {err}"
    );
}

/// R2 (adversarial review of #261): `create_with_id` must refuse to overwrite
/// a conversation that belongs to ANOTHER workspace — `id` is a global PK and
/// REPLACE would cascade-delete the other workspace's turns.
#[test]
fn create_with_id_refuses_cross_workspace_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

    let id = new_conversation_id();
    store_a
        .create_with_id(&id, "workspace A's conversation", None)
        .unwrap();
    store_a.append_turn(&id, "precious", "history").unwrap();

    // Workspace B tries to create the same id: must bail, not REPLACE.
    let err = store_b
        .create_with_id(&id, "hijack", None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("another workspace"), "got: {err}");

    // A's conversation and its turns are intact.
    let record = store_a.load(&id).unwrap();
    assert_eq!(record.title, "workspace A's conversation");
    assert_eq!(record.turns.len(), 1);
    store_a.verify_chain(&id).unwrap();

    // Same-workspace re-create keeps JSON-backend parity (overwrite allowed).
    store_a.create_with_id(&id, "recreated", None).unwrap();
    assert_eq!(store_a.load(&id).unwrap().turns.len(), 0);
}

/// R1 (adversarial review of #261): concurrent first-run nonce minting must
/// converge every racer on ONE writer fingerprint.
#[test]
fn concurrent_nonce_mint_converges_on_one_fingerprint() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let root = root.path().to_path_buf();
        let ws = ws.path().to_path_buf();
        handles.push(std::thread::spawn(move || {
            let store = ConversationStore::new(&root, &ws, 100).unwrap();
            store.writer_fingerprint().to_string()
        }));
    }
    let fps: std::collections::HashSet<String> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(fps.len(), 1, "all racers must adopt one identity: {fps:?}");
}
