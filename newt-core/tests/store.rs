//! SQLite `ConversationStore` suite — Phase 17.1a (issue #246).
//!
//! Part 1 ports tests/conversation_store.rs unchanged semantically (the
//! backend swap must be invisible through the public API; the two
//! storage-format-specific tests are ported to their SQLite analogues).
//! Part 2 covers what is new in 17.1a: §6 causal ordering (MRU = activity
//! tick, never a timestamp), the clock-skew case, BLAKE3 chain integrity
//! and tamper detection, two-writer `busy_timeout` concurrency, and the
//! schema-diff migration.

use newt_core::{new_conversation_id, session_plan_dir, session_plan_path, ConversationTurn};
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
// backend's tests/conversation_store.rs).
// =========================================================================

#[test]
fn create_with_id_adopts_the_supplied_id() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = new_conversation_id();
    assert!(!store.exists(&id));
    store
        .create_with_id(&id, "pre-assigned title", Some("coder"))
        .unwrap();
    assert!(store.exists(&id));

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

/// SQLite analogue of `corrupt_record_does_not_poison_the_workspace`: the
/// legacy per-record JSON tree (including a corrupt record — 17.1b's import
/// problem, not ours) sits under the same root and must not affect the
/// SQLite store in any way.
#[test]
fn legacy_json_records_beside_the_db_do_not_poison_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

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
    assert_eq!(store.load("legacy-conv").unwrap().title, "from v1");

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

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let len = a
        .bytes()
        .zip(b.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    assert!(len > 0, "test ids should share the unix timestamp prefix");
    &a[..len]
}
