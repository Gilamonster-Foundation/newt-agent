//! SQLite `ConversationStore` suite — Phase 17.1a/17.1b (issue #246).
//!
//! Part 1 ports the retired JSON backend's suite unchanged semantically
//! (the backend swap must be invisible through the public API; the two
//! storage-format-specific tests are ported to their SQLite analogues),
//! plus the shared free-function tests that moved here when 17.1b deleted
//! tests/conversation_store.rs. Part 2 covers what is new in 17.1a: §6
//! causal ordering (MRU = activity tick, never a timestamp), the clock-skew
//! case, BLAKE3 chain integrity and tamper detection, two-writer
//! `busy_timeout` concurrency, and the schema-diff migration. Part 3 covers
//! 17.1b: the one-time legacy JSON import, per-row `encoding_version` (N1),
//! and byte-case-exact prefix resolution (N5).

use newt_core::{
    new_conversation_id, session_plan_dir, session_plan_path, ConversationRecord, ConversationTurn,
    PhantomReach, PhantomResolution, ToolEvent,
};
// The canonical (root re-exported) store IS the SQLite backend as of 17.1a.
use newt_core::ConversationStore;

fn db_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("conversations.db")
}

/// Open the store's database directly — the tests' tamper/skew/inspect hatch.
fn raw(root: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db_path(root)).unwrap()
}

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
        std::path::Path::new(".newt/sessions/abc-123/plan.md"),
    );
    assert_eq!(
        session_plan_dir("abc-123"),
        std::path::Path::new(".newt/sessions/abc-123"),
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

// =========================================================================
// Part 2 — new in 17.1a: §6 ordering, chain integrity, concurrency,
// migration, WAL fallback surface.
// =========================================================================

/// §6: MRU is the activity tick (max per-writer seq), never a timestamp.
/// Skew the display claims as adversarially as we like — ordering must not
/// move, because no ordering query reads a `*_claim` column.
#[test]
fn mru_is_activity_tick_not_timestamp() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let a = store.create("a", None).unwrap();
    let b = store.create("b", None).unwrap();
    store.append_turn(&a, "later activity", "yes").unwrap(); // a is now MRU

    // Forge the display claims: make `b` look written far in the future and
    // `a` in the distant past.
    let conn = raw(root.path());
    conn.execute(
        "UPDATE conversations SET updated_at_claim = 9000000000000000000 WHERE id = ?1",
        [&b],
    )
    .unwrap();
    conn.execute(
        "UPDATE conversations SET updated_at_claim = 1 WHERE id = ?1",
        [&a],
    )
    .unwrap();

    let ids: Vec<_> = store.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        vec![b.clone(), a.clone()],
        "MRU (= last in list) must be `a` by activity tick, claims be damned"
    );

    // And the prune victim is picked by tick, not claim: cap 2 must evict
    // `b` (lowest tick) even though its claim says "newest".
    let tight = ConversationStore::new(root.path(), workspace.path(), 2).unwrap();
    let c = tight.create("c", None).unwrap();
    let survivors: Vec<_> = tight.list().unwrap().into_iter().map(|s| s.id).collect();
    assert!(survivors.contains(&c));
    assert!(survivors.contains(&a), "max-tick conversation must survive");
    assert!(!survivors.contains(&b), "low-tick conversation pruned");
}

/// §6 clock-skew test: the wall clock runs BACKWARDS mid-conversation (the
/// store itself honestly records skewed claims through the normal API).
/// Turn order, MRU, and the content chain must all be unaffected.
#[test]
fn clock_skew_mid_conversation_does_not_affect_ordering() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let early = store.create("skewed", None).unwrap();
    let other = store.create("other", None).unwrap();
    store.append_turn(&early, "turn 1", "one").unwrap();
    store.append_turn(&early, "turn 2", "two").unwrap();

    // The system clock jumps an hour into the past…
    store.set_claim_clock_for_test(|| 1_000);
    store.append_turn(&early, "turn 3", "three").unwrap();
    store.append_turn(&early, "turn 4", "four").unwrap();

    // …and turn order is still append order (per-writer seq, not ts_claim).
    let record = store.load(&early).unwrap();
    let users: Vec<_> = record.turns.iter().map(|t| t.user.as_str()).collect();
    assert_eq!(users, vec!["turn 1", "turn 2", "turn 3", "turn 4"]);

    // "Latest" is unaffected: `early` out-ticks `other` even though its
    // claims now say it was last touched at nanosecond 1000.
    let ids: Vec<_> = store.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![other, early.clone()]);

    // Honest skew is not tampering: the chain still verifies.
    store.verify_chain(&early).unwrap();

    // The skewed claim is faithfully *displayed* (it is a claim, after all).
    let summaries = store.list().unwrap();
    let skewed = summaries.iter().find(|s| s.id == early).unwrap();
    assert_eq!(skewed.updated_at_unix_nanos, 1_000);
}

/// §6 target — content-chained turns: prev_hash links verify, and a
/// tampered row (here: the assistant text edited behind the store's back)
/// is detectable.
#[test]
fn chain_integrity_detects_a_tampered_row() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("chained", None).unwrap();
    store.append_turn(&id, "one", "1").unwrap();
    store.append_turn(&id, "two", "2").unwrap();
    store.append_turn(&id, "three", "3").unwrap();
    store.verify_chain(&id).expect("untampered chain verifies");

    // Tamper with the middle turn's content directly in the db.
    let conn = raw(root.path());
    let changed = conn
        .execute(
            "UPDATE turns SET assistant = 'doctored' WHERE conversation_id = ?1
               AND seq = (SELECT MIN(seq) + 1 FROM turns WHERE conversation_id = ?1)",
            [&id],
        )
        .unwrap();
    assert_eq!(changed, 1);

    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation"),
        "tampering must break the chain: {err}"
    );

    // Tampering with the LAST row is caught too (by the stored tip hash).
    let root2 = tempfile::tempdir().unwrap();
    let store2 = ConversationStore::new(root2.path(), workspace.path(), 100).unwrap();
    let id2 = store2.create("tip", None).unwrap();
    store2.append_turn(&id2, "only", "turn").unwrap();
    raw(root2.path())
        .execute(
            "UPDATE turns SET assistant = 'doctored' WHERE conversation_id = ?1",
            [&id2],
        )
        .unwrap();
    let err2 = store2.verify_chain(&id2).unwrap_err().to_string();
    assert!(err2.contains("chain violation"), "{err2}");
}

