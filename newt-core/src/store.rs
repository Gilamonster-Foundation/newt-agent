//! SQLite-backed conversation store — Phase 17.1a (issue #246).
//!
//! Drop-in backend swap for [`crate::conversation::ConversationStore`]: the
//! same public API (`create` / `create_with_id` / `exists` / `append_turn` /
//! `load` / `list` / `rename` / `delete` / `resolve_id`, prefix resolution,
//! workspace scoping, create-time pruning) now backed by a single SQLite
//! database at `<root>/conversations.db` instead of one pretty-printed JSON
//! file per conversation. The JSON code path is untouched this PR — 17.1b
//! performs the one-time import and deletes it.
//!
//! # §6 — ordering is causal, time is a claim (BINDING)
//!
//! Per the mesh-readiness amendment in
//! `docs/design/context-memory-hermes-learnings.md` §6:
//!
//! * **Ordering key:** `(writer_fingerprint, seq)` — a per-writer strictly
//!   monotonic Lamport tick, allocated from the `writer_clock` table inside
//!   the same transaction as the row it orders. "Most recent" is always
//!   `MAX(activity_tick)` / the chain tip — **never** a wall-clock
//!   comparison.
//! * **Content chain:** every turn carries `prev_hash` = BLAKE3 of the prior
//!   turn's canonical encoding (genesis-derived for the first turn), so each
//!   conversation is a per-writer merkle log: the record carries its own
//!   proof of order and tampering is detectable ([`ConversationStore::verify_chain`]).
//! * **Wall-clock columns** (`started_at_claim`, `updated_at_claim`,
//!   `ts_claim`) are **display-only claims**. No query in this module orders,
//!   prunes, or resolves by them.
//!
//! # Writer identity (17.1a scope)
//!
//! `writer_fingerprint` is the BLAKE3 hex of a per-install nonce generated
//! once at `<root>/install-nonce`. It is stable across sessions and distinct
//! across installs, which is exactly what the single-machine Lamport clock
//! needs today. Step 17.2 upgrades this to the agent's mesh-key fingerprint;
//! the column is already shaped for that. Ticks are not yet *signed* — that
//! also arrives with real key material in 17.2.
//!
//! # NFS / concurrency
//!
//! The connection opens with `journal_mode=WAL` + `synchronous=NORMAL`
//! (the SQLite-documented corruption-safe pairing — fsync at checkpoints,
//! not per commit) and falls back to `journal_mode=DELETE` at the default
//! `synchronous=FULL` when SQLite reports the WAL-on-network-filesystem
//! failure modes ("locking protocol" / "disk I/O error" — NFS homes); the
//! captured error is exposed via [`ConversationStore::wal_fallback_notice`]
//! for a user-facing message. A 5 s `busy_timeout` lets two concurrent newt
//! processes share the database; every write happens inside a single
//! `BEGIN IMMEDIATE` transaction so tick allocation, chain extension, and the
//! row insert are atomic.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use crate::conversation::{
    self, session_plan_dir, ConversationRecord, ConversationSummary, ConversationTurn,
};

/// Database file name under the store root (`~/.newt/conversations.db`).
const DB_FILE: &str = "conversations.db";

/// Per-install nonce file under the store root; its BLAKE3 hex is the
/// `writer_fingerprint` (see module docs — 17.2 replaces this with the
/// mesh-key fingerprint).
const NONCE_FILE: &str = "install-nonce";

/// How long a writer waits on a locked database before erroring. Two newts
/// sharing `~/.newt/conversations.db` serialize their write transactions
/// behind this.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Domain-separation prefix for the canonical turn encoding (`prev_hash`
/// chain). Versioned so a future encoding change cannot collide with v1.
const TURN_ENCODING_VERSION: &[u8] = b"newt-turn:v1";

/// Domain-separation prefix for the per-(conversation, writer) genesis hash.
const GENESIS_PREFIX: &[u8] = b"newt-turn-chain-genesis:v1";

/// SQLite-backed conversation store (see module docs).
///
/// Cheap to clone: clones share one connection behind a mutex. All methods
/// take `&self`, matching the JSON-backed predecessor.
#[derive(Debug, Clone)]
pub struct ConversationStore {
    conn: Arc<Mutex<Connection>>,
    workspace: PathBuf,
    workspace_id: String,
    writer_fingerprint: String,
    max_per_workspace: usize,
    /// `Some(captured sqlite error)` when WAL was refused and the store fell
    /// back to `journal_mode=DELETE` (NFS homes). Surface this to the user.
    wal_fallback: Option<String>,
    /// Wall-clock source for the display-only `*_claim` columns. Injectable
    /// so tests can drive the clock backwards mid-conversation and prove
    /// ordering never consults it (§6 clock-skew test).
    claim_clock: fn() -> i64,
}

