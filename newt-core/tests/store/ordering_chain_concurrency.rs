use super::*;

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
        // #1030: the thin roadmap-tree pointer columns are additive too — an
        // older db (this hand-built v1) gains them on open via reconciliation.
        ("conversations", "roadmap_id"),
        ("conversations", "node_id"),
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
    // #1030: the thin pointer columns back-fill to NULL — a migrated legacy
    // conversation reads as an ad-hoc chat, not part of any roadmap tree.
    assert!(
        legacy.roadmap_id.is_none() && legacy.node_id.is_none(),
        "migrated legacy conversation must be an unlinked ad-hoc chat"
    );
    // #1030: the new tables are created on open (CREATE TABLE IF NOT EXISTS),
    // so an older db additively gains the roadmap tree + live-owner store.
    for table in ["roadmaps", "live_owners"] {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "{table} table must exist after open");
    }

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

/// #1030: the roadmap-tree pointer columns round-trip through the store — a
/// fresh conversation is an ad-hoc chat (NULL pointers), `link_conversation_to_node`
/// binds it to a (roadmap, node), and passing `None` clears it back.
#[test]
fn conversation_roadmap_link_round_trips_and_defaults_to_none() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    let id = store.create("plan node", None).unwrap();
    // A freshly created conversation is unlinked — an ad-hoc chat.
    let rec = store.load(&id).unwrap();
    assert!(rec.roadmap_id.is_none() && rec.node_id.is_none());

    // Bind it to a Plan node in a roadmap tree.
    store
        .link_conversation_to_node(&id, Some("roadmap-1"), Some("plan-node-7"))
        .unwrap();
    let linked = store.load(&id).unwrap();
    assert_eq!(linked.roadmap_id.as_deref(), Some("roadmap-1"));
    assert_eq!(linked.node_id.as_deref(), Some("plan-node-7"));

    // Clearing the link returns it to an ad-hoc chat.
    store.link_conversation_to_node(&id, None, None).unwrap();
    let cleared = store.load(&id).unwrap();
    assert!(cleared.roadmap_id.is_none() && cleared.node_id.is_none());
}

// ── #1030 collision fix: live_owners claim / release / heartbeat ────────────

#[test]
fn claim_grants_a_fresh_conversation_and_reaffirms_its_own_claim() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    store.set_owner_for_test("hostA", "boot-1", 1001);
    // A session claims its freshly-minted id at startup — before any turn is
    // saved, so the conversation row does not exist yet.
    let id = newt_core::new_conversation_id();

    assert_eq!(store.claim(&id).unwrap(), newt_core::ClaimOutcome::Claimed);
    // Re-claiming our OWN conversation is idempotent, not a conflict.
    assert_eq!(store.claim(&id).unwrap(), newt_core::ClaimOutcome::Claimed);

    let owner = store.live_owner(&id).unwrap().expect("claimed");
    assert_eq!(owner.host, "hostA");
    assert_eq!(owner.pid, 1001);
}

#[test]
fn a_second_live_process_is_refused_the_same_conversation() {
    // THE #1030 bug, prevented: two LIVE newts cannot both own one conversation
    // (which is how their turns interleaved into one record).
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = newt_core::new_conversation_id();

    // Process A claims.
    store.set_owner_for_test("host", "boot-1", 1001);
    assert_eq!(store.claim(&id).unwrap(), newt_core::ClaimOutcome::Claimed);

    // Process B (different pid) finds A's claim LIVE -> refused, writes nothing.
    store.set_liveness_for_test(|_owner, _now| true);
    store.set_owner_for_test("host", "boot-1", 2002);
    match store.claim(&id).unwrap() {
        newt_core::ClaimOutcome::HeldBy { pid, host } => {
            assert_eq!(pid, 1001);
            assert_eq!(host, "host");
        }
        other => panic!("expected HeldBy A, got {other:?}"),
    }
    // A still owns it — B did not overwrite the claim.
    assert_eq!(store.live_owner(&id).unwrap().unwrap().pid, 1001);
}

