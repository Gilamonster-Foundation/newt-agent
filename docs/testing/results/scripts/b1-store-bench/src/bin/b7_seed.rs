//! B7 — conversation-store seeder, AFTER harness (issue #245 follow-up).
//!
//! The baseline seeder (`scripts/b7_seed_store.py`) wrote per-conversation
//! JSON files matching the JSON backend's schema. The store is SQLite now
//! (17.1a), so the after-seeder drives the REAL `ConversationStore` API —
//! same N conversations × 10 turns of equivalent synthetic payload — into a
//! sandbox HOME's `~/.newt/conversations.db`.
//!
//! Usage:
//!   b7_seed --home /tmp/newt-bench/b7/home-1000 --workspace /tmp/newt-bench/b7/ws --count 1000
//!
//! NOT a workspace member, NOT in CI (scratch bench bin; see ../Cargo.toml).

use newt_core::ConversationStore;

fn turn(i: usize) -> (String, String) {
    let user = format!("turn {i}: please fix the failing test and re-run cargo test until green.");
    let assistant = format!(
        "Read the file, found the bug. {}tool_event: {{\"name\":\"run_command\",\"digest\":\"abcd{i:04}\"}}",
        "context survives across sessions; ".repeat(40)
    );
    (user, assistant)
}

fn main() -> anyhow::Result<()> {
    let mut home = None;
    let mut workspace = None;
    let mut count = None;
    let mut turns = 10usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--home" => home = args.next(),
            "--workspace" => workspace = args.next(),
            "--count" => count = args.next().and_then(|v| v.parse::<usize>().ok()),
            "--turns" => turns = args.next().and_then(|v| v.parse().ok()).unwrap_or(10),
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }
    let home = home.ok_or_else(|| anyhow::anyhow!("--home required"))?;
    let workspace = workspace.ok_or_else(|| anyhow::anyhow!("--workspace required"))?;
    let count = count.ok_or_else(|| anyhow::anyhow!("--count required"))?;

    let root = std::path::Path::new(&home).join(".newt");
    std::fs::create_dir_all(&root)?;
    let store = ConversationStore::new(&root, &workspace, 0)?;
    for c in 0..count {
        let id = store.create(&format!("synthetic conversation {c}"), None)?;
        for t in 0..turns {
            let (u, a) = turn(c * turns + t);
            store.append_turn(&id, &u, &a)?;
        }
    }
    let db = std::fs::metadata(root.join("conversations.db")).map(|m| m.len())?;
    let wal = std::fs::metadata(root.join("conversations.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "seeded {count} conversations ({turns} turns each) into {}/conversations.db: \
         db {db} bytes + wal {wal} bytes",
        root.display()
    );
    Ok(())
}
