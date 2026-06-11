//! B1 — turn-write cost & store size, AFTER scratch harness (issues #245/#246).
//!
//! The baseline version of this bin (committed in the `f0f4f6e` baseline,
//! `docs/testing/results/context-baseline-f0f4f6e.md`) measured the JSON
//! `ConversationStore` that rewrote the whole pretty-printed record per turn.
//! This version measures the SQLite store that replaced it (17.1a, #261),
//! same payloads, same windows, same iteration counts:
//!
//!   * Phase A — per-append wall time at N ∈ {1, 10, 100, 1000} turns
//!     (p50/p95 over the same windows), on-disk size (db + wal) at each N;
//!   * Phase A2 — `verify_chain()` and FTS `search()` latency at 1,000 turns;
//!   * Phase B — `list()` latency at 100 / 1000 conversations;
//!   * Phase C — §6 ordering-primitive overhead in isolation: a replica
//!     `turns`/`conversations`/`writer_clock` schema (same pragmas, **no FTS
//!     triggers**) driven through three variants per append —
//!       V0  bare insert + conversation-row update + commit,
//!       V1  V0 + the per-writer Lamport tick (same SQL as `next_tick`),
//!       V2  V1 + the content chain (last-turn SELECT + BLAKE3 of the v1
//!           canonical encoding — replicated below; if `store.rs` changes
//!           its encoding, update this copy).
//!     (V2−V0)/V0 is the tick+chain overhead the design doc budgets at <5%
//!     (`docs/design/context-memory-hermes-learnings.md` §6). The production
//!     `append_turn` cannot toggle these primitives off, so the isolation
//!     lives here, in the bench replica, not in shipped code.
//!
//! Store root is a tempdir (local ext4; WAL + synchronous=NORMAL exactly as
//! the real store applies them). NOT a workspace member, NOT in CI.

use std::time::Instant;

use newt_core::ConversationStore;
use rusqlite::Connection;

/// Deterministic ~1–4 KB of "realistic" turn text: prose + a code fragment +
/// tool-event-ish JSON, sized by a cheap LCG so runs are reproducible.
/// IDENTICAL to the baseline harness — the comparison depends on it.
fn turn_payload(i: usize) -> (String, String) {
    // LCG for deterministic size variation in [1024, 4096) total bytes.
    let r = (i as u64)
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let total = 1024 + (r % 3072) as usize;
    let user = format!(
        "turn {i}: please fix the failing test in newt-core/src/conversation.rs and \
         re-run `cargo test -p newt-core` until green. Context: issue #245, branch \
         step-17.1a; the record path is ~/.newt/conversations/<ws>/<id>.json."
    );
    let mut assistant = format!(
        "Round {i}: read the file, found the bug at line {}. Patch:\n```rust\n\
         fn oldest_records_first(a: &Rec, b: &Rec) -> Ordering {{\n    a.tick.cmp(&b.tick)\n}}\n```\n\
         tool_event: {{\"name\":\"run_command\",\"args_digest\":\"blake3:abcd{i:04}\",\
         \"result\":\"test result: ok. 42 passed\"}}\n",
        i % 300
    );
    let filler = "the quick brown newt regenerates context across sessions; ";
    while user.len() + assistant.len() < total {
        assistant.push_str(filler);
    }
    (user, assistant)
}