#[test]
fn a_stale_claim_from_a_dead_process_is_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = newt_core::new_conversation_id();

    store.set_owner_for_test("host", "boot-1", 1001);
    store.claim(&id).unwrap();

    // A has died: the oracle reports its claim not-live. B reclaims cleanly.
    store.set_liveness_for_test(|_owner, _now| false);
    store.set_owner_for_test("host", "boot-1", 2002);
    assert_eq!(store.claim(&id).unwrap(), newt_core::ClaimOutcome::Claimed);
    assert_eq!(store.live_owner(&id).unwrap().unwrap().pid, 2002);
}

#[test]
fn release_frees_only_this_processs_own_claim() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = newt_core::new_conversation_id();

    store.set_owner_for_test("host", "boot-1", 1001);
    store.claim(&id).unwrap();

    // A different process's release does NOT free A's live claim.
    store.set_owner_for_test("host", "boot-1", 9999);
    store.release(&id).unwrap();
    assert!(
        store.live_owner(&id).unwrap().is_some(),
        "a foreign release must be a no-op"
    );

    // A's own release frees it.
    store.set_owner_for_test("host", "boot-1", 1001);
    store.release(&id).unwrap();
    assert!(store.live_owner(&id).unwrap().is_none());
}

#[test]
fn heartbeat_refreshes_freshness_and_is_owner_live_uses_the_oracle() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = newt_core::new_conversation_id();

    store.set_owner_for_test("host", "boot-1", 1001);
    store.set_claim_clock_for_test(|| 1_000);
    store.claim(&id).unwrap();
    assert_eq!(
        store.live_owner(&id).unwrap().unwrap().heartbeat_tick,
        1_000
    );

    store.set_claim_clock_for_test(|| 5_000);
    store.heartbeat(&id).unwrap();
    let owner = store.live_owner(&id).unwrap().unwrap();
    assert_eq!(owner.heartbeat_tick, 5_000);

    // is_owner_live delegates to the injected oracle.
    store.set_liveness_for_test(|_owner, _now| false);
    assert!(!store.is_owner_live(&owner));
    store.set_liveness_for_test(|_owner, _now| true);
    assert!(store.is_owner_live(&owner));
}

// ── #1030 roadmap CRUD ──────────────────────────────────────────────────────

#[test]
fn roadmap_crud_round_trips_the_tree() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

    // A small Roadmap→Phase→Plan tree authored as a plan.rs::Plan.
    let toml = r#"
[[subtask]]
id = "road"
instruction = "the roadmap"
kind = "roadmap"

[[subtask]]
id = "phase-1"
instruction = "phase one"
kind = "phase"
parent = "road"

[[subtask]]
id = "plan-1"
instruction = "implement it"
kind = "plan"
parent = "phase-1"
"#;
    let tree = newt_core::plan::Plan::from_toml_str(toml).unwrap();
    store
        .create_roadmap("rm-1", "Mermaid in Rust", &tree)
        .unwrap();

    // Load round-trips the tree byte-for-byte (same Plan value).
    let loaded = store.load_roadmap("rm-1").unwrap().expect("roadmap exists");
    assert_eq!(loaded.title, "Mermaid in Rust");
    assert_eq!(loaded.tree, tree);
    assert_eq!(loaded.tree.subtasks.len(), 3);

    // Update replaces the tree (grow a Task under the plan).
    let grown_toml = format!(
        "{toml}\n[[subtask]]\nid = \"task-1\"\ninstruction = \"commit\"\nkind = \"task\"\nparent = \"plan-1\"\n"
    );
    let grown = newt_core::plan::Plan::from_toml_str(&grown_toml).unwrap();
    store.update_roadmap("rm-1", &grown).unwrap();
    assert_eq!(
        store
            .load_roadmap("rm-1")
            .unwrap()
            .unwrap()
            .tree
            .subtasks
            .len(),
        4
    );

    // list_roadmaps surfaces it with a node count.
    let list = store.list_roadmaps().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "rm-1");
    assert_eq!(list[0].node_count, 4);

    // An absent id loads as None (not an error).
    assert!(store.load_roadmap("nope").unwrap().is_none());
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