/// Even display claims are tamper-evident: ts_claim participates in the
/// canonical encoding, so editing history's *timestamps* breaks the chain
/// (while never affecting ordering, which doesn't read them).
#[test]
fn chain_integrity_detects_tampered_claims() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("claims", None).unwrap();
    store.append_turn(&id, "one", "1").unwrap();
    store.append_turn(&id, "two", "2").unwrap();

    raw(root.path())
        .execute(
            "UPDATE turns SET ts_claim = 42 WHERE conversation_id = ?1
               AND seq = (SELECT MIN(seq) FROM turns WHERE conversation_id = ?1)",
            [&id],
        )
        .unwrap();

    // Ordering still fine — nothing orders by ts_claim…
    let record = store.load(&id).unwrap();
    assert_eq!(record.turns.len(), 2);
    assert_eq!(record.turns[0].user, "one");
    // …but the forged claim is detectable.
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(err.contains("chain violation"), "{err}");
}

/// Two stores (two connections — i.e. two newt processes) appending to the
/// same conversation concurrently: busy_timeout + BEGIN IMMEDIATE must
/// serialize them with zero failures, ticks stay strictly monotonic and
/// unique, and the chain stays intact.
#[test]
fn concurrent_appends_from_two_stores_share_the_db() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    assert_eq!(
        store_a.writer_fingerprint(),
        store_b.writer_fingerprint(),
        "same install nonce → same writer"
    );

    let id = store_a.create("contended", None).unwrap();
    const PER_WRITER: usize = 25;

    let id_a = id.clone();
    let id_b = id.clone();
    let t_a = std::thread::spawn(move || {
        for i in 0..PER_WRITER {
            store_a
                .append_turn(&id_a, &format!("a{i}"), "ok")
                .expect("writer A must never hit SQLITE_BUSY");
        }
        store_a
    });
    let t_b = std::thread::spawn(move || {
        for i in 0..PER_WRITER {
            store_b
                .append_turn(&id_b, &format!("b{i}"), "ok")
                .expect("writer B must never hit SQLITE_BUSY");
        }
        store_b
    });
    let store_a = t_a.join().unwrap();
    let _ = t_b.join().unwrap();

    let record = store_a.load(&id).unwrap();
    assert_eq!(record.turns.len(), 2 * PER_WRITER);

    // Ticks: unique and strictly increasing (the §6 floor).
    let conn = raw(root.path());
    let mut stmt = conn
        .prepare("SELECT seq FROM turns WHERE conversation_id = ?1 ORDER BY seq ASC")
        .unwrap();
    let seqs: Vec<i64> = stmt
        .query_map([&id], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(seqs.len(), 2 * PER_WRITER);
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "per-writer ticks must be strictly monotonic: {seqs:?}"
    );

    // And the interleaved chain still verifies end to end.
    store_a.verify_chain(&id).unwrap();
}

/// Schema-diff reconciliation: opening a database created by an older
/// 17.1a (missing the `end_reason`, `events`, and token columns, and the
/// writer_clock table entirely) adds the missing columns and keeps the
/// existing data usable — and the re-seeded clock cannot reuse ticks.
#[test]
fn opening_a_drifted_schema_adds_missing_columns() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace_path = std::fs::canonicalize(workspace.path()).unwrap();
    // A db written by an older newt carries v1 (UUIDv5) workspace keys; the
    // 17.2 open-time migration re-keys them, so the drifted row stays
    // visible through the store after reconciliation.
    #[allow(deprecated)]
    let workspace_key = ConversationStore::workspace_id_for_path(workspace.path()).unwrap();

    // Hand-build the "v1" database: additive drift only (no end_reason on
    // conversations; no events/tokens on turns; no writer_clock at all).
    std::fs::create_dir_all(root.path()).unwrap();
    {
        let conn = rusqlite::Connection::open(db_path(root.path())).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
                 id TEXT PRIMARY KEY, title TEXT NOT NULL,
                 workspace_path TEXT NOT NULL, workspace_key TEXT NOT NULL,
                 persona TEXT,
                 writer_fingerprint TEXT NOT NULL, activity_tick INTEGER NOT NULL,
                 tip_hash TEXT NOT NULL,
                 started_at_claim INTEGER NOT NULL, updated_at_claim INTEGER NOT NULL
             );
             CREATE TABLE turns (
                 conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                 writer_fingerprint TEXT NOT NULL, seq INTEGER NOT NULL,
                 prev_hash TEXT NOT NULL, user TEXT NOT NULL, assistant TEXT NOT NULL,
                 ts_claim INTEGER NOT NULL,
                 PRIMARY KEY (conversation_id, writer_fingerprint, seq)
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations
               (id, title, workspace_path, workspace_key, persona, writer_fingerprint,
                activity_tick, tip_hash, started_at_claim, updated_at_claim)
             VALUES ('legacy-conv', 'from v1', ?1, ?2, NULL, 'old-writer', 7, '', 1, 1)",
            rusqlite::params![workspace_path.to_string_lossy(), workspace_key],
        )
        .unwrap();
    }

    // Opening the store reconciles the schema…
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let conn = raw(root.path());
    for (table, column) in [
        ("conversations", "end_reason"),
        // #713 / #715: the working-memory snapshot columns are additive too —
        // an older db (this hand-built v1) gains them on open.
        ("conversations", "scratchpad"),
        ("conversations", "plan"),
        ("turns", "events"),
        ("turns", "tokens_in"),
        ("turns", "tokens_out"),
    ] {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            cols.iter().any(|c| c == column),
            "{table}.{column} must be added by reconciliation; have {cols:?}"
        );
    }

    // …the v1 row is fully usable…
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "legacy-conv");
    let legacy = store.load("legacy-conv").unwrap();
    assert_eq!(legacy.title, "from v1");
    // #715: the back-filled `plan` column's `{}` default decodes to an empty
    // snapshot (load would have errored on invalid JSON) — proving the additive
    // column is loadable, not garbage, on an older db.
    assert!(legacy.plan.is_empty(), "back-filled plan parses empty");

    // …and new activity ticks past the drifted data's max (the rebuilt
    // writer_clock seeds from existing rows; monotonicity survives).
    store
        .append_turn("legacy-conv", "post-migration", "ok")
        .unwrap();
    let new_conv = store.create("fresh", None).unwrap();
    let max_seq: i64 = conn
        .query_row("SELECT MAX(activity_tick) FROM conversations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(max_seq > 7, "new ticks must continue past drifted data");
    store.load(&new_conv).unwrap();

    // Chain verification works on the migrated conversation after the first
    // post-migration append (review finding N2 on #261: the tip check is
    // writer-agnostic — it follows the conversation row's recorded writer,
    // so foreign/migrated history doesn't spuriously fail).
    store
        .verify_chain("legacy-conv")
        .expect("migrated conversation must verify after a post-migration append");
}