impl ConversationStore {
    /// Open (creating if needed) the store at `<root>/conversations.db`,
    /// scoped to `workspace`. `max_per_workspace` is the create-time prune
    /// cap (0 = no pruning), identical to the JSON backend.
    pub fn new(
        root: impl AsRef<Path>,
        workspace: impl AsRef<Path>,
        max_per_workspace: usize,
    ) -> anyhow::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let workspace = std::fs::canonicalize(workspace.as_ref())?;
        let workspace_id = Self::workspace_id_for_path(&workspace)?;
        std::fs::create_dir_all(&root)?;
        let writer_fingerprint = load_or_create_writer_fingerprint(&root)?;

        let conn = Connection::open(root.join(DB_FILE))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let wal_fallback = apply_journal_mode(&conn)?;
        create_schema(&conn)?;
        reconcile_schema(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            workspace,
            workspace_id,
            writer_fingerprint,
            max_per_workspace,
            wal_fallback,
            claim_clock: now_claim_nanos,
        })
    }

    /// Stable workspace key for a path. Today this is the same UUIDv5
    /// derivation as the JSON backend (single-sourced from it); step 17.2
    /// replaces the derivation with `blake3(git remote + branch)`.
    pub fn workspace_id_for_path(path: impl AsRef<Path>) -> anyhow::Result<String> {
        conversation::ConversationStore::workspace_id_for_path(path)
    }

    /// `Some(error text)` when the database refused WAL and the store is
    /// running on the `journal_mode=DELETE` fallback (typical for NFS
    /// homes). Callers should surface this once to the user.
    pub fn wal_fallback_notice(&self) -> Option<&str> {
        self.wal_fallback.as_deref()
    }

    /// This install's writer fingerprint — the `writer_fingerprint` half of
    /// the §6 `(writer_fingerprint, seq)` ordering key.
    pub fn writer_fingerprint(&self) -> &str {
        &self.writer_fingerprint
    }

    /// Create a conversation with a freshly minted id; returns the id.
    pub fn create(&self, title: &str, persona: Option<&str>) -> anyhow::Result<String> {
        let id = conversation::new_conversation_id();
        self.create_with_id(&id, title, persona)?;
        Ok(id)
    }

    /// Create a conversation record using a caller-supplied `id`.
    ///
    /// The TUI pre-generates a conversation id at session start (so the
    /// per-session plan path is stable from turn 1, see issue #220) and the
    /// record adopts that id when the first turn is saved — same lazy-create
    /// contract as the JSON backend.
    pub fn create_with_id(
        &self,
        id: &str,
        title: &str,
        persona: Option<&str>,
    ) -> anyhow::Result<()> {
        validate_record_id(id)?;
        let now = (self.claim_clock)();
        {
            let conn = self.lock_conn();
            let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
            // Workspace fence: `id` is a GLOBAL primary key and REPLACE fires
            // `ON DELETE CASCADE` — without this check, re-creating an id that
            // belongs to ANOTHER workspace would silently destroy that
            // workspace's conversation and all its turns. Same-workspace
            // REPLACE keeps JSON-backend parity (re-create = overwrite).
            let foreign: Option<String> = tx
                .query_row(
                    "SELECT workspace_key FROM conversations WHERE id = ?1",
                    rusqlite::params![id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(owner) = foreign {
                if owner != self.workspace_id {
                    anyhow::bail!(
                        "conversation id `{id}` already exists in another workspace \
                         (key {owner}); refusing to overwrite across the workspace fence"
                    );
                }
            }
            let tick = next_tick(&tx, &self.writer_fingerprint)?;
            // INSERT OR REPLACE mirrors the JSON backend, where re-creating an
            // existing id overwrote the record (turns reset). The REPLACE
            // deletes the old row, and `ON DELETE CASCADE` drops its turns —
            // safe only because of the fence above.
            tx.execute(
                "INSERT OR REPLACE INTO conversations
                   (id, title, workspace_path, workspace_key, persona, end_reason,
                    writer_fingerprint, activity_tick, tip_hash,
                    started_at_claim, updated_at_claim)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?9)",
                rusqlite::params![
                    id,
                    title.trim(),
                    self.workspace.to_string_lossy(),
                    self.workspace_id,
                    persona,
                    self.writer_fingerprint,
                    tick,
                    genesis_hash(id, &self.writer_fingerprint),
                    now,
                ],
            )?;
            tx.commit()?;
        }
        self.prune_to_cap()?;
        Ok(())
    }

    /// `true` if a record for exactly `id` exists in this workspace. Used by
    /// the save path to decide between [`create_with_id`](Self::create_with_id)
    /// (first turn) and [`append_turn`](Self::append_turn).
    ///
    /// Errors propagate rather than read as "absent": a transient failure
    /// (e.g. a busy reader past the timeout under the NFS DELETE fallback)
    /// mistaken for "doesn't exist" would route the caller into
    /// `create_with_id` and overwrite a live conversation.
    pub fn exists(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT 1 FROM conversations WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id, self.workspace_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Append one `(user, assistant)` turn. `id` may be a unique prefix.
    ///
    /// One `BEGIN IMMEDIATE` transaction covers: tick allocation, chain
    /// extension (`prev_hash` from the current per-writer tip), the row
    /// insert, and the conversation's activity/tip update. Appending never
    /// prunes — only `create` does, matching the JSON backend.
    pub fn append_turn(&self, id: &str, user: &str, assistant: &str) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let tick = next_tick(&tx, &self.writer_fingerprint)?;

        // The §6 content chain: hash the canonical encoding of this writer's
        // previous turn (re-derived from the row itself, so a drifted
        // `tip_hash` column can never poison the chain).
        let prev_hash = match last_turn(&tx, &id, &self.writer_fingerprint)? {
            Some(prev) => prev.content_hash(),
            None => genesis_hash(&id, &self.writer_fingerprint),
        };

        let row = TurnRow {
            conversation_id: id.clone(),
            writer_fingerprint: self.writer_fingerprint.clone(),
            seq: tick,
            prev_hash,
            user: user.to_string(),
            assistant: assistant.to_string(),
            events: "[]".to_string(),
            tokens_in: None,
            tokens_out: None,
            ts_claim: now,
        };
        tx.execute(
            "INSERT INTO turns
               (conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                events, tokens_in, tokens_out, ts_claim)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                row.conversation_id,
                row.writer_fingerprint,
                row.seq,
                row.prev_hash,
                row.user,
                row.assistant,
                row.events,
                row.tokens_in,
                row.tokens_out,
                row.ts_claim,
            ],
        )?;
        // Activity tick + chain tip move together; updated_at_claim is a
        // display claim only (§6) — nothing orders by it.
        tx.execute(
            "UPDATE conversations
                SET writer_fingerprint = ?2, activity_tick = ?3, tip_hash = ?4,
                    updated_at_claim = ?5
              WHERE id = ?1",
            rusqlite::params![id, self.writer_fingerprint, tick, row.content_hash(), now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Load a full record (turns in causal `(writer, seq)` order). `id` may
    /// be a unique prefix.
    pub fn load(&self, id: &str) -> anyhow::Result<ConversationRecord> {
        let id = self.resolve_id(id)?;
        let conn = self.lock_conn();
        let mut record = conn
            .query_row(
                "SELECT id, title, workspace_path, workspace_key, persona,
                        started_at_claim, updated_at_claim
                   FROM conversations
                  WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id, self.workspace_id],
                |row| {
                    Ok(ConversationRecord {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        workspace: row.get(2)?,
                        workspace_id: row.get(3)?,
                        persona: row.get(4)?,
                        turns: Vec::new(),
                        created_at_unix_nanos: claim_to_u128(row.get(5)?),
                        updated_at_unix_nanos: claim_to_u128(row.get(6)?),
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("conversation `{id}` not found"))?;

        // §6: turn order is the causal tick, never ts_claim.
        let mut stmt = conn.prepare(
            "SELECT user, assistant FROM turns
              WHERE conversation_id = ?1
              ORDER BY seq ASC, writer_fingerprint ASC",
        )?;
        let turns = stmt.query_map([&id], |row| {
            Ok(ConversationTurn::new(
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
            ))
        })?;
        for turn in turns {
            record.turns.push(turn?);
        }
        Ok(record)
    }

    /// All conversations in this workspace, least-recently-active first —
    /// "active" meaning the §6 activity tick, never a timestamp. The
    /// summaries' `updated_at_unix_nanos` is the display claim.
    pub fn list(&self) -> anyhow::Result<Vec<ConversationSummary>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.title, c.persona, c.updated_at_claim,
                    (SELECT COUNT(*) FROM turns t WHERE t.conversation_id = c.id)
               FROM conversations c
              WHERE c.workspace_key = ?1
              ORDER BY c.activity_tick ASC, c.id ASC",
        )?;
        let rows = stmt.query_map([&self.workspace_id], |row| {
            Ok(ConversationSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                persona: row.get(2)?,
                updated_at_unix_nanos: claim_to_u128(row.get(3)?),
                turn_count: row.get::<_, i64>(4)?.max(0) as usize,
            })
        })?;
        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(row?);
        }
        Ok(summaries)
    }

    /// Rename a conversation. Updates the display claim but does NOT tick
    /// the activity clock: a rename is metadata, not activity, so it cannot
    /// perturb MRU ordering (§6 dissolved the old rename-bumps-`updated_at`
    /// defect, design doc §1).
    pub fn rename(&self, id: &str, title: &str) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE conversations SET title = ?2, updated_at_claim = ?3 WHERE id = ?1",
            rusqlite::params![id, title.trim(), now],
        )?;
        Ok(())
    }

    /// Delete a conversation (its turns cascade) and, best-effort, its
    /// per-session plan dir (issue #220).
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        {
            let conn = self.lock_conn();
            conn.execute(
                "DELETE FROM conversations WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id, self.workspace_id],
            )?;
        }
        // Ignore errors: the dir may not exist, and a stray plan must never
        // block deletion of the record.
        let plan_dir = self.workspace.join(session_plan_dir(&id));
        let _ = std::fs::remove_dir_all(plan_dir);
        Ok(())
    }

    /// Resolve an exact id or unique prefix within this workspace.
    pub fn resolve_id(&self, id_or_prefix: &str) -> anyhow::Result<String> {
        validate_record_id(id_or_prefix)?;
        let conn = self.lock_conn();
        let exact = conn
            .query_row(
                "SELECT id FROM conversations WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id_or_prefix, self.workspace_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(id) = exact {
            return Ok(id);
        }
        // Ids are validated to alphanumeric + '-' above, so the prefix can
        // never contain LIKE metacharacters.
        let mut stmt = conn.prepare(
            "SELECT id FROM conversations
              WHERE workspace_key = ?1 AND id LIKE ?2
              ORDER BY id ASC",
        )?;
        let matches = stmt
            .query_map(
                rusqlite::params![self.workspace_id, format!("{id_or_prefix}%")],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        match matches.as_slice() {
            [id] => Ok(id.clone()),
            [] => anyhow::bail!("conversation `{id_or_prefix}` not found"),
            many => anyhow::bail!(
                "ambiguous conversation id prefix `{}`; matches: {}",
                id_or_prefix,
                many.join(", ")
            ),
        }
    }

    /// Verify the §6 content chain for a conversation: every writer's turns
    /// must link `prev_hash` → BLAKE3(prior turn's canonical encoding) from
    /// the genesis hash, and the stored chain tip must match this writer's
    /// last turn. A tampered row (content OR claims — claims are inside the
    /// canonical encoding, so they are tamper-evident too) breaks the chain.
    pub fn verify_chain(&self, id: &str) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let conn = self.lock_conn();
        let (tip, tip_writer): (String, String) = conn.query_row(
            "SELECT tip_hash, writer_fingerprint FROM conversations WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let mut stmt = conn.prepare(
            "SELECT conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                    events, tokens_in, tokens_out, ts_claim
               FROM turns
              WHERE conversation_id = ?1
              ORDER BY writer_fingerprint ASC, seq ASC",
        )?;
        let rows = stmt
            .query_map([&id], turn_row_from_sql)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut prev: Option<&TurnRow> = None;
        for row in &rows {
            let same_writer = prev.is_some_and(|p| p.writer_fingerprint == row.writer_fingerprint);
            if same_writer {
                let p = prev.expect("same_writer implies prev");
                if row.seq <= p.seq {
                    anyhow::bail!(
                        "chain violation in `{id}`: seq {} not strictly after {}",
                        row.seq,
                        p.seq
                    );
                }
                if row.prev_hash != p.content_hash() {
                    anyhow::bail!(
                        "chain violation in `{id}`: turn seq {} does not link to seq {} \
                         (row tampered or out of order)",
                        row.seq,
                        p.seq
                    );
                }
            } else {
                let genesis = genesis_hash(&id, &row.writer_fingerprint);
                if row.prev_hash != genesis {
                    anyhow::bail!(
                        "chain violation in `{id}`: first turn of writer {} (seq {}) does \
                         not link to the genesis hash",
                        row.writer_fingerprint,
                        row.seq
                    );
                }
            }
            prev = Some(row);
        }

        // The stored tip must match the chain of the conversation row's
        // RECORDED last writer (set at create, updated on every append in
        // the same txn) — not whoever happens to be verifying. This keeps
        // verify_chain writer-agnostic: a store that authored no turns in a
        // migrated/foreign conversation still verifies it correctly
        // (adversarial-review finding N2 on #261).
        let expected_tip = rows
            .iter()
            .rfind(|r| r.writer_fingerprint == tip_writer)
            .map_or_else(|| genesis_hash(&id, &tip_writer), TurnRow::content_hash);
        if tip != expected_tip {
            anyhow::bail!("chain violation in `{id}`: stored tip_hash does not match the chain");
        }
        Ok(())
    }

    /// Drive the display-claim clock from a test. Hidden, test-only: lets the
    /// §6 clock-skew test write *honestly skewed* claims through the normal
    /// API (clock runs backwards mid-conversation) and prove that ordering,
    /// MRU, and chain verification are all unaffected.
    #[doc(hidden)]
    pub fn set_claim_clock_for_test(&mut self, clock: fn() -> i64) {
        self.claim_clock = clock;
    }

    fn prune_to_cap(&self) -> anyhow::Result<()> {
        if self.max_per_workspace == 0 {
            return Ok(());
        }
        let victims: Vec<String> = {
            let conn = self.lock_conn();
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM conversations WHERE workspace_key = ?1",
                [&self.workspace_id],
                |row| row.get(0),
            )?;
            let excess = count - self.max_per_workspace as i64;
            if excess <= 0 {
                return Ok(());
            }
            // Oldest = lowest activity tick (§6 — never a timestamp).
            let mut stmt = conn.prepare(
                "SELECT id FROM conversations
                  WHERE workspace_key = ?1
                  ORDER BY activity_tick ASC, id ASC
                  LIMIT ?2",
            )?;
            let ids = stmt
                .query_map(rusqlite::params![self.workspace_id, excess], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };
        for id in victims {
            // Route through delete() so plan dirs are cleaned up too.
            self.delete(&id)?;
        }
        Ok(())
    }

    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned mutex means another thread panicked mid-operation; the
        // connection itself is still usable (transactions roll back), so
        // recover rather than cascade the panic.
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One turn row, exactly as stored. Internal: the canonical encoding hashes
/// every field, so this struct is the unit of chain verification.
#[derive(Debug)]
struct TurnRow {
    conversation_id: String,
    writer_fingerprint: String,
    seq: i64,
    prev_hash: String,
    user: String,
    assistant: String,
    events: String,
    tokens_in: Option<i64>,
    tokens_out: Option<i64>,
    ts_claim: i64,
}

