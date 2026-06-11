//! B1 — turn-write cost & store size, BASELINE scratch harness (issue #245).
//!
//! Measures the CURRENT `ConversationStore::append_turn` (which loads and
//! pretty-rewrites the whole JSON record on every turn) and `list()`:
//!
//!   * per-append wall time at N ∈ {1, 10, 100, 1000} turns (p50/p95 over
//!     windows around each N, plus min/max),
//!   * on-disk record size at each N,
//!   * `list()` latency at 100 and 1000 conversations in one workspace.
//!
//! Store root is a tempdir (local disk, page-cache writes — `save_record`
//! does not fsync). NOT a workspace member, NOT wired into CI; see Cargo.toml.

use std::time::Instant;

use newt_core::ConversationStore;

/// Deterministic ~1–4 KB of "realistic" turn text: prose + a code fragment +
/// tool-event-ish JSON, sized by a cheap LCG so runs are reproducible.
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

fn main() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let ws = tempfile::tempdir()?;
    println!("store root: {} (tempdir)", root.path().display());

    // ---- Phase A: one conversation, 1000 appends, per-append latency ----
    // max_per_workspace = 0 disables prune (we measure append, not prune;
    // append never prunes anyway — see conversation.rs:119-127).
    let store = ConversationStore::new(root.path(), ws.path(), 0)?;
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
            let path = root
                .path()
                .join("conversations")
                .join(store_workspace_id(&store, ws.path())?)
                .join(format!("{id}.json"));
            size_at.push((i, std::fs::metadata(&path)?.len()));
        }
    }

    println!("\n## B1.A — append_turn latency vs record size (single conversation, 1000 appends)");
    println!("| N (turn #) | window | p50 (us) | p95 (us) | min (us) | max (us) |");
    println!("|---|---|---|---|---|---|");
    let windows: &[(&str, std::ops::Range<usize>)] = &[
        ("1", 0..1),
        ("10", 1..10),     // turns 2-10
        ("100", 89..110),  // turns 90-110
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
    println!("\n| N turns | on-disk size (bytes) |");
    println!("|---|---|");
    for (n, sz) in &size_at {
        println!("| {n} | {sz} |");
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
        // Total store size on disk.
        let dir = root_b
            .path()
            .join("conversations")
            .join(store_workspace_id(&store_b, ws.path())?);
        let total: u64 = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        println!("   total on-disk: {total} bytes across {count} records");
    }

    Ok(())
}

/// The store keeps workspace_id private; recompute it the same way it does.
fn store_workspace_id(
    _store: &ConversationStore,
    ws: &std::path::Path,
) -> anyhow::Result<String> {
    ConversationStore::workspace_id_for_path(ws)
}