/// On a healthy local filesystem WAL applies cleanly: no fallback notice.
/// (The NFS failure itself can't be simulated portably in CI; the error
/// classifier has unit tests in src/store.rs.)
#[test]
fn wal_applies_on_local_filesystems_with_no_fallback_notice() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    assert_eq!(store.wal_fallback_notice(), None);

    let conn = raw(root.path());
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}

/// §6: a rename is metadata, not activity — it must not perturb MRU order
/// (the old backend's rename-bumps-`updated_at` defect is dissolved, design
/// doc §1).
#[test]
fn rename_does_not_bump_mru_order() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let first = store.create("first", None).unwrap();
    let second = store.create("second", None).unwrap();
    store.rename(&first, "renamed first").unwrap();

    let ids: Vec<_> = store.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        vec![first.clone(), second],
        "rename must not move `first` to the MRU slot"
    );
    assert_eq!(store.load(&first).unwrap().title, "renamed first");
}

/// The store is cheap to clone and clones share state — the TUI clones it
/// across the session/agentic-loop boundary.
#[test]
fn clones_share_the_same_database() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let clone = store.clone();

    let id = store.create("shared", None).unwrap();
    clone.append_turn(&id, "via clone", "ok").unwrap();
    assert_eq!(store.load(&id).unwrap().turns.len(), 1);
}

// =========================================================================
// Part 3 — new in 17.1b: the one-time legacy JSON import, per-row
// encoding_version (review NIT N1 on #261), and byte-case-exact prefix
// resolution (NIT N5).
// =========================================================================

/// Write one legacy-format record exactly where the JSON backend kept it:
/// `<root>/conversations/<workspace_id>/<id>.json`, pretty-printed.
fn write_legacy_record(root: &std::path::Path, record: &ConversationRecord) {
    let dir = root.join("conversations").join(&record.workspace_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.json", record.id)),
        serde_json::to_string_pretty(record).unwrap(),
    )
    .unwrap();
}

fn legacy_record(
    id: &str,
    title: &str,
    workspace: &std::path::Path,
    turns: &[(&str, &str)],
    created: u128,
    updated: u128,
) -> ConversationRecord {
    // Legacy records carry the retired v1 (UUIDv5) key in their body and
    // dir name — that is the format under test.
    #[allow(deprecated)]
    let workspace_id = ConversationStore::workspace_id_for_path(workspace).unwrap();
    ConversationRecord {
        id: id.to_string(),
        title: title.to_string(),
        workspace: workspace.to_string_lossy().into_owned(),
        workspace_id,
        persona: Some("coder".to_string()),
        turns: turns
            .iter()
            .map(|(u, a)| ConversationTurn::new(*u, *a))
            .collect(),
        scratchpad: std::collections::BTreeMap::new(),
        plan: newt_core::PlanSnapshot::default(),
        created_at_unix_nanos: created,
        updated_at_unix_nanos: updated,
    }
}

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

/// N1 (#261): every turn row records its encoding version (only v1 exists),
/// and `verify_chain` refuses a version it does not understand with a clear
/// error instead of hashing under the wrong rules.
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
    assert_eq!(versions, vec![1, 1], "v1 is recorded per row");

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

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let len = a
        .bytes()
        .zip(b.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    assert!(len > 0, "test ids should share the unix timestamp prefix");
    &a[..len]
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

// =========================================================================
// Part 5 — new in 17.3: the FTS5 recall index (trigger maintenance,
// backfill-on-migration, workspace fencing, ranking/snippet shape, the
// events seam for 17.6, and adversarial queries end to end).
// =========================================================================

/// Trigger maintenance, both directions: an appended turn is immediately
/// searchable (AFTER INSERT), and deleting the conversation removes its
/// hits (AFTER DELETE via the FK cascade).
#[test]
fn fts_appends_are_searchable_and_conversation_delete_removes_them() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("indexing test", None).unwrap();
    store
        .append_turn(
            &id,
            "please fix the tokenizer bug",
            "tokenizer fixed and tests added",
        )
        .unwrap();

    let hits = store.search("tokenizer", 10).unwrap();
    assert_eq!(hits.len(), 1, "one matching turn → one hit");
    assert_eq!(hits[0].conversation_id, id);
    assert_eq!(hits[0].title, "indexing test");
    assert!(hits[0].seq > 0, "seq is the turn's §6 tick");
    assert!(
        hits[0].snippet.contains(">>>tokenizer<<<"),
        "snippet must mark the match: {}",
        hits[0].snippet
    );
    assert!(
        hits[0].rank < 0.0,
        "bm25 scores are negative: {}",
        hits[0].rank
    );

    // Matches in the user half are found too.
    assert_eq!(store.search("please", 10).unwrap().len(), 1);

    store.delete(&id).unwrap();
    assert!(
        store.search("tokenizer", 10).unwrap().is_empty(),
        "the conversation-delete cascade must clear the index"
    );
}

/// The one-time legacy JSON import writes turns through the normal insert
/// path — the trigger indexes imported history with no extra pass.
#[test]
fn fts_indexes_legacy_imported_turns() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let rec = legacy_record(
        "9000-imported",
        "old times",
        ws.path(),
        &[("remember the quokka incident", "documented it")],
        1,
        2,
    );
    write_legacy_record(root.path(), &rec);

    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    let hits = store.search("quokka", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].conversation_id, rec.id);
}

/// Workspace fencing: a hit in workspace A is never returned to workspace B,
/// even though both share one database and one FTS index.
#[test]
fn fts_search_is_workspace_fenced() {
    let root = tempfile::tempdir().unwrap();
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
    let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

    let id_a = store_a.create("a's secret", None).unwrap();
    store_a
        .append_turn(&id_a, "the zanzibar rollout plan", "drafted")
        .unwrap();
    let id_b = store_b.create("b's own", None).unwrap();
    store_b
        .append_turn(&id_b, "unrelated work", "done")
        .unwrap();

    let a_hits = store_a.search("zanzibar", 10).unwrap();
    assert_eq!(a_hits.len(), 1);
    assert_eq!(a_hits[0].conversation_id, id_a);
    assert!(
        store_b.search("zanzibar", 10).unwrap().is_empty(),
        "workspace B must never see A's turns"
    );
}