impl TurnRow {
    /// BLAKE3 hex of this row's canonical encoding — what the *next* turn's
    /// `prev_hash` must equal.
    fn content_hash(&self) -> String {
        blake3::hash(&self.canonical_encoding())
            .to_hex()
            .to_string()
    }

    /// Canonical byte encoding of a turn: version tag, then every field
    /// length-prefixed (u64 LE) so adjacent fields can never be reparsed
    /// ambiguously (`("ab","c")` ≠ `("a","bc")`). Integers are 8-byte LE
    /// with a presence byte for the optional token counts.
    fn canonical_encoding(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            64 + self.conversation_id.len()
                + self.writer_fingerprint.len()
                + self.prev_hash.len()
                + self.user.len()
                + self.assistant.len()
                + self.events.len(),
        );
        out.extend_from_slice(TURN_ENCODING_VERSION);
        for field in [
            self.conversation_id.as_bytes(),
            self.writer_fingerprint.as_bytes(),
            self.prev_hash.as_bytes(),
            self.user.as_bytes(),
            self.assistant.as_bytes(),
            self.events.as_bytes(),
        ] {
            out.extend_from_slice(&(field.len() as u64).to_le_bytes());
            out.extend_from_slice(field);
        }
        out.extend_from_slice(&self.seq.to_le_bytes());
        for opt in [self.tokens_in, self.tokens_out] {
            match opt {
                Some(v) => {
                    out.push(1);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                None => out.push(0),
            }
        }
        out.extend_from_slice(&self.ts_claim.to_le_bytes());
        out
    }
}