fn pct(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn stats(samples: &[u128]) -> (u128, u128, u128, u128) {
    let mut s = samples.to_vec();
    s.sort_unstable();
    (pct(&s, 0.50), pct(&s, 0.95), s[0], s[s.len() - 1])
}

fn us(v: u128) -> String {
    format!("{:.1}", v as f64 / 1000.0)
}

/// db + wal size (bytes) for the store at `root`.
fn db_sizes(root: &std::path::Path) -> (u64, u64) {
    let db = std::fs::metadata(root.join("conversations.db"))
        .map(|m| m.len())
        .unwrap_or(0);
    let wal = std::fs::metadata(root.join("conversations.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    (db, wal)
}

fn main() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let ws = tempfile::tempdir()?;
    println!("store root: {} (tempdir)", root.path().display());

    // ---- Phase A: one conversation, 1000 appends, per-append latency ----
    // max_per_workspace = 0 disables prune (we measure append, not prune;
    // append never prunes — only create does, same as the JSON backend).
    let store = ConversationStore::new(root.path(), ws.path(), 0)?;
    if let Some(fb) = store.wal_fallback_notice() {
        println!("!! WAL fallback active ({fb}) — numbers are DELETE-mode, not WAL");
    }
    let id = store.create("bench conversation", None)?;

    let n_total = 1000usize;
    let mut lat_ns: Vec<u128> = Vec::with_capacity(n_total);
    let mut size_at = Vec::new();
    for i in 1..=n_total {
        let (user, assistant) = turn_payload(i);
        let t0 = Instant::now();
        store.append_turn(&id, &user, &assistant)?;
        lat_ns.push(t0.elapsed().as_nanos());
        if matches!(i, 1 | 10 | 100 | 1000) {
            size_at.push((i, db_sizes(root.path())));
        }
    }

    println!("\n## B1.A — append_turn latency vs store size (single conversation, 1000 appends)");
    println!("| N (turn #) | window | p50 (us) | p95 (us) | min (us) | max (us) |");
    println!("|---|---|---|---|---|---|");
    let windows: &[(&str, std::ops::Range<usize>)] = &[
        ("1", 0..1),
        ("10", 1..10),       // turns 2-10
        ("100", 89..110),    // turns 90-110
        ("1000", 899..1000), // turns 900-1000
    ];
    for (label, range) in windows {
        let (p50, p95, min, max) = stats(&lat_ns[range.clone()]);
        println!(
            "| {label} | turns {}-{} | {} | {} | {} | {} |",
            range.start + 1,
            range.end,
            us(p50),
            us(p95),
            us(min),
            us(max)
        );
    }
    println!("\n| N turns | db (bytes) | wal (bytes) |");
    println!("|---|---|---|");
    for (n, (db, wal)) in &size_at {
        println!("| {n} | {db} | {wal} |");
    }

    // ---- Phase A2: verify_chain + FTS search at 1000 turns ----
    let mut vc = Vec::new();
    for _ in 0..10 {
        let t0 = Instant::now();
        store.verify_chain(&id)?;
        vc.push(t0.elapsed().as_nanos());
    }
    let (p50, p95, min, max) = stats(&vc);
    println!(
        "\n## B1.A2 — verify_chain @ 1000 turns (10 iters): p50 {} us, p95 {} us, \
         min {} us, max {} us",
        us(p50),
        us(p95),
        us(min),
        us(max)
    );

    // Queries hit the payload text: a common word, a code token, a phrase,
    // and an issue-number-style token (sanitizer path).
    let queries = [
        "failing test",
        "blake3",
        "regenerates",
        "\"cargo test\"",
        "#245",
    ];
    println!("\n## B1.A2 — search() @ 1000 turns (limit 10, warm, 20 iters/query)");
    println!("| query | hits | p50 (us) | p95 (us) |");
    println!("|---|---|---|---|");
    for q in queries {
        let warm = store.search(q, 10)?; // warm page cache + statement
        let mut samples = Vec::new();
        for _ in 0..20 {
            let t0 = Instant::now();
            let hits = store.search(q, 10)?;
            samples.push(t0.elapsed().as_nanos());
            assert_eq!(hits.len(), warm.len());
        }
        let (p50, p95, _, _) = stats(&samples);
        println!("| `{q}` | {} | {} | {} |", warm.len(), us(p50), us(p95));
    }

    // ---- Phase B: list() at 100 / 1000 conversations ----
    for &count in &[100usize, 1000] {
        let root_b = tempfile::tempdir()?;
        let store_b = ConversationStore::new(root_b.path(), ws.path(), 0)?;
        for c in 0..count {
            let cid = store_b.create(&format!("conversation {c}"), None)?;
            for t in 1..=5 {
                let (u, a) = turn_payload(c * 7 + t);
                store_b.append_turn(&cid, &u, &a)?;
            }
        }
        // Warm one list (page cache), then measure 20 iterations.
        let _ = store_b.list()?;
        let mut samples = Vec::new();
        for _ in 0..20 {
            let t0 = Instant::now();
            let l = store_b.list()?;
            samples.push(t0.elapsed().as_nanos());
            assert_eq!(l.len(), count);
        }
        let (p50, p95, min, max) = stats(&samples);
        println!(
            "\n## B1.B — list() @ {count} conversations (5 turns each, 20 iters, warm): \
             p50 {} us, p95 {} us, min {} us, max {} us",
            us(p50),
            us(p95),
            us(min),
            us(max)
        );
        let (db, wal) = db_sizes(root_b.path());
        println!("   on-disk: db {db} bytes + wal {wal} bytes");
    }

    // ---- Phase C: §6 tick+chain overhead in isolation (replica schema) ----
    phase_c()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Phase C — replica of the store's tables (NO FTS triggers) so the §6
// primitives can be toggled. Schema/pragmas copied from newt-core/src/store.rs
// @ cf1aa3e; the FTS view/triggers are deliberately omitted (17.3 recall is
// not a §6 primitive — its cost shows up in the Phase A absolute numbers).
// ---------------------------------------------------------------------------

const TURN_ENCODING_V1_PREFIX: &[u8] = b"newt-turn:v1";

/// Replica of `TurnRow::canonical_encoding_v1` (store.rs @ cf1aa3e).
#[allow(clippy::too_many_arguments)]
fn canonical_encoding_v1(
    conversation_id: &str,
    writer_fingerprint: &str,
    seq: i64,
    prev_hash: &str,
    user: &str,
    assistant: &str,
    events: &str,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    ts_claim: i64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + user.len() + assistant.len() + events.len());
    out.extend_from_slice(TURN_ENCODING_V1_PREFIX);
    for field in [
        conversation_id.as_bytes(),
        writer_fingerprint.as_bytes(),
        prev_hash.as_bytes(),
        user.as_bytes(),
        assistant.as_bytes(),
        events.as_bytes(),
    ] {
        out.extend_from_slice(&(field.len() as u64).to_le_bytes());
        out.extend_from_slice(field);
    }
    out.extend_from_slice(&seq.to_le_bytes());
    for opt in [tokens_in, tokens_out] {
        match opt {
            Some(v) => {
                out.push(1);
                out.extend_from_slice(&v.to_le_bytes());
            }
            None => out.push(0),
        }
    }
    out.extend_from_slice(&ts_claim.to_le_bytes());
    out
}

fn open_replica(path: &std::path::Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let mode: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    anyhow::ensure!(mode.eq_ignore_ascii_case("wal"), "WAL did not take");
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE conversations (
             id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace_path TEXT NOT NULL,
             workspace_key TEXT NOT NULL, persona TEXT, end_reason TEXT,
             writer_fingerprint TEXT NOT NULL, activity_tick INTEGER NOT NULL,
             tip_hash TEXT NOT NULL, started_at_claim INTEGER NOT NULL,
             updated_at_claim INTEGER NOT NULL
         );
         CREATE TABLE turns (
             conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
             writer_fingerprint TEXT NOT NULL, seq INTEGER NOT NULL,
             prev_hash TEXT NOT NULL, user TEXT NOT NULL, assistant TEXT NOT NULL,
             events TEXT NOT NULL DEFAULT '[]', tokens_in INTEGER, tokens_out INTEGER,
             ts_claim INTEGER NOT NULL, encoding_version INTEGER NOT NULL DEFAULT 1,
             PRIMARY KEY (conversation_id, writer_fingerprint, seq)
         );
         CREATE TABLE writer_clock (
             writer_fingerprint TEXT PRIMARY KEY, last_tick INTEGER NOT NULL
         );
         CREATE INDEX idx_conversations_ws_tick
             ON conversations (workspace_key, activity_tick);
         INSERT INTO conversations VALUES
             ('conv-bench', 'bench', '/tmp/ws', 'wskey', NULL, NULL,
              'writer-bench', 0, 'genesis', 0, 0);",
    )?;
    Ok(conn)
}

/// One append through the replica. `tick`: allocate the Lamport tick via the
/// store's `next_tick` SQL (vs trusting the loop counter). `chain`: last-turn
/// SELECT + BLAKE3 canonical-encoding hash for `prev_hash` / `tip_hash`.
fn replica_append(conn: &Connection, i: usize, tick: bool, chain: bool) -> anyhow::Result<()> {
    let (user, assistant) = turn_payload(i);
    let now = i as i64; // claim clock — constant-cost stand-in
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;

    let seq: i64 = if tick {
        // Same statements as store.rs::next_tick (steady-state branch: the
        // writer_clock row exists after the first append).
        let bumped = tx.execute(
            "UPDATE writer_clock SET last_tick = last_tick + 1 WHERE writer_fingerprint = ?1",
            ["writer-bench"],
        )?;
        if bumped == 0 {
            tx.execute(
                "INSERT OR IGNORE INTO writer_clock (writer_fingerprint, last_tick)
                 SELECT ?1, COALESCE(MAX(t), 0) FROM (
                     SELECT MAX(seq) AS t FROM turns
                     UNION ALL SELECT MAX(activity_tick) FROM conversations
                     UNION ALL SELECT MAX(last_tick) FROM writer_clock
                 )",
                ["writer-bench"],
            )?;
            tx.execute(
                "UPDATE writer_clock SET last_tick = last_tick + 1 WHERE writer_fingerprint = ?1",
                ["writer-bench"],
            )?;
        }
        tx.query_row(
            "SELECT last_tick FROM writer_clock WHERE writer_fingerprint = ?1",
            ["writer-bench"],
            |row| row.get(0),
        )?
    } else {
        i as i64
    };

    let (prev_hash, tip_hash) = if chain {
        // Same SELECT as store.rs::last_turn, then BLAKE3 over the prior
        // row's canonical encoding (or the genesis stand-in on turn 1).
        let prev: Option<(i64, String, String, String, String, i64)> = tx
            .query_row(
                "SELECT seq, prev_hash, user, assistant, events, ts_claim FROM turns
                  WHERE conversation_id = 'conv-bench' AND writer_fingerprint = 'writer-bench'
                  ORDER BY seq DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let prev_hash = match prev {
            Some((pseq, pprev, puser, passistant, pevents, pts)) => {
                blake3::hash(&canonical_encoding_v1(
                    "conv-bench",
                    "writer-bench",
                    pseq,
                    &pprev,
                    &puser,
                    &passistant,
                    &pevents,
                    None,
                    None,
                    pts,
                ))
                .to_hex()
                .to_string()
            }
            None => blake3::hash(b"genesis-stand-in").to_hex().to_string(),
        };
        let tip = blake3::hash(&canonical_encoding_v1(
            "conv-bench",
            "writer-bench",
            seq,
            &prev_hash,
            &user,
            &assistant,
            "[]",
            None,
            None,
            now,
        ))
        .to_hex()
        .to_string();
        (prev_hash, tip)
    } else {
        ("constant-prev".to_string(), "constant-tip".to_string())
    };

    tx.execute(
        "INSERT INTO turns (conversation_id, writer_fingerprint, seq, prev_hash, user,
                            assistant, events, tokens_in, tokens_out, ts_claim, encoding_version)
         VALUES ('conv-bench', 'writer-bench', ?1, ?2, ?3, ?4, '[]', NULL, NULL, ?5, 1)",
        rusqlite::params![seq, prev_hash, user, assistant, now],
    )?;
    tx.execute(
        "UPDATE conversations
            SET activity_tick = ?1, tip_hash = ?2, updated_at_claim = ?3
          WHERE id = 'conv-bench'",
        rusqlite::params![seq, tip_hash, now],
    )?;
    tx.commit()?;
    Ok(())
}

fn phase_c() -> anyhow::Result<()> {
    // The three variants run INTERLEAVED (V0 turn i, V1 turn i, V2 turn i,
    // V0 turn i+1, …) into three separate replica dbs, so background drift
    // (WAL checkpoints, page-cache writeback) lands on all three equally —
    // sequential runs showed run-order effects larger than the measured
    // delta. p50 is the comparison statistic: per-append commits have a
    // multi-ms outlier tail (checkpoint stalls) that makes means lie.
    println!("\n## B1.C — §6 tick+chain overhead, replica schema (no FTS), 1000 interleaved appends each");
    println!("| variant | p50 (us) | p95 (us) |");
    println!("|---|---|---|");
    let variants: [(&str, bool, bool); 3] = [
        ("V0 bare insert+update+commit", false, false),
        ("V1 V0 + Lamport tick", true, false),
        ("V2 V1 + BLAKE3 chain", true, true),
    ];
    let dirs: Vec<_> = (0..3)
        .map(|_| tempfile::tempdir())
        .collect::<Result<_, _>>()?;
    let conns: Vec<Connection> = dirs
        .iter()
        .map(|d| open_replica(&d.path().join("replica.db")))
        .collect::<Result<_, _>>()?;
    let mut samples: [Vec<u128>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for i in 1..=1000usize {
        for (v, (_, tick, chain)) in variants.iter().enumerate() {
            let t0 = Instant::now();
            replica_append(&conns[v], i, *tick, *chain)?;
            samples[v].push(t0.elapsed().as_nanos());
        }
    }
    let mut p50s = Vec::new();
    for (v, (label, _, _)) in variants.iter().enumerate() {
        let (p50, p95, _, _) = stats(&samples[v]);
        p50s.push(p50 as f64);
        println!("| {label} | {} | {} |", us(p50), us(p95));
    }
    println!(
        "   overhead vs bare (p50): tick {:+.1}%, tick+chain {:+.1}% (budget: <5%)",
        (p50s[1] / p50s[0] - 1.0) * 100.0,
        (p50s[2] / p50s[0] - 1.0) * 100.0,
    );

    // Direct microbench of each §6 primitive against the populated V2 db
    // (1000 turns), outside any timing-confounding commit: what does ONE
    // tick allocation / last-turn SELECT / BLAKE3 canonical hash cost?
    let conn = &conns[2];
    let mut tick_ns = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let t0 = Instant::now();
        conn.execute(
            "UPDATE writer_clock SET last_tick = last_tick + 1 WHERE writer_fingerprint = ?1",
            ["writer-bench"],
        )?;
        let _: i64 = conn.query_row(
            "SELECT last_tick FROM writer_clock WHERE writer_fingerprint = ?1",
            ["writer-bench"],
            |row| row.get(0),
        )?;
        tick_ns.push(t0.elapsed().as_nanos());
    }
    let mut sel_ns = Vec::with_capacity(1000);
    let mut hash_ns = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let t0 = Instant::now();
        let row: (i64, String, String, String, String, i64) = conn.query_row(
            "SELECT seq, prev_hash, user, assistant, events, ts_claim FROM turns
              WHERE conversation_id = 'conv-bench' AND writer_fingerprint = 'writer-bench'
              ORDER BY seq DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
        sel_ns.push(t0.elapsed().as_nanos());
        let t1 = Instant::now();
        let h = blake3::hash(&canonical_encoding_v1(
            "conv-bench",
            "writer-bench",
            row.0,
            &row.1,
            &row.2,
            &row.3,
            &row.4,
            None,
            None,
            row.5,
        ))
        .to_hex()
        .to_string();
        hash_ns.push(t1.elapsed().as_nanos());
        std::hint::black_box(h);
    }
    println!("\n   §6 primitive microbench (1000 iters each, on the 1000-turn replica):");
    for (label, s) in [
        ("tick UPDATE+SELECT", &tick_ns),
        ("last-turn SELECT", &sel_ns),
        ("BLAKE3 canonical hash (~2.5KB)", &hash_ns),
    ] {
        let (p50, p95, _, _) = stats(s);
        println!("   - {label}: p50 {} us, p95 {} us", us(p50), us(p95));
    }
    Ok(())
}