/// Backfill-on-migration: a database written by a pre-17.3 newt (no FTS
/// objects at all) opens, gains the index + triggers, and its existing
/// turns — including events-derived columns — become searchable. A second
/// open is a no-op (presence of the table = done): same single hit, no
/// duplicates.
#[test]
fn fts_backfills_pre_fts_databases_once() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let workspace_path = std::fs::canonicalize(ws.path()).unwrap();
    // A pre-17.3 db carries pre-17.2 (UUIDv5) keys in the general case;
    // the open-time migration re-keys them before any search can run.
    #[allow(deprecated)]
    let old_key = ConversationStore::workspace_id_for_path(ws.path()).unwrap();

    // Hand-build the 17.1-shaped database: current tables, NO fts objects.
    std::fs::create_dir_all(root.path()).unwrap();
    {
        let conn = rusqlite::Connection::open(db_path(root.path())).unwrap();
        conn.execute_batch(
            "CREATE TABLE conversations (
                 id TEXT PRIMARY KEY, title TEXT NOT NULL,
                 workspace_path TEXT NOT NULL, workspace_key TEXT NOT NULL,
                 persona TEXT, end_reason TEXT,
                 writer_fingerprint TEXT NOT NULL, activity_tick INTEGER NOT NULL,
                 tip_hash TEXT NOT NULL,
                 started_at_claim INTEGER NOT NULL, updated_at_claim INTEGER NOT NULL
             );
             CREATE TABLE turns (
                 conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                 writer_fingerprint TEXT NOT NULL, seq INTEGER NOT NULL,
                 prev_hash TEXT NOT NULL, user TEXT NOT NULL, assistant TEXT NOT NULL,
                 events TEXT NOT NULL DEFAULT '[]',
                 tokens_in INTEGER, tokens_out INTEGER,
                 ts_claim INTEGER NOT NULL,
                 encoding_version INTEGER NOT NULL DEFAULT 1,
                 PRIMARY KEY (conversation_id, writer_fingerprint, seq)
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations
               (id, title, workspace_path, workspace_key, persona, end_reason,
                writer_fingerprint, activity_tick, tip_hash, started_at_claim, updated_at_claim)
             VALUES ('pre-fts-conv', 'from before recall', ?1, ?2, NULL, NULL,
                     'old-writer', 2, '', 1, 1)",
            rusqlite::params![workspace_path.to_string_lossy(), old_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO turns VALUES
               ('pre-fts-conv', 'old-writer', 1, '', 'the wombat deployment failed',
                'rolled it back', '[]', NULL, NULL, 1, 1),
               ('pre-fts-conv', 'old-writer', 2, '', 'retry it', 'done',
                '[{\"tool\":\"chat-send\",\"args_digest\":\"channel=#ops\"}]', NULL, NULL, 1, 1)",
            [],
        )
        .unwrap();
    }

    // First 17.3 open: index created + backfilled in one transaction.
    {
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let hits = store.search("wombat", 10).unwrap();
        assert_eq!(hits.len(), 1, "pre-FTS turns must be searchable");
        assert_eq!(hits[0].conversation_id, "pre-fts-conv");
        assert_eq!(hits[0].title, "from before recall");
        // Backfill derives the events columns too — the 17.6 seam applies
        // to history, not just new appends.
        assert_eq!(store.search("chat-send", 10).unwrap().len(), 1);
    }

    // Second open: idempotent — still exactly one hit each, no duplicates.
    let again = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(again.search("wombat", 10).unwrap().len(), 1);
    assert_eq!(again.search("chat-send", 10).unwrap().len(), 1);
    // And the live write path is wired in the migrated db.
    again
        .append_turn("pre-fts-conv", "also index the axolotl", "indexed")
        .unwrap();
    assert_eq!(again.search("axolotl", 10).unwrap().len(), 1);
}

/// Ranking sanity: bm25 puts the turn where the term is exact-and-dense
/// above one where it is buried in noise; hits arrive best-first; `limit`
/// truncates from the bottom. A quoted phrase matches only the turn with
/// the exact adjacent words, not scattered mentions.
#[test]
fn fts_ranking_prefers_exact_dense_matches_over_scattered() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let dense = store.create("dense", None).unwrap();
    store
        .append_turn(&dense, "kraken kraken status", "the kraken is released")
        .unwrap();
    let scattered = store.create("scattered", None).unwrap();
    store
        .append_turn(
            &scattered,
            "long unrelated discussion about build pipelines caching tokens \
             models budgets and somewhere in the middle a kraken appears once \
             before more words about pipelines caching and budgets trail off",
            "noted",
        )
        .unwrap();

    let hits = store.search("kraken", 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].conversation_id, dense,
        "dense match must rank first"
    );
    assert!(
        hits[0].rank <= hits[1].rank,
        "hits must arrive best-first: {} vs {}",
        hits[0].rank,
        hits[1].rank
    );
    // limit truncates from the bottom of the ranking.
    let top = store.search("kraken", 1).unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].conversation_id, dense);

    // Phrase query: only the exact adjacent words match.
    let phrase = store.search("\"kraken is released\"", 10).unwrap();
    assert_eq!(phrase.len(), 1);
    assert_eq!(phrase[0].conversation_id, dense);
}

/// Snippet shape: a match deep inside a long turn comes back as a short
/// excerpt — match marked `>>> <<<`, `…` at the trimmed edges — never the
/// full turn content.
#[test]
fn fts_snippet_is_a_marked_excerpt_not_the_full_turn() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
                  eiusmod tempor incididunt ut labore et dolore magna aliqua "
        .repeat(4);
    let long_text = format!("{filler} the platypus hides here {filler}");
    let id = store.create("haystack", None).unwrap();
    store.append_turn(&id, "question", &long_text).unwrap();

    let hits = store.search("platypus", 10).unwrap();
    assert_eq!(hits.len(), 1);
    let snippet = &hits[0].snippet;
    assert!(snippet.contains(">>>platypus<<<"), "{snippet}");
    assert!(
        snippet.contains('…'),
        "trimmed edges must show ellipses: {snippet}"
    );
    assert!(
        snippet.len() < long_text.len() / 4,
        "snippet must be an excerpt ({} chars of {})",
        snippet.len(),
        long_text.len()
    );
}