fn turn_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnRow> {
    Ok(TurnRow {
        conversation_id: row.get(0)?,
        writer_fingerprint: row.get(1)?,
        seq: row.get(2)?,
        prev_hash: row.get(3)?,
        user: row.get(4)?,
        assistant: row.get(5)?,
        events: row.get(6)?,
        tokens_in: row.get(7)?,
        tokens_out: row.get(8)?,
        ts_claim: row.get(9)?,
    })
}

/// The §6 genesis hash anchoring a writer's chain within a conversation.
fn genesis_hash(conversation_id: &str, writer_fingerprint: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(GENESIS_PREFIX);
    for field in [conversation_id.as_bytes(), writer_fingerprint.as_bytes()] {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    hasher.finalize().to_hex().to_string()
}

/// This writer's most recent turn in a conversation (chain tip source).
fn last_turn(
    conn: &Connection,
    conversation_id: &str,
    writer_fingerprint: &str,
) -> anyhow::Result<Option<TurnRow>> {
    Ok(conn
        .query_row(
            "SELECT conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                    events, tokens_in, tokens_out, ts_claim
               FROM turns
              WHERE conversation_id = ?1 AND writer_fingerprint = ?2
              ORDER BY seq DESC
              LIMIT 1",
            rusqlite::params![conversation_id, writer_fingerprint],
            turn_row_from_sql,
        )
        .optional()?)
}

/// Allocate the next per-writer Lamport tick (strictly monotonic — §6 floor).
///
/// Must run inside the caller's `BEGIN IMMEDIATE` transaction so the
/// read-modify-write is atomic across concurrent writers sharing the db.
///
/// When the writer has no clock row yet (fresh db, or `writer_clock` lost to
/// schema drift), the seed is the **global** max tick already present in the
/// database — the Lamport receive rule: a clock never starts behind any tick
/// it has observed, so cross-writer `activity_tick` comparisons on a shared
/// db stay causally meaningful and a re-seeded clock can never reuse a seq.
/// The seeding scan runs only on clock-row creation, never per append.
fn next_tick(conn: &Connection, writer_fingerprint: &str) -> anyhow::Result<i64> {
    let bumped = conn.execute(
        "UPDATE writer_clock SET last_tick = last_tick + 1 WHERE writer_fingerprint = ?1",
        [writer_fingerprint],
    )?;
    if bumped == 0 {
        conn.execute(
            "INSERT OR IGNORE INTO writer_clock (writer_fingerprint, last_tick)
             SELECT ?1, COALESCE(MAX(t), 0) FROM (
                 SELECT MAX(seq) AS t FROM turns
                 UNION ALL
                 SELECT MAX(activity_tick) AS t FROM conversations
                 UNION ALL
                 -- Other writers' issued ticks: keeps the seed at the true
                 -- issued-max even when their rows were pruned (review
                 -- finding N6 on #261 — Lamport receive rule over all
                 -- observable evidence, not just surviving rows).
                 SELECT MAX(last_tick) AS t FROM writer_clock
             )",
            [writer_fingerprint],
        )?;
        conn.execute(
            "UPDATE writer_clock SET last_tick = last_tick + 1 WHERE writer_fingerprint = ?1",
            [writer_fingerprint],
        )?;
    }
    Ok(conn.query_row(
        "SELECT last_tick FROM writer_clock WHERE writer_fingerprint = ?1",
        [writer_fingerprint],
        |row| row.get(0),
    )?)
}

/// Try WAL; fall back to DELETE on the known network-filesystem failure
/// modes, returning the captured error text for a user-facing notice.
/// Any other error is real and propagates.
///
/// Under WAL, `synchronous` drops to NORMAL: SQLite documents WAL +
/// NORMAL as corruption-safe (fsync at checkpoints, not per commit), and
/// per-append cost falls from ~2 ms (one fsync per turn) to tens of µs —
/// a power cut can cost the last turns, never the database. The DELETE
/// fallback keeps the FULL default, where NORMAL is *not* corruption-safe.
fn apply_journal_mode(conn: &Connection) -> anyhow::Result<Option<String>> {
    let wal: Result<String, rusqlite::Error> =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0));
    match wal {
        // Assert the pragma actually took (it has documented silent-no-op
        // cases) — NORMAL is only safe under WAL; any other mode keeps the
        // compiled default of FULL (review finding N4 on #261).
        Ok(mode) if mode.eq_ignore_ascii_case("wal") => {
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            Ok(None)
        }
        Ok(mode) => {
            tracing::warn!(%mode, "journal_mode=WAL did not take; keeping synchronous=FULL");
            Ok(Some(format!("journal_mode pragma returned `{mode}`")))
        }
        Err(e) if wal_fallback_eligible(&e.to_string()) => {
            let captured = e.to_string();
            conn.pragma_update(None, "journal_mode", "DELETE")?;
            tracing::warn!(
                error = %captured,
                "SQLite refused WAL (network filesystem?); conversations.db is running \
                 on the slower journal_mode=DELETE fallback"
            );
            Ok(Some(captured))
        }
        Err(e) => Err(e.into()),
    }
}

/// `true` for the SQLite error texts WAL is known to produce on filesystems
/// without shared-memory mmap / POSIX lock support (NFS homes): the store
/// should fall back to `journal_mode=DELETE` rather than fail to open.
fn wal_fallback_eligible(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    lower.contains("locking protocol") || lower.contains("disk i/o error")
}

/// Schema, v17.1a. §6-binding shape — see the module docs. Every `*_claim`
/// column is a DISPLAY-ONLY wall-clock claim (unix nanos): never an ordering
/// key, never compared. Ordering is `(writer_fingerprint, seq)` /
/// `activity_tick`; integrity is the `prev_hash` BLAKE3 chain + `tip_hash`.
/// `events`/`tokens_in`/`tokens_out` are day-one columns filled by 17.6.
fn create_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS conversations (
             id                 TEXT PRIMARY KEY,
             title              TEXT NOT NULL,
             workspace_path     TEXT NOT NULL,            -- display only
             workspace_key      TEXT NOT NULL,            -- scoping key (17.2: blake3(remote+branch))
             persona            TEXT,
             end_reason         TEXT,                     -- set by 17.7
             writer_fingerprint TEXT NOT NULL,            -- §6 ordering key, half 1
             activity_tick      INTEGER NOT NULL,         -- §6 ordering key, half 2 (per-writer Lamport tick)
             tip_hash           TEXT NOT NULL,            -- §6 chain tip (BLAKE3)
             started_at_claim   INTEGER NOT NULL,         -- DISPLAY ONLY (wall-clock claim, unix nanos)
             updated_at_claim   INTEGER NOT NULL          -- DISPLAY ONLY
         );
         CREATE TABLE IF NOT EXISTS turns (
             conversation_id    TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
             writer_fingerprint TEXT NOT NULL,            -- §6: whose clock ticked
             seq                INTEGER NOT NULL,         -- §6: strictly monotonic per writer — THE ordering key
             prev_hash          TEXT NOT NULL,            -- §6: BLAKE3 of prior turn's canonical encoding
             user               TEXT NOT NULL,
             assistant          TEXT NOT NULL,
             events             TEXT NOT NULL DEFAULT '[]', -- JSON tool events; filled by 17.6
             tokens_in          INTEGER,                  -- filled by 17.6, consumed by 18.x
             tokens_out         INTEGER,
             ts_claim           INTEGER NOT NULL,         -- DISPLAY ONLY (wall-clock claim, unix nanos)
             PRIMARY KEY (conversation_id, writer_fingerprint, seq)
         );
         -- The per-writer Lamport clock (§6 'each agent is its own clock').
         CREATE TABLE IF NOT EXISTS writer_clock (
             writer_fingerprint TEXT PRIMARY KEY,
             last_tick          INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_conversations_ws_tick
             ON conversations (workspace_key, activity_tick);",
    )?;
    Ok(())
}