/// The 17.6 seam, proven end to end: a turn whose `events` JSON carries
/// tool entries (hand-inserted — nothing records events until 17.6) gets
/// its tool names and args digests indexed by the trigger, searchable
/// through the same API, with the snippet drawn from the derived column.
#[test]
fn fts_events_derived_columns_light_up_when_events_arrive() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("tool runs", None).unwrap();
    store.append_turn(&id, "seed turn", "ok").unwrap();

    // Hand-insert a turn carrying events, exactly as 17.6 will write them.
    let writer = store.writer_fingerprint().to_string();
    raw(root.path())
        .execute(
            "INSERT INTO turns
               (conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                events, tokens_in, tokens_out, ts_claim, encoding_version)
             VALUES (?1, ?2, 9999, 'x', 'run the deploy', 'deployed',
                     '[{\"tool\":\"chat-send\",\"args_digest\":\"target=ops channel=#general\"},
                       {\"tool\":\"file-read\",\"args_digest\":\"path=src/store.rs\"}]',
                     NULL, NULL, 1, 1)",
            rusqlite::params![id, writer],
        )
        .unwrap();

    // Tool names are searchable (auto-quoting carries `chat-send` through).
    let hits = store.search("chat-send", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].conversation_id, id);
    assert!(
        hits[0].snippet.contains(">>>chat-send<<<"),
        "{}",
        hits[0].snippet
    );
    assert_eq!(store.search("file-read", 10).unwrap().len(), 1);

    // Args digests are searchable too — including path-shaped tokens.
    assert_eq!(store.search("channel", 10).unwrap().len(), 1);
    assert_eq!(store.search("src/store.rs", 10).unwrap().len(), 1);

    // Malformed events must never break appends or search: the extraction
    // is json_valid-guarded and yields empty derived columns.
    raw(root.path())
        .execute(
            "INSERT INTO turns
               (conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                events, tokens_in, tokens_out, ts_claim, encoding_version)
             VALUES (?1, ?2, 10000, 'x', 'capybara checkpoint', 'ok',
                     'not json at all', NULL, NULL, 1, 1)",
            rusqlite::params![id, writer],
        )
        .unwrap();
    assert_eq!(store.search("capybara", 10).unwrap().len(), 1);
}

/// Re-creating an existing id (the JSON-parity REPLACE path) cascades the
/// old turns away — their index entries must go with them, or a later
/// turn reusing the rowid would inherit ghost terms.
#[test]
fn fts_recreating_a_conversation_resets_its_index() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = new_conversation_id();
    store.create_with_id(&id, "first life", None).unwrap();
    store
        .append_turn(&id, "the narwhal detail", "noted")
        .unwrap();
    assert_eq!(store.search("narwhal", 10).unwrap().len(), 1);

    store.create_with_id(&id, "second life", None).unwrap();
    assert!(
        store.search("narwhal", 10).unwrap().is_empty(),
        "REPLACE must clear the old turns' index entries"
    );
    store.append_turn(&id, "fresh start", "ok").unwrap();
    assert_eq!(store.search("fresh", 10).unwrap().len(), 1);
}

/// Adversarial queries end to end: everything the sanitizer matrix throws
/// must either search cleanly or fail with the sanitizer's own "reduced to
/// nothing" — an FTS5 syntax error reaching the user is a bug.
#[test]
fn fts_adversarial_queries_never_surface_syntax_errors() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("target", None).unwrap();
    store
        .append_turn(&id, "run chat-send for P2.2", "sent via src/lib.rs")
        .unwrap();
    store
        .append_turn(
            &id,
            "what about '; DROP TABLE turns; -- style attacks",
            "they are plain text to the index",
        )
        .unwrap();

    let nasties = [
        "\"",
        "*",
        "(",
        "^",
        "((((",
        "\"unbalanced",
        "AND",
        "NOT",
        "OR OR",
        "foo AND",
        "NEAR",
        "NEAR(a b, 2)",
        "col:filter",
        "user:secret",
        "-exclude",
        "+plus",
        "a.b/c:d-e",
        "chat-send P2.2 src/lib.rs",
        "\"phrase\" AND ( ) ^",
        "→ ☃",
        "'; DROP TABLE turns; --",
        "",
    ];
    for q in nasties {
        match store.search(q, 10) {
            Ok(_) => {}
            Err(e) => {
                let text = e.to_string();
                assert!(
                    text.contains("reduced to nothing"),
                    "{q:?} must sanitize or reduce, not error with: {text}"
                );
            }
        }
    }

    // And the sanitized forms actually FIND things.
    assert_eq!(store.search("chat-send", 10).unwrap().len(), 1);
    assert_eq!(store.search("P2.2", 10).unwrap().len(), 1);
    assert_eq!(store.search("src/lib.rs", 10).unwrap().len(), 1);
    // SQL injection text is just terms; the turns table survived.
    assert_eq!(store.search("\"DROP TABLE\"", 10).unwrap().len(), 1);
}

/// Perf probe (not a gate): build a 1k-turn corpus and time searches.
/// Run with: cargo test -p newt-core --test store -- --ignored fts_search_latency
#[test]
#[ignore = "perf probe — run with --ignored to measure recall latency"]
fn fts_search_latency_on_a_1k_turn_corpus() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 0).unwrap();

    let vocab = [
        "parser",
        "tokenizer",
        "budget",
        "probe",
        "kraken",
        "deploy",
        "rollback",
        "coverage",
        "ratchet",
        "snippet",
        "mesh",
        "caveat",
        "lattice",
        "chain",
    ];
    for c in 0..100 {
        let id = store.create(&format!("conv {c}"), None).unwrap();
        for t in 0..10 {
            let mut user = String::new();
            for w in 0..12 {
                user.push_str(vocab[(c + t * 3 + w) % vocab.len()]);
                user.push(' ');
            }
            let assistant = format!("turn {t} of conversation {c}: {user}");
            store.append_turn(&id, &user, &assistant).unwrap();
        }
    }

    let queries = [
        "kraken",
        "tokenizer budget",
        "\"parser tokenizer\"",
        "chat-send",
        "ratchet OR mesh",
    ];
    let started = std::time::Instant::now();
    let mut total_hits = 0usize;
    const ROUNDS: usize = 20;
    for _ in 0..ROUNDS {
        for q in queries {
            total_hits += store.search(q, 10).unwrap().len();
        }
    }
    let elapsed = started.elapsed();
    let per_query = elapsed / (ROUNDS * queries.len()) as u32;
    println!(
        "1k-turn corpus: {} queries in {elapsed:?} → {per_query:?}/query ({total_hits} hits)",
        ROUNDS * queries.len()
    );
    assert!(
        per_query < std::time::Duration::from_millis(50),
        "recall must stay interactive on a 1k-turn corpus: {per_query:?}"
    );
}