/// Expected columns per table, with ALTER-safe declarations (additive
/// schema-diff reconciliation: a db written by an older newt gains any
/// missing columns on open; unknown extra columns are left alone).
const EXPECTED_COLUMNS: &[(&str, &[(&str, &str)])] = &[
    (
        "conversations",
        &[
            ("id", "TEXT"),
            ("title", "TEXT NOT NULL DEFAULT ''"),
            ("workspace_path", "TEXT NOT NULL DEFAULT ''"),
            ("workspace_key", "TEXT NOT NULL DEFAULT ''"),
            ("persona", "TEXT"),
            ("end_reason", "TEXT"),
            ("writer_fingerprint", "TEXT NOT NULL DEFAULT ''"),
            ("activity_tick", "INTEGER NOT NULL DEFAULT 0"),
            ("tip_hash", "TEXT NOT NULL DEFAULT ''"),
            ("started_at_claim", "INTEGER NOT NULL DEFAULT 0"),
            ("updated_at_claim", "INTEGER NOT NULL DEFAULT 0"),
        ],
    ),
    (
        "turns",
        &[
            ("conversation_id", "TEXT"),
            ("writer_fingerprint", "TEXT NOT NULL DEFAULT ''"),
            ("seq", "INTEGER NOT NULL DEFAULT 0"),
            ("prev_hash", "TEXT NOT NULL DEFAULT ''"),
            ("user", "TEXT NOT NULL DEFAULT ''"),
            ("assistant", "TEXT NOT NULL DEFAULT ''"),
            ("events", "TEXT NOT NULL DEFAULT '[]'"),
            ("tokens_in", "INTEGER"),
            ("tokens_out", "INTEGER"),
            ("ts_claim", "INTEGER NOT NULL DEFAULT 0"),
        ],
    ),
    (
        "writer_clock",
        &[
            ("writer_fingerprint", "TEXT"),
            ("last_tick", "INTEGER NOT NULL DEFAULT 0"),
        ],
    ),
];