/// A corrupt identity.pem must not block the store: it falls back to the
/// per-install nonce — the same fingerprint the install had before.
#[test]
fn corrupt_identity_pem_falls_back_to_install_nonce() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let nonce_fp = {
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        store.writer_fingerprint().to_string()
    };
    std::fs::write(root.path().join("identity.pem"), "not a pem at all").unwrap();
    let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
    assert_eq!(
        store.writer_fingerprint(),
        nonce_fp,
        "unparseable key file must fall back to the stable nonce identity"
    );
}

// =========================================================================
// Part 4 — 17.5: StoreRecallSource, the `recall` model tool's store backend
// (workspace-fenced by the store, current conversation excluded).
// =========================================================================

#[test]
fn store_recall_source_excludes_the_current_conversation() {
    use newt_core::{RecallSource as _, StoreRecallSource};

    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    // The conversation the model is "in" — its turns must never come back.
    let current = store.create("current work", None).unwrap();
    store
        .append_turn(&current, "fix the tokio panic", "patched the tokio panic")
        .unwrap();
    // A past conversation on the same topic — this is what recall is for.
    let past = store.create("past work", None).unwrap();
    store
        .append_turn(
            &past,
            "we hit a tokio panic in retry",
            "bounded the retries",
        )
        .unwrap();

    // The raw store search sees both conversations …
    let raw = store.search("tokio panic", 10).unwrap();
    assert!(raw.iter().any(|h| h.conversation_id == current));
    assert!(raw.iter().any(|h| h.conversation_id == past));

    // … the recall source sees only the past one.
    let source = StoreRecallSource::new(&store, &current);
    let hits = source.search("tokio panic", 5).unwrap();
    assert!(!hits.is_empty(), "the past conversation must match");
    assert!(
        hits.iter().all(|h| h.conversation_id == past),
        "current conversation leaked into recall: {hits:?}"
    );
}

#[test]
fn store_recall_source_truncates_to_limit_after_exclusion() {
    use newt_core::{RecallSource as _, StoreRecallSource};

    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let current = store.create("current", None).unwrap();
    store
        .append_turn(&current, "fts5 ranking question", "fts5 ranking answer")
        .unwrap();
    for i in 0..4 {
        let id = store.create(&format!("past {i}"), None).unwrap();
        store
            .append_turn(&id, &format!("fts5 ranking case {i}"), "noted")
            .unwrap();
    }

    let source = StoreRecallSource::new(&store, &current);
    let hits = source.search("fts5 ranking", 2).unwrap();
    assert_eq!(hits.len(), 2, "limit applies after exclusion: {hits:?}");
    assert!(hits.iter().all(|h| h.conversation_id != current));
}

#[test]
fn this_conversation_recent_returns_the_current_conversations_own_last_turns() {
    // #714: the OPPOSITE of search's filter — `resume_context` must read THIS
    // conversation's own pre-interrupt turns (the affordance recall refuses).
    use newt_core::{RecallSource as _, StoreRecallSource};

    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let current = store.create("resumed work", None).unwrap();
    for i in 0..5 {
        store
            .append_turn(&current, &format!("ask {i}"), &format!("reply {i}"))
            .unwrap();
    }
    // A different conversation must NOT leak in (this is a self-read, fenced to
    // the current id — the mirror of search's exclusion).
    let other = store.create("other work", None).unwrap();
    store.append_turn(&other, "unrelated", "unrelated").unwrap();

    let source = StoreRecallSource::new(&store, &current);
    let hits = source.this_conversation_recent(3).unwrap();
    assert_eq!(hits.len(), 3, "last `limit` turns only: {hits:?}");
    // All hits belong to THIS conversation — the opposite of recall's filter.
    assert!(
        hits.iter().all(|h| h.conversation_id == current),
        "another conversation leaked into the self-read: {hits:?}"
    );
    // Oldest-first within the window (turns 2,3,4 of 0..5), seq = 1-based pos.
    assert!(hits[0].snippet.contains("ask 2") && hits[0].snippet.contains("reply 2"));
    assert!(hits[2].snippet.contains("ask 4"));
    assert_eq!(hits[0].seq, 3, "1-based position of turn index 2");
    assert_eq!(hits[2].seq, 5);

    // limit 0 → empty (the guard), and a limit past the end clamps to all turns.
    assert!(source.this_conversation_recent(0).unwrap().is_empty());
    assert_eq!(source.this_conversation_recent(99).unwrap().len(), 5);
}

// =========================================================================
// Part 5 — 17.6: tool-event + token-usage recording (issue #246).
// The turn grows past `(task, reply)`: `append_turn_full` persists the
// loop's recorded ToolEvents into the `events` JSON column and the
// backend-reported token actuals into `tokens_in`/`tokens_out`. Events are
// §6 content (chain-covered), their digests never carry raw args, and the
// 17.3 FTS trigger picks the new columns up with no schema work.
// =========================================================================

/// The events the agentic loop would record for a small two-tool turn.
fn sample_events() -> Vec<ToolEvent> {
    vec![
        ToolEvent::from_call(
            "read_file",
            &serde_json::json!({"path": "src/store.rs"}),
            true,
            Some(4),
        ),
        ToolEvent::from_call(
            "run_command",
            &serde_json::json!({"command": "cargo test -q"}),
            false,
            Some(2_500),
        ),
    ]
}

#[test]
fn tool_events_and_tokens_round_trip_through_append_and_load() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("tooling", None).unwrap();
    let events = sample_events();
    store
        .append_turn_full(
            &id,
            "fix the bug",
            "fixed",
            &events,
            &[],
            Some(1_204),
            Some(892),
        )
        .unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(record.turns.len(), 1);
    let turn = &record.turns[0];
    assert_eq!(turn.user, "fix the bug");
    assert_eq!(turn.assistant, "fixed");
    assert_eq!(turn.events, events, "events must round-trip verbatim");
    assert_eq!(turn.tokens_in, Some(1_204));
    assert_eq!(turn.tokens_out, Some(892));
    // The outcome and duration claims survive too.
    assert!(turn.events[0].ok);
    assert!(!turn.events[1].ok);
    assert_eq!(turn.events[1].duration_ms, Some(2_500));
}

/// #717: the per-turn phantom-reach telemetry persists into its own
/// `phantom_reaches` column and reloads verbatim — distinct from `events`.
/// Also proves the §6 content chain still verifies, i.e. the new column is
/// additive and NOT folded into the canonical encoding (telemetry, not
/// provenance), so existing chains remain valid byte-for-byte.
#[test]
fn phantom_reaches_round_trip_and_chain_still_verifies() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("phantoms", None).unwrap();
    let phantoms = vec![
        PhantomReach {
            name_as_called: "bash".to_string(),
            resolution: PhantomResolution::Rewrite("run_command".to_string()),
            active_context_features: Vec::new(),
        },
        PhantomReach {
            name_as_called: "enter_plan_mode".to_string(),
            resolution: PhantomResolution::Unknown,
            active_context_features: Vec::new(),
        },
    ];
    store
        .append_turn_full(&id, "do it", "done", &[], &phantoms, None, None)
        .unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(record.turns.len(), 1);
    let turn = &record.turns[0];
    assert_eq!(
        turn.phantom_reaches, phantoms,
        "phantom reaches must round-trip verbatim"
    );
    // They are distinct telemetry: no tool events were recorded this turn.
    assert!(
        turn.events.is_empty(),
        "phantom reaches are not tool events"
    );
    // The new column rides outside the §6 canonical encoding, so the chain
    // — populated with a non-empty phantom payload — still verifies.
    store.verify_chain(&id).unwrap();
}

/// #713: the conversation scratchpad `<state>` snapshot persists into its own
/// `scratchpad` column and reloads verbatim, so an interrupt + auto-resume can
/// re-hydrate the live store. Also proves the §6 content chain still verifies,
/// i.e. the column is additive and NOT folded into the canonical encoding
/// (working memory, not provenance) — existing chains remain valid
/// byte-for-byte.
#[test]
fn scratchpad_round_trips_and_chain_still_verifies() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("scratchpad", None).unwrap();
    // A turn establishes the §6 chain we will re-verify after the scratchpad
    // write — proving the scratchpad rides outside the chain.
    store
        .append_turn(&id, "what were we doing?", "fixing the parser")
        .unwrap();
    let tip_before = store.load(&id).unwrap();
    assert!(
        tip_before.scratchpad.is_empty(),
        "a fresh row carries the empty `{{}}` backfill"
    );

    let mut state = std::collections::BTreeMap::new();
    state.insert("current_task".to_string(), "fix the parser".to_string());
    state.insert("open_file".to_string(), "src/parser.rs:128".to_string());
    store.update_scratchpad(&id, &state).unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(
        record.scratchpad, state,
        "scratchpad <state> must round-trip verbatim through save + load"
    );
    // The exact round-0 black-hole probe now resolves from the restored snapshot.
    assert_eq!(
        record.scratchpad.get("current_task").map(String::as_str),
        Some("fix the parser"),
        "the resumed `state_get(\"current_task\")` survives"
    );
    // The scratchpad rides the conversation row, outside the §6 canonical
    // encoding, so the chain — written before AND independent of the scratchpad
    // — still verifies byte-for-byte.
    store.verify_chain(&id).unwrap();

    // An overwrite (the live store mutating across turns) replaces, not merges.
    let mut state2 = std::collections::BTreeMap::new();
    state2.insert("current_task".to_string(), "ship the fix".to_string());
    store.update_scratchpad(&id, &state2).unwrap();
    let reloaded = store.load(&id).unwrap();
    assert_eq!(reloaded.scratchpad, state2, "latest snapshot wins");
    assert!(
        !reloaded.scratchpad.contains_key("open_file"),
        "a fresh snapshot is the whole map, not a merge"
    );
    store.verify_chain(&id).unwrap();
}

/// #715: the conversation plan-ledger snapshot persists into its own `plan`
/// column and reloads VERBATIM — including which steps are Done and which is
/// Active (the full state `set_plan` would reset), so an interrupt + auto-resume
/// can re-hydrate the live ledger. Also proves the §6 content chain still
/// verifies, i.e. the column is additive and NOT folded into the canonical
/// encoding (working memory, not provenance).
#[test]
fn plan_snapshot_round_trips_and_chain_still_verifies() {
    use newt_core::StepLedger;
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("plan", None).unwrap();
    // A turn establishes the §6 chain we re-verify after the plan write.
    store
        .append_turn(&id, "what is the plan?", "let me set one")
        .unwrap();
    let before = store.load(&id).unwrap();
    assert!(
        before.plan.is_empty(),
        "a fresh row carries the empty `{{}}` backfill"
    );

    // Build an ADVANCED ledger: step 1 Done, step 2 Active, step 3 Todo.
    let ledger = newt_core::SessionStepLedger::default();
    ledger.set_plan(&[
        "read the code".to_string(),
        "write the fix".to_string(),
        "test it".to_string(),
    ]);
    ledger.advance(); // step 1 → Done, step 2 → Active
    let snap = ledger.snapshot();
    store.update_plan_snapshot(&id, &snap).unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(
        record.plan, snap,
        "plan snapshot must round-trip verbatim (steps + statuses)"
    );
    // The active step + done statuses survive — not reset to a fresh plan.
    assert_eq!(record.plan.len(), 3);
    assert_eq!(record.plan.steps[0].status, newt_core::StepStatus::Done);
    assert_eq!(record.plan.steps[1].status, newt_core::StepStatus::Active);
    assert_eq!(record.plan.steps[2].status, newt_core::StepStatus::Todo);
    // The plan rides the conversation row, outside the §6 canonical encoding, so
    // the chain — written before AND independent of the plan — still verifies.
    store.verify_chain(&id).unwrap();

    // A later snapshot replaces, not merges (the live ledger advancing a step).
    ledger.advance(); // step 2 → Done, step 3 → Active
    store.update_plan_snapshot(&id, &ledger.snapshot()).unwrap();
    let reloaded = store.load(&id).unwrap();
    assert_eq!(reloaded.plan.steps[1].status, newt_core::StepStatus::Done);
    assert_eq!(reloaded.plan.steps[2].status, newt_core::StepStatus::Active);
    store.verify_chain(&id).unwrap();
}