/// Compare `PRAGMA table_info` against [`EXPECTED_COLUMNS`] and `ALTER TABLE
/// ... ADD COLUMN` any additive drift. Removed/renamed columns are NOT
/// handled here — destructive migrations get their own explicit step.
fn reconcile_schema(conn: &Connection) -> anyhow::Result<()> {
    for (table, expected) in EXPECTED_COLUMNS {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let present: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;
        for (name, decl) in *expected {
            if !present.iter().any(|c| c == name) {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {decl}"))?;
                tracing::info!(
                    table = *table,
                    column = *name,
                    "schema migration: added missing column"
                );
            }
        }
    }
    Ok(())
}

/// Load (or mint, atomically) the per-install nonce and derive the writer
/// fingerprint as its BLAKE3 hex. See module docs — 17.2 upgrades this to
/// the agent's mesh-key fingerprint.
fn load_or_create_writer_fingerprint(root: &Path) -> anyhow::Result<String> {
    let path = root.join(NONCE_FILE);
    let nonce = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
        _ => {
            // Mint with O_EXCL (`create_new`) so two racing first-run
            // processes converge on ONE nonce: exactly one create succeeds;
            // the loser sees AlreadyExists and adopts the winner's nonce.
            // (Write-then-rename is NOT safe here: rename replaces the file,
            // so a slow racer could overwrite the winner's nonce after the
            // winner already derived its fingerprint — permanently orphaning
            // that writer's rows. The writer identity must mint exactly once.)
            use std::io::Write as _;
            let minted = uuid::Uuid::new_v4().to_string();
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut f) => {
                    f.write_all(minted.as_bytes())?;
                    minted
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let adopted = std::fs::read_to_string(&path)?.trim().to_string();
                    if adopted.is_empty() {
                        anyhow::bail!(
                            "install nonce at {} exists but is empty — remove it and retry",
                            path.display()
                        );
                    }
                    adopted
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    Ok(blake3::hash(nonce.as_bytes()).to_hex().to_string())
}

/// Wall-clock for the display-only claim columns. Saturates at `i64::MAX`
/// (year 2262 in unix nanos) — claims are never compared, so saturation is
/// harmless by construction.
fn now_claim_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn claim_to_u128(claim: i64) -> u128 {
    claim.max(0) as u128
}

/// Same id alphabet as the JSON backend (alphanumeric + '-'). SQL parameters
/// make injection moot, but the shared alphabet keeps ids portable across
/// both backends until 17.1b retires the JSON one.
fn validate_record_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("invalid conversation id `{id}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_fallback_classifier_matches_known_nfs_failures() {
        assert!(wal_fallback_eligible("locking protocol"));
        assert!(wal_fallback_eligible("disk I/O error"));
        assert!(wal_fallback_eligible(
            "sqlite failure: `Error code 15: Locking Protocol`"
        ));
        assert!(!wal_fallback_eligible("no such table: turns"));
        assert!(!wal_fallback_eligible("database is locked"));
        assert!(!wal_fallback_eligible(""));
    }

    #[test]
    fn canonical_encoding_is_unambiguous_across_field_boundaries() {
        let base = TurnRow {
            conversation_id: "c".into(),
            writer_fingerprint: "w".into(),
            seq: 1,
            prev_hash: "p".into(),
            user: "ab".into(),
            assistant: "c".into(),
            events: "[]".into(),
            tokens_in: None,
            tokens_out: None,
            ts_claim: 7,
        };
        let shifted = TurnRow {
            user: "a".into(),
            assistant: "bc".into(),
            ..clone_row(&base)
        };
        assert_ne!(
            base.canonical_encoding(),
            shifted.canonical_encoding(),
            "length prefixes must prevent (ab,c) == (a,bc)"
        );
        // Every field participates in the hash — including the claims and
        // token counts, which makes even display fields tamper-evident.
        let skewed = TurnRow {
            ts_claim: 8,
            ..clone_row(&base)
        };
        assert_ne!(base.content_hash(), skewed.content_hash());
        let tokens = TurnRow {
            tokens_in: Some(5),
            ..clone_row(&base)
        };
        assert_ne!(base.content_hash(), tokens.content_hash());
    }

    #[test]
    fn genesis_hash_is_deterministic_and_writer_scoped() {
        assert_eq!(genesis_hash("conv", "w1"), genesis_hash("conv", "w1"));
        assert_ne!(genesis_hash("conv", "w1"), genesis_hash("conv", "w2"));
        assert_ne!(genesis_hash("conv", "w1"), genesis_hash("other", "w1"));
        // Length-prefixing: ("ab","c") must not collide with ("a","bc").
        assert_ne!(genesis_hash("ab", "c"), genesis_hash("a", "bc"));
    }

    #[test]
    fn writer_fingerprint_is_stable_per_install_and_distinct_across_installs() {
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        let first = load_or_create_writer_fingerprint(root_a.path()).unwrap();
        let again = load_or_create_writer_fingerprint(root_a.path()).unwrap();
        let other = load_or_create_writer_fingerprint(root_b.path()).unwrap();
        assert_eq!(first, again, "fingerprint must be stable per install");
        assert_ne!(first, other, "two installs must not share a fingerprint");
        assert_eq!(first.len(), 64, "blake3 hex");
    }

    #[test]
    fn wal_mode_pairs_with_synchronous_normal_on_the_stores_connection() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        // `synchronous` is per-connection, so ask the store's own connection
        // (a fresh external connection would only show its own default).
        let conn = store.lock_conn();
        let sync_level: i64 = conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sync_level, 1, "WAL must run at synchronous=NORMAL (1)");
    }

    #[test]
    fn claim_clock_saturates_instead_of_wrapping() {
        let now = now_claim_nanos();
        assert!(now > 0);
        assert_eq!(claim_to_u128(-5), 0);
        assert_eq!(claim_to_u128(42), 42);
    }

    fn clone_row(row: &TurnRow) -> TurnRow {
        TurnRow {
            conversation_id: row.conversation_id.clone(),
            writer_fingerprint: row.writer_fingerprint.clone(),
            seq: row.seq,
            prev_hash: row.prev_hash.clone(),
            user: row.user.clone(),
            assistant: row.assistant.clone(),
            events: row.events.clone(),
            tokens_in: row.tokens_in,
            tokens_out: row.tokens_out,
            ts_claim: row.ts_claim,
        }
    }
}