/// The args digest is keys + hash, never values: feed a secret-looking arg
/// and prove the stored row carries no trace of it anywhere.
#[test]
fn args_digest_never_carries_raw_arg_values() {
    let secret = "AKIA-hunter2-SUPERSECRET";
    let event = ToolEvent::from_call(
        "write_file",
        &serde_json::json!({"path": "creds.env", "content": secret}),
        true,
        None,
    );
    // Key names are searchable; the value is absent (only a digest remains).
    assert!(event.args_digest.contains("content"));
    assert!(event.args_digest.contains("path"));
    assert!(event.args_digest.contains("b3:"));
    assert!(
        !event.args_digest.contains("hunter2"),
        "raw arg values must never reach the digest: {}",
        event.args_digest
    );
    assert!(!event.args_digest.contains("creds.env"));

    // End to end: nothing in the persisted row leaks the secret either.
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("secret turn", None).unwrap();
    store
        .append_turn_full(&id, "write creds", "done", &[event], &[], None, None)
        .unwrap();
    let stored: String = raw(root.path())
        .query_row(
            "SELECT events FROM turns WHERE conversation_id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!stored.contains("hunter2"), "leaked into events: {stored}");
    // And identical args correlate: same digest, different turn.
    let again = ToolEvent::from_call(
        "write_file",
        &serde_json::json!({"path": "creds.env", "content": secret}),
        true,
        None,
    );
    assert_eq!(
        again.args_digest,
        store.load(&id).unwrap().turns[0].events[0].args_digest
    );
}

/// Tokens are measurements: present when the backend reported them, NULL
/// when it did not — never a zero or an estimate dressed as one (18.5
/// rehydrates from these columns and must be able to trust them).
#[test]
fn absent_backend_usage_stores_null_not_a_guess() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("usage", None).unwrap();

    store
        .append_turn_full(&id, "with usage", "ok", &[], &[], Some(100), Some(20))
        .unwrap();
    store
        .append_turn_full(&id, "backend silent", "ok", &[], &[], None, None)
        .unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(record.turns[0].tokens_in, Some(100));
    assert_eq!(record.turns[0].tokens_out, Some(20));
    assert_eq!(record.turns[1].tokens_in, None);
    assert_eq!(record.turns[1].tokens_out, None);

    // At the SQL level the silent turn is genuinely NULL, not 0.
    let (tin, tout): (Option<i64>, Option<i64>) = raw(root.path())
        .query_row(
            "SELECT tokens_in, tokens_out FROM turns
              WHERE conversation_id = ?1 AND user = 'backend silent'",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((tin, tout), (None, None));
}

/// The 17.3 AFTER INSERT trigger derives tool_names/tool_args_digest from
/// the events JSON — so a turn appended through `append_turn_full` is
/// immediately recallable by the tool name it used and by digest terms.
#[test]
fn fts_finds_tool_names_recorded_by_append_turn_full() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("deploy day", None).unwrap();
    store
        .append_turn_full(
            &id,
            "ship it",
            "shipped",
            &[ToolEvent::from_call(
                "web_fetch",
                &serde_json::json!({"url": "https://release.example"}),
                true,
                Some(90),
            )],
            &[],
            Some(50),
            Some(10),
        )
        .unwrap();

    // Tool name hits via the derived tool_names column…
    let hits = store.search("web_fetch", 10).unwrap();
    assert_eq!(hits.len(), 1, "a recorded tool name must be recallable");
    assert_eq!(hits[0].conversation_id, id);
    assert!(hits[0].snippet.contains(">>>"), "{}", hits[0].snippet);
    // …and digest key terms via tool_args_digest ("url" is in the digest;
    // the URL value itself never reached the index).
    assert_eq!(store.search("url", 10).unwrap().len(), 1);
    assert!(store.search("release.example", 10).unwrap().is_empty());
}

/// §6: events and token counts are row content. The chain verifies with
/// them populated, and editing a stored event after the fact breaks it —
/// same tamper evidence as user/assistant text.
#[test]
fn chain_verifies_with_events_and_detects_event_tampering() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("chained tools", None).unwrap();

    // Mixed history: plain turn, evented turn, plain turn.
    store.append_turn(&id, "plan", "planned").unwrap();
    store
        .append_turn_full(&id, "act", "acted", &sample_events(), &[], Some(700), None)
        .unwrap();
    store.append_turn(&id, "wrap", "wrapped").unwrap();
    store
        .verify_chain(&id)
        .expect("populated events must verify under the unchanged v1 encoding");

    // Rewriting history's tool record is detectable.
    let changed = raw(root.path())
        .execute(
            "UPDATE turns SET events = '[]' WHERE conversation_id = ?1 AND user = 'act'",
            [&id],
        )
        .unwrap();
    assert_eq!(changed, 1);
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation"),
        "tampered events must break the chain: {err}"
    );
}

/// Back-compat: rows written before 17.6 (events = '[]', token columns
/// NULL — exactly what plain `append_turn` still writes) load as empty
/// events and absent tokens, and verify unchanged.
#[test]
fn pre_17_6_rows_with_empty_events_still_load_and_verify() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("legacy shape", None).unwrap();
    store.append_turn(&id, "old task", "old reply").unwrap();

    let record = store.load(&id).unwrap();
    assert_eq!(record.turns.len(), 1);
    assert!(record.turns[0].events.is_empty());
    assert_eq!(record.turns[0].tokens_in, None);
    assert_eq!(record.turns[0].tokens_out, None);
    // The wrapper writes the byte-identical pre-17.6 shape ('[]'/NULL)…
    let stored: (String, Option<i64>) = raw(root.path())
        .query_row(
            "SELECT events, tokens_in FROM turns WHERE conversation_id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, ("[]".to_string(), None));
    // …so pre-17.6 chains keep verifying under the same v1 encoding.
    store.verify_chain(&id).unwrap();

    // A garbage events blob (writable only by an external tool) refuses to
    // load as silent garbage — the encoding_version philosophy.
    raw(root.path())
        .execute(
            "UPDATE turns SET events = 'not json' WHERE conversation_id = ?1",
            [&id],
        )
        .unwrap();
    let err = store.load(&id).unwrap_err().to_string();
    assert!(err.contains("tool-event"), "{err}");
}
