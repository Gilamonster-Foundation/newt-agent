//! SQLite-backed conversation store — Phase 17.1a/17.1b (issue #246).
// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 18:33 EDT | Date: 2026-08-12
//!
//! The only conversation backend: the same public API the JSON-file store
//! established (`create` / `create_with_id` / `exists` / `append_turn` /
//! `load` / `list` / `rename` / `delete` / `resolve_id`, prefix resolution,
//! workspace scoping, create-time pruning) backed by a single SQLite
//! database at `<root>/conversations.db`. The legacy per-conversation JSON
//! tree (`<root>/conversations/<workspace-uuid>/<id>.json`) is imported once
//! on open and kept as a backup — see [One-time JSON import](#one-time-json-import-171b).
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
//! **Honesty note on the envelope (17.1b, review NIT N3 on #261):** the
//! tamper-evident envelope covers the `turns` rows and the stored chain tip
//! — nothing else. Conversation-row metadata (`title`, `activity_tick`, the
//! `*_claim` columns, persona) can be edited in place undetectably with any
//! SQLite client. 17.2 derives the writer fingerprint from real key material
//! when it exists (below), but ticks are still not *signed* — so this
//! integrity story remains anti-naive-edit, not anti-adversary, until a
//! future step adds signatures.
//!
//! # Workspace identity v2 (17.2)
//!
//! The `workspace_key` scoping column is the v2 derivation
//! ([`crate::workspace_key::workspace_key_v2`]): BLAKE3 hex of
//! `(git origin URL, branch)` when the workspace is a git checkout with
//! both, else BLAKE3 hex of the canonical path. Two clones of the same
//! project on the same branch therefore *share* conversations — the
//! decision doc's "folder = conversation across clones and containers"
//! thesis — while non-git dirs keep per-path scoping.
//!
//! **Row migration:** on open, any conversation whose `workspace_key`
//! equals THIS workspace's retired UUIDv5 key
//! ([`ConversationStore::workspace_id_for_path`], kept for exactly this
//! lookup) is re-keyed to the v2 key in one idempotent UPDATE. Other
//! workspaces' rows are untouched — they migrate when their own workspace
//! next opens, because only that open knows the path the UUIDv5 key was
//! derived from (the hash is not reversible). The key is not part of the
//! §6 turn encoding or genesis hash, so re-keying cannot disturb chain
//! verification.
//!
//! # One-time JSON import (17.1b)
//!
//! On open, if the retired JSON backend's tree exists at
//! `<root>/conversations/`, every readable record in every per-workspace
//! UUID dir is imported into SQLite — all workspaces under the root, not
//! just the opening store's (the files carry their workspace identity in
//! the dir name and the record body). Turns get ticks through the normal
//! `next_tick` path in legacy MRU order (ascending `updated_at`), so
//! post-import MRU matches what the JSON backend would have shown; the
//! legacy `unix_nanos` fields are ingested **only** as display claims
//! (`*_claim` / `ts_claim` — §6). The chain is built turn by turn from the
//! genesis hash, so [`ConversationStore::verify_chain`] passes on imported
//! history. Corrupt records are skipped with a warning (the legacy store's
//! own semantics). The import is idempotent and non-destructive: records
//! whose id already exists are skipped, and after a successful pass the
//! legacy dir is renamed to `conversations.imported/` and kept as a backup,
//! so a second open finds nothing to import.
//!
//! # Writer identity (17.2)
//!
//! `writer_fingerprint` is, in preference order:
//!
//! 1. **The operator's mesh-key fingerprint** — when `<root>/identity.pem`
//!    exists and parses (the newt-identity `UserKey`; for the production
//!    root `~/.newt` this is exactly `~/.newt/identity.pem`), the
//!    fingerprint is [`agent_mesh_protocol::UserKey::fingerprint`] in full
//!    hex: BLAKE3 of the ed25519 public key, stable per operator across
//!    installs and machines. Dependency note: this comes straight from
//!    `agent-mesh-protocol` (already a direct dep); it must NOT come from
//!    `newt-identity`, which depends on newt-core — the inversion would be
//!    a cycle.
//! 2. **The 17.1a per-install nonce fallback** — BLAKE3 hex of a nonce
//!    minted once at `<root>/install-nonce`: stable across sessions,
//!    distinct across installs. Used when no identity exists yet, or when
//!    `identity.pem` is unreadable/corrupt (logged; a broken key file must
//!    never block the store).
//!
//! Rows written before an identity existed keep their recorded nonce-derived
//! writer and still verify: chains are per-writer (genesis is keyed by
//! `(conversation, writer)`), `verify_chain` follows each row's *recorded*
//! writer, and the Lamport clock seeds from the global max tick — so a
//! fingerprint upgrade mid-history reads as a writer handoff, which §6
//! already supports. Ticks are still not *signed*; that needs a schema
//! column and arrives with a later step.
//!
//! # FTS5 recall index (17.3)
//!
//! `turns_fts` is a trigger-maintained **external-content** FTS5 table
//! (unicode61 tokenizer) over four columns per turn: `user`, `assistant`,
//! `tool_names`, and `tool_args_digest`. The latter two are derived **at
//! index time** from the `events` JSON column — the 17.6 seam: `events` is
//! a JSON array, and every element carrying a `tool` / `args_digest` string
//! field contributes to the respective column (space-joined). As of 17.6
//! [`ConversationStore::append_turn_full`] records real tool events
//! ([`crate::ToolEvent`] — name, privacy-preserving args digest, outcome,
//! duration claim), so a recall search for a tool name or digest term hits;
//! rows written through plain `append_turn` (and every pre-17.6 row) carry
//! `'[]'` and contribute empty derived columns.
//!
//! External content means FTS5 stores only the inverted index; at query
//! time, column values (for [`ConversationStore::search`]'s `snippet()`)
//! are read back through the `turns_fts_content` view, which derives the
//! two event columns with the **same SQL expression** the triggers use
//! ([`events_extract_sql`]) — so the indexed terms and the content read
//! back can never disagree.
//!
//! Maintenance is by trigger: AFTER INSERT on `turns` (covers live appends
//! and the one-time legacy import alike) and AFTER DELETE on `turns`
//! (fires per row via the conversation-delete `ON DELETE CASCADE`). There
//! is deliberately **no UPDATE trigger**: turns are append-only — no code
//! path updates a turn row, and the §6 content chain depends on that
//! invariant. The external-content `'delete'` command relies on it too:
//! the values passed at delete time must equal the values indexed at
//! insert time, which append-only rows guarantee.
//!
//! **Schema-diff story:** opening an older database that predates the
//! index creates the view + virtual table + triggers AND backfills every
//! existing turn, all in one `BEGIN IMMEDIATE` transaction. Presence of
//! the `turns_fts` table is the idempotence marker — the backfill runs
//! exactly once per database.
//!
//! **Rowid caveat (honesty note):** the index is keyed by `turns`' implicit
//! rowid, and `turns` has a composite TEXT primary key — so SQLite's
//! `VACUUM` is allowed to renumber those rowids, which would silently
//! re-point index entries at the wrong turns. Nothing in newt ever VACUUMs
//! `conversations.db`; external tools must not either. Recovery if one
//! did: `DROP TABLE turns_fts;` and reopen — the open-time path recreates
//! the table and re-runs the backfill.
//!
//! Query strings never reach `MATCH` raw: [`sanitize_fts5_query`] (the
//! ported hermes sanitizer) preserves balanced `"phrases"`, strips FTS5
//! metacharacters, trims dangling `AND`/`OR`/`NOT`, and auto-quotes
//! dotted/hyphenated/path-like tokens (`chat-send`, `P2.2`,
//! `src/store.rs`) so they are matched as text instead of parsed as
//! syntax.
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

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

mod artifacts;

mod fts;
use fts::create_fts_index;
pub use fts::sanitize_fts5_query;

mod legacy_import;
use legacy_import::{import_legacy_json, migrate_workspace_key};

mod liveness;
pub(crate) use liveness::pid_is_alive;
#[cfg(all(test, target_os = "linux"))]
use liveness::pid_start_unix_nanos;
use liveness::{current_host_boot, live_owner_row, system_liveness};
#[cfg(test)]
use liveness::{
    open_process_failure_reports_live_or_unknown, pid_identity_matches,
    wait_probe_reports_live_or_unknown,
};
pub use liveness::{ClaimOutcome, LivenessFn, StoredOwner};

mod schema;
#[cfg(test)]
use schema::wal_fallback_eligible;
use schema::{apply_journal_mode, create_schema, reconcile_schema, BUSY_TIMEOUT};

mod turn_chain;
use turn_chain::{
    genesis_hash, insert_turn_row, last_turn, next_tick, turn_at_seq, turn_row_from_sql, TurnRow,
    TURN_ENCODING_VERSION_CURRENT,
};

use crate::conversation::{
    new_conversation_id, session_plan_dir, ConversationRecord, ConversationSummary,
    ConversationTurn,
};
use crate::prompt::{NewPrompt, PromptId, PromptOrigin, PromptReceipt, TurnPromptContext};

/// Database file name under the store root (`~/.newt/conversations.db`).
const DB_FILE: &str = "conversations.db";

/// Per-install nonce file under the store root; its BLAKE3 hex is the
/// `writer_fingerprint` *fallback* when no identity key exists (see
/// module docs — Writer identity).
const NONCE_FILE: &str = "install-nonce";

/// The operator's root identity key under the store root (`~/.newt` in
/// production — the same `~/.newt/identity.pem` newt-identity mints). When
/// present, its fingerprint IS the writer fingerprint.
const IDENTITY_PEM_FILE: &str = "identity.pem";

/// Maximum number of receipts examined while resolving the active operator
/// through a harness-retry parent chain. This includes both the submitted
/// receipt and the terminal operator receipt. A finite bound makes corrupted
/// or adversarial lineage fail closed without unbounded CPU/memory use; 256
/// still permits far more consecutive automatic retries than a useful turn
/// should ever require.
const MAX_PROMPT_LINEAGE_DEPTH: usize = 256;

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
    /// #1030: this process's hostname + kernel boot id, captured once at open —
    /// the machine-identity half of a `live_owners` claim (paired with `pid`).
    host: String,
    boot_id: String,
    /// This process's OS pid — the process-unique half of a claim. The writer
    /// fingerprint is shared per machine (derived from `identity.pem`), so it
    /// cannot identify a process on its own; `pid` + `host` + `boot_id` does.
    pid: i64,
    /// Liveness oracle used by `claim` / `is_owner_live` to decide whether a
    /// stored claim is still a running process. Injectable for tests (default
    /// [`system_liveness`]).
    liveness: LivenessFn,
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
        let workspace_id = crate::workspace_key::workspace_key_v2(&workspace)?;
        std::fs::create_dir_all(&root)?;
        let writer_fingerprint = resolve_writer_fingerprint(&root)?;

        let conn = Connection::open(root.join(DB_FILE))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // First-open init under concurrency: the journal-mode transition has
        // documented busy-handler-EXEMPT lock paths, so SQLITE_BUSY can escape
        // despite busy_timeout when several first runs race (reproduced: 8
        // concurrent opens under llvm-cov). Bounded retry; once the db is in
        // WAL, re-running this phase is a no-op so steady-state never loops.
        let wal_fallback = {
            let mut attempt = 0u32;
            loop {
                match apply_journal_mode(&conn)
                    .and_then(|fb| create_schema(&conn).map(|()| fb))
                    .and_then(|fb| reconcile_schema(&conn).map(|()| fb))
                    // #1086: rebuild a legacy id-only-PK roadmaps table to the
                    // composite (id, workspace_key) key. No-op once composite.
                    .and_then(|fb| migrate_roadmaps_pk(&conn).map(|()| fb))
                    // After reconciliation: the FTS view reads `events`,
                    // which on a drifted pre-17.1b db exists only once the
                    // column reconciliation above has run.
                    .and_then(|fb| create_fts_index(&conn).map(|()| fb))
                {
                    Ok(fb) => break fb,
                    Err(e)
                        if attempt < 20
                            && e.to_string().to_ascii_lowercase().contains("locked") =>
                    {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(
                            25 * u64::from(attempt.min(4)),
                        ));
                    }
                    Err(e) => return Err(e),
                }
            }
        };

        import_legacy_json(&conn, &root, &writer_fingerprint)?;
        // 17.2: after the import (whose records carry UUIDv5 keys), re-key
        // THIS workspace's rows from the retired UUIDv5 derivation to v2.
        migrate_workspace_key(&conn, &workspace, &workspace_id)?;

        let (host, boot_id) = current_host_boot();
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            workspace,
            workspace_id,
            writer_fingerprint,
            max_per_workspace,
            wal_fallback,
            claim_clock: now_claim_nanos,
            host,
            boot_id,
            pid: i64::from(std::process::id()),
            liveness: system_liveness,
        })
    }

    /// The RETIRED v1 workspace key: UUIDv5 of the canonical path — the
    /// derivation the JSON backend used (its per-workspace dir names) and
    /// 17.1a inherited for `workspace_key`. Kept for exactly two lookups:
    /// the one-time legacy JSON import (dir names are UUIDv5) and the 17.2
    /// open-time migration that re-keys this workspace's old rows to
    /// [`crate::workspace_key::workspace_key_v2`]. Do not key anything new
    /// with it.
    #[deprecated(
        since = "0.6.8",
        note = "v1 keying is path-fragile; use `newt_core::workspace_key_v2` \
                (17.2). This stays only for the UUIDv5→v2 row migration and \
                legacy-import dir names."
    )]
    pub fn workspace_id_for_path(path: impl AsRef<Path>) -> anyhow::Result<String> {
        let canonical = std::fs::canonicalize(path.as_ref())?;
        let normalized = canonical.to_string_lossy().replace('\\', "/");
        Ok(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, normalized.as_bytes()).to_string())
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

    /// Atomically accept one prompt before any model or tool work begins.
    ///
    /// The transaction lazy-creates `conversation_id` when this is its first
    /// prompt, allocates the prompt's Lamport sequence, resolves explicit
    /// ancestry, writes the immutable receipt, and advances conversation
    /// activity. It deliberately does **not** touch the conversation's turn
    /// `writer_fingerprint` / `tip_hash` pair: prompts have their own hashes
    /// and must not masquerade as completed turns in the existing chain.
    ///
    /// `previous_prompt_id` is assigned from serialized receipt chronology.
    /// That is independent from semantic parentage: a plain operator prompt
    /// has no parent and roots at itself. Explicit continuations/retries must
    /// name a same-conversation parent and inherit its validated root.
    pub fn begin_prompt(
        &self,
        conversation_id: &str,
        title: &str,
        persona: Option<&str>,
        prompt: NewPrompt,
    ) -> anyhow::Result<TurnPromptContext> {
        validate_record_id(conversation_id)?;
        std::str::from_utf8(prompt.model_text()).map_err(|error| {
            anyhow::anyhow!(
                "prompt model text is not valid UTF-8 and cannot be sent to inference: {error}"
            )
        })?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;

        // The global conversation id is itself an authority boundary. Never
        // let a caller in workspace B attach a prompt to workspace A's row.
        let owner: Option<String> = tx
            .query_row(
                "SELECT workspace_key FROM conversations WHERE id = ?1",
                [conversation_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(owner) = owner.as_deref() {
            if owner != self.workspace_id {
                anyhow::bail!(
                    "conversation id `{conversation_id}` belongs to another workspace \
                     (key {owner}); refusing to attach a prompt across the workspace fence"
                );
            }
        }

        let tick = next_tick(&tx, &self.writer_fingerprint)?;
        let created = owner.is_none();
        if created {
            // Unlike `create_with_id`, this is an INSERT, never REPLACE. A
            // prompt receipt and the conversation that owns it become visible
            // together at commit; no crash window can leave only one behind.
            tx.execute(
                "INSERT INTO conversations
                   (id, title, workspace_path, workspace_key, persona, end_reason,
                    writer_fingerprint, activity_tick, tip_hash,
                    started_at_claim, updated_at_claim)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?9)",
                rusqlite::params![
                    conversation_id,
                    title.trim(),
                    self.workspace.to_string_lossy(),
                    self.workspace_id,
                    persona,
                    self.writer_fingerprint,
                    tick,
                    genesis_hash(conversation_id, &self.writer_fingerprint),
                    now,
                ],
            )?;
        } else {
            // A submitted prompt is real activity even if inference later
            // fails. Do not move the turn writer/tip pair here.
            tx.execute(
                "UPDATE conversations
                    SET activity_tick = ?2, updated_at_claim = ?3
                  WHERE id = ?1 AND workspace_key = ?4",
                rusqlite::params![conversation_id, tick, now, self.workspace_id],
            )?;
        }

        let previous_prompt_id = latest_prompt_on_conn(&tx, conversation_id, &self.workspace_id)?
            .map(|receipt| receipt.id());
        let prompt_id = PromptId::new();

        let parent = match prompt.parent_prompt_id {
            Some(parent_id) => Some(
                load_prompt_in_conversation_on_conn(
                    &tx,
                    conversation_id,
                    parent_id,
                    &self.workspace_id,
                )?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "prompt parent {parent_id} is not in conversation \
                             `{conversation_id}` in this workspace"
                    )
                })?,
            ),
            None => None,
        };
        if prompt.origin == PromptOrigin::HarnessRetry && parent.is_none() {
            anyhow::bail!("a harness retry must name an operator-prompt parent");
        }

        let root_prompt_id = match parent.as_ref() {
            Some(parent) => {
                validate_objective_root_on_conn(
                    &tx,
                    conversation_id,
                    parent.root_prompt_id(),
                    &self.workspace_id,
                )?;
                parent.root_prompt_id()
            }
            None => prompt_id,
        };
        let active_operator = match (prompt.origin, parent.as_ref()) {
            // A fresh operator submission is its own active authority.
            (PromptOrigin::Operator, None) => None,
            // bug/steering-regressions: an operator CONTINUATION (a decision
            // or clarification reply bound to a parent ask) REFINES the parent
            // objective — it must not usurp it as the active operator prompt.
            // Otherwise the protected active-prompt card carries the ceremony
            // ("1: proceed") for the whole agentic turn and mid-turn
            // compaction evicts the real task (live gpt-4.1 + Qwen3-Coder
            // drives, 2026-07-26/27).
            (PromptOrigin::Operator, Some(parent)) | (PromptOrigin::HarnessRetry, Some(parent)) => {
                let (active, parent_depth) = resolve_active_operator_on_conn(
                    &tx,
                    conversation_id,
                    parent,
                    &self.workspace_id,
                )?;
                if parent_depth >= MAX_PROMPT_LINEAGE_DEPTH {
                    anyhow::bail!(
                        "prompt lineage would exceed the maximum prompt lineage depth of \
                         {MAX_PROMPT_LINEAGE_DEPTH} receipts"
                    );
                }
                Some(active)
            }
            (PromptOrigin::HarnessRetry, None) => {
                anyhow::bail!("a harness retry must name an operator-prompt parent")
            }
        };
        let active_operator_id = active_operator
            .as_ref()
            .map_or(prompt_id, PromptReceipt::id);

        let receipt = PromptReceipt::new(
            prompt_id,
            conversation_id.to_string(),
            self.writer_fingerprint.clone(),
            tick,
            previous_prompt_id,
            parent.as_ref().map(PromptReceipt::id),
            root_prompt_id,
            active_operator_id,
            prompt.origin,
            prompt.raw_text,
            prompt.model_text,
            now,
        );
        insert_prompt_receipt(&tx, &receipt)?;
        let context = TurnPromptContext::new(
            receipt.clone(),
            active_operator.unwrap_or_else(|| receipt.clone()),
        );
        tx.commit()?;
        drop(conn);

        if created {
            // The receipt is already committed. Retention is housekeeping,
            // not part of prompt acceptance: reporting an error here would
            // tell the caller the prompt was not recorded even though it is
            // durable, inviting a duplicate retry. Keep the accepted receipt
            // authoritative and surface pruning failure as a diagnostic.
            if let Err(error) = self.prune_to_cap_excluding(conversation_id) {
                tracing::warn!(
                    %error,
                    conversation_id,
                    "prompt committed but conversation retention pruning failed"
                );
            }
        }
        Ok(context)
    }

    /// Read one prompt by its stable address, fenced to this store's
    /// workspace. An address owned by another workspace is indistinguishable
    /// from absence.
    pub fn load_prompt(&self, prompt_id: PromptId) -> anyhow::Result<Option<PromptReceipt>> {
        let conn = self.lock_conn();
        load_prompt_on_conn(&conn, prompt_id, &self.workspace_id)
    }

    /// Read one prompt only when both its address and owning conversation
    /// match. This is the narrow resolver used by an always-on prompt tool: a
    /// model cannot use a valid same-workspace handle to escape its active
    /// conversation.
    pub fn load_prompt_in_conversation(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
    ) -> anyhow::Result<Option<PromptReceipt>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        load_prompt_in_conversation_on_conn(&conn, conversation_id, prompt_id, &self.workspace_id)
    }

    /// The most recently received prompt in a conversation, or `None` for an
    /// unknown/empty conversation. Receipt order is SQLite-serialized append
    /// order, not a wall-clock claim.
    pub fn latest_prompt(&self, conversation_id: &str) -> anyhow::Result<Option<PromptReceipt>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        latest_prompt_on_conn(&conn, conversation_id, &self.workspace_id)
    }

    /// Follow the automatic chronological predecessor link for one prompt.
    pub fn previous_prompt(&self, prompt_id: PromptId) -> anyhow::Result<Option<PromptReceipt>> {
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let Some(receipt) = load_prompt_on_conn(&tx, prompt_id, &self.workspace_id)? else {
            tx.commit()?;
            return Ok(None);
        };
        let Some(previous) = receipt.previous_prompt_id() else {
            tx.commit()?;
            return Ok(None);
        };
        let previous = load_prompt_in_conversation_on_conn(
            &tx,
            receipt.conversation_id(),
            previous,
            &self.workspace_id,
        )?;
        tx.commit()?;
        Ok(previous)
    }

    /// All prompt receipts in durable receipt order for one conversation.
    pub fn prompt_chain(&self, conversation_id: &str) -> anyhow::Result<Vec<PromptReceipt>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        prompt_chain_on_conn(&conn, conversation_id, &self.workspace_id)
    }

    /// Rebuild the submitted-vs-active authority context for a prompt. An
    /// operator receipt is active itself; a harness retry resolves to the
    /// nearest validated operator authority inherited from its parent. The
    /// objective root remains a separate lineage pointer.
    pub fn turn_prompt_context(
        &self,
        conversation_id: &str,
        submitted_prompt_id: PromptId,
    ) -> anyhow::Result<Option<TurnPromptContext>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let Some(submitted) = load_prompt_in_conversation_on_conn(
            &tx,
            conversation_id,
            submitted_prompt_id,
            &self.workspace_id,
        )?
        else {
            tx.commit()?;
            return Ok(None);
        };
        let (active, _) =
            resolve_active_operator_on_conn(&tx, conversation_id, &submitted, &self.workspace_id)?;
        tx.commit()?;
        Ok(Some(TurnPromptContext::new(submitted, active)))
    }

    /// Create a conversation with a freshly minted id; returns the id.
    pub fn create(&self, title: &str, persona: Option<&str>) -> anyhow::Result<String> {
        let id = new_conversation_id();
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
                let has_prompt_receipts: bool = tx.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM prompt_receipts WHERE conversation_id = ?1
                     )",
                    [id],
                    |row| row.get(0),
                )?;
                if has_prompt_receipts {
                    anyhow::bail!(
                        "conversation `{id}` has immutable prompt receipts; refusing to \
                         recreate it implicitly (delete it explicitly before reusing the id)"
                    );
                }
            }
            let tick = next_tick(&tx, &self.writer_fingerprint)?;
            // INSERT OR REPLACE mirrors the JSON backend for legacy rows where
            // re-creating an existing id overwrote the record (turns reset).
            // Prompt-bearing rows are rejected above: an ordinary "create"
            // API must never cascade away an already-accepted prompt receipt.
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
        // Creation is committed above; retention failure must not turn that
        // success into a false negative for the caller.
        if let Err(error) = self.prune_to_cap_excluding(id) {
            tracing::warn!(
                %error,
                conversation_id = id,
                "conversation created but retention pruning failed"
            );
        }
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

    /// Append one `(user, assistant)` turn with no tool events and no token
    /// usage. `id` may be a unique prefix. Thin wrapper over
    /// [`append_turn_full`](Self::append_turn_full): an empty event slice
    /// serializes to `'[]'` and absent tokens to NULL — byte-identical to
    /// the pre-17.6 row shape, so existing callers are unchanged.
    pub fn append_turn(&self, id: &str, user: &str, assistant: &str) -> anyhow::Result<()> {
        self.append_turn_full(id, user, assistant, &[], &[], &[], None, None)
    }

    /// Append one turn with its recorded tool events and backend-reported
    /// token usage (Step 17.6, issue #246). `id` may be a unique prefix.
    ///
    /// One `BEGIN IMMEDIATE` transaction covers: tick allocation, chain
    /// extension (`prev_hash` from the current per-writer tip), the row
    /// insert, and the conversation's activity/tip update. Appending never
    /// prunes — only `create` does, matching the JSON backend.
    ///
    /// **Chain (§6):** events and token counts are row content — the v1
    /// canonical encoding has length-prefixed the serialized `events`
    /// string and the token presence bytes since 17.1a, so populated
    /// values hash under the exact rules empty ones did. No
    /// `encoding_version` bump: pre-17.6 rows (`'[]'`, NULL) and 17.6 rows
    /// verify under the same v1 dispatch, and tampering with a stored
    /// event breaks [`verify_chain`](Self::verify_chain) like any other
    /// field.
    ///
    /// **Tokens are measurements, not estimates:** pass the backend's
    /// reported counts or `None`. `None` is stored as NULL — absence stays
    /// observable (18.5 rehydrates from these columns and must be able to
    /// trust them; gates-are-honest).
    ///
    /// **FTS:** the 17.3 AFTER INSERT trigger derives `tool_names` /
    /// `tool_args_digest` from the events JSON at index time — recording
    /// events here lights recall up with no schema work.
    ///
    /// **Phantom reaches (#717 → #1786):** the per-turn alias-seam record
    /// persists alongside `events` in its own `phantom_reaches` column. As of
    /// the v2 encoding it is INSIDE the canonical hash: `Rewrite` records
    /// that newt substituted a different tool for the one the model named —
    /// the derivation edge between emitted and executed — and `Unknown` is
    /// the fabrication ledger. Both are provenance someone will rely on, not
    /// telemetry. Pre-bump v1 rows keep their reaches outside the hash
    /// forever (their arm cannot change); see the spec's §3.2 residue.
    ///
    /// **Sources (#1786):** content ids of the turns a DERIVED row (a
    /// compaction summary) was derived from; empty for witnessed turns.
    /// Validated (64 lowercase hex each) and canonicalized (sorted, deduped,
    /// compact JSON) here, so this write path cannot produce the
    /// non-canonical bytes verification refuses.
    #[allow(clippy::too_many_arguments)]
    pub fn append_turn_full(
        &self,
        id: &str,
        user: &str,
        assistant: &str,
        events: &[crate::ToolEvent],
        phantom_reaches: &[crate::PhantomReach],
        sources: &[String],
        tokens_in: Option<u32>,
        tokens_out: Option<u32>,
    ) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let now = (self.claim_clock)();
        let events_json = serde_json::to_string(events)?;
        let phantom_reaches_json = serde_json::to_string(phantom_reaches)?;
        // #1786 §3 — the derived-row shape invariant, enforced HERE and not
        // only at verification. `verify_chain` refuses a row carrying both
        // derivation and tool activity; without the same refusal at the
        // append, a caller could commit one and brick the conversation
        // permanently (the read path repairs nothing by design). A write
        // path must never admit what verification rejects.
        if !sources.is_empty() && !(events.is_empty() && phantom_reaches.is_empty()) {
            anyhow::bail!(
                "refusing the append -- this turn claims derivation (non-empty \
                 sources) AND tool activity; a derived row is harness-minted and \
                 carries neither events nor phantom reaches, and a row with both \
                 could never be verified again"
            );
        }
        let sources_json = canonical_sources_json(sources)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let tick = next_tick(&tx, &self.writer_fingerprint)?;

        // §6 tip witness (#1785). `tip_hash` is a SECOND, independently written
        // record of where this conversation's chain ends. Nothing chains on it —
        // `prev_hash` is always re-derived from the row itself — and that is
        // precisely what makes it a witness rather than a cache: two values
        // written at different moments that must agree.
        //
        // This deliberately O(1) witness check compares only the recorded
        // writer's final row with the stored tip. It catches an altered or
        // deleted tip at the moment we would otherwise extend it. An edit to
        // an earlier turn that leaves the final row and witness intact passes
        // here and is discovered by load_verified / verify_chain on restore.
        //
        // Writer-agnostic, matching `verify_chain`: the stored tip belongs to
        // the conversation row's RECORDED writer, not whoever is appending, so
        // a second writer joining a conversation does not spuriously fail.
        //
        // An EMPTY tip is absence of evidence, NOT evidence of tampering. A
        // database predating the column gains it as `''` from the schema-diff
        // backfill, and the first post-migration append is what repairs it —
        // refusing here would lock writes out of exactly the oldest histories,
        // and would state a conclusion nothing recorded supports.
        // `.optional()` for the same resolve-then-read gap as
        // `verify_conversation_chain`: a conversation deleted between
        // `resolve_id` and this transaction is a deletion, not corruption.
        let (recorded_tip, tip_writer): (String, String) = tx
            .query_row(
                "SELECT tip_hash, writer_fingerprint FROM conversations WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("conversation `{id}` not found"))?;
        // PHYSICAL ORDER vs SPEC ORDER. Spec §5 lists the write path as
        // (1) the appending writer's own witness, (2) the conversations-row
        // tip, (3) relocation, (4) insert + upserts. The code below runs
        // 2 -> 3 -> 1 -> 4, because relocation needs `tip_writer_final`,
        // which the tip check already computed.
        //
        // The orders are equivalent where it matters: all four steps share
        // ONE `Immediate` transaction, and every check bails with `?`, so a
        // failure at any step rolls the whole append back and nothing partial
        // is ever observable. The accept/reject set is therefore identical —
        // both orders refuse if EITHER witness fails, because both run before
        // the commit. The only difference is which diagnostic surfaces first
        // when more than one witness is simultaneously bad, and each message
        // names the witness it is about.
        if !recorded_tip.is_empty() {
            // Shared policy + diagnostics: `check_tip_witness` owns both, so
            // the write path and the read path cannot drift apart. The added
            // context names what is being refused and why: an append onto a
            // witness mismatch would chain new work on top of the damage.
            // (`verify_chain` gives the per-turn diagnosis when a specific
            // turn's link is broken; a witness-only mismatch has no "first
            // bad turn" to locate — the message says what it can prove.)
            // Flattened (not `.context()`) for the same Display-visibility
            // reason as `load_verified`: callers render these with `{e}`.
            let tip_writer_final = last_turn(&tx, &id, &tip_writer)?;
            Self::check_tip_witness(&id, &recorded_tip, &tip_writer, tip_writer_final.as_ref())
                .map_err(|e| {
                    // "could not be confirmed", not "disagrees": the inner error
                    // may also be a cannot-compute (an unknown encoding_version
                    // refuses hashing on an intact record) — the wrapper must
                    // stay accurate for both.
                    anyhow::anyhow!(
                        "refusing the append -- the recorded chain tip could not be \
                         confirmed, so new work must not extend that tip: {e:#}"
                    )
                })?;

            // #1786 §5 step 3 — witness RELOCATION on handoff: when a
            // different writer is about to take the conversations-row tip
            // and the outgoing writer has no per-writer witness yet, the
            // just-verified witness is copied down BEFORE the update below
            // overwrites it. Without this, the first post-migration handoff
            // append destroys the only witness pinning the outgoing writer's
            // final turn — one statement after proving it correct. This is
            // relocation of CHECKED evidence, not backfill-from-rows (a
            // witness computed from the rows it witnesses agrees by
            // construction — the vacuous-green pattern, which stays banned).
            if tip_writer != self.writer_fingerprint {
                if let Some(final_row) = tip_writer_final.as_ref() {
                    // The outgoing writer's existing per-writer witness, if
                    // any, is CHECKED before anything overwrites it. A blind
                    // `DO NOTHING` here left a stale witness in place while
                    // the conversations-row tip moved to the incoming writer
                    // — and because the outgoing writer never appends again,
                    // "repaired by the next append" never arrives, so its
                    // final turn ended up pinned by nothing (#1794 residual
                    // 2, reopened through the rollback path).
                    let existing: Option<(String, i64)> = tx
                        .query_row(
                            "SELECT tip_hash, tip_seq FROM writer_tips
                              WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
                            rusqlite::params![id, tip_writer],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    match existing {
                        // No per-writer row (the migration shape): relocate
                        // the just-verified conversations-row witness down.
                        None => {
                            tx.execute(
                                "INSERT INTO writer_tips
                                   (conversation_id, writer_fingerprint, tip_hash, tip_seq)
                                 VALUES (?1, ?2, ?3, ?4)",
                                rusqlite::params![id, tip_writer, recorded_tip, final_row.seq],
                            )?;
                        }
                        // A witness already exists. Verify it on its own
                        // terms FIRST — a bad one must fail the append and be
                        // left exactly as found, never overwritten by the
                        // advance below (that would launder the evidence).
                        Some((existing_hash, existing_seq)) => {
                            let fetched;
                            let row_at: Option<&TurnRow> = if final_row.seq == existing_seq {
                                Some(final_row)
                            } else {
                                fetched = turn_at_seq(&tx, &id, &tip_writer, existing_seq)?;
                                fetched.as_ref()
                            };
                            Self::check_writer_tip_witness(
                                &id,
                                &tip_writer,
                                &existing_hash,
                                existing_seq,
                                final_row.seq,
                                row_at,
                            )
                            .map_err(|e| {
                                anyhow::anyhow!(
                                    "refusing the append -- the outgoing writer's own \
                                     recorded witness could not be confirmed, so the \
                                     handoff must not overwrite it: {e:#}"
                                )
                            })?;
                            // Valid but STALE: advance it to the tip this
                            // very append already verified against the
                            // outgoing writer's final row. This relocates
                            // CHECKED evidence — `recorded_tip` comes from
                            // the conversations row and was confirmed by
                            // `check_tip_witness` above — rather than
                            // recomputing a hash from the rows the witness
                            // exists to protect (vacuous backfill, banned).
                            if existing_seq < final_row.seq {
                                tx.execute(
                                    "UPDATE writer_tips SET tip_hash = ?3, tip_seq = ?4
                                      WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
                                    rusqlite::params![id, tip_writer, recorded_tip, final_row.seq],
                                )?;
                            }
                        }
                    }
                }
            }
        }

        // #1786 §5 step 1 — the appending writer's OWN witness, seq-aware,
        // checked BEFORE its last row is trusted as the chain tip. Without
        // this, a tamper of a non-tip writer's final turn is laundered — and
        // its only evidence overwritten by the upsert below — the moment that
        // writer appends again. Absence is skip (the writer predates the
        // table); a LOWER tip_seq is a stale-but-honest witness (a rolled-back
        // binary appended without maintaining writer_tips) verified against
        // the row it actually pins and repaired by the upsert below.
        let own_last = last_turn(&tx, &id, &self.writer_fingerprint)?;
        let own_witness: Option<(String, i64)> = tx
            .query_row(
                "SELECT tip_hash, tip_seq FROM writer_tips
                  WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
                rusqlite::params![id, self.writer_fingerprint],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((tip_hash, tip_seq)) = own_witness {
            let final_seq = own_last.as_ref().map(|r| r.seq).unwrap_or(0);
            let fetched;
            let row_at_seq: Option<&TurnRow> =
                if own_last.as_ref().is_some_and(|r| r.seq == tip_seq) {
                    own_last.as_ref()
                } else {
                    fetched = turn_at_seq(&tx, &id, &self.writer_fingerprint, tip_seq)?;
                    fetched.as_ref()
                };
            Self::check_writer_tip_witness(
                &id,
                &self.writer_fingerprint,
                &tip_hash,
                tip_seq,
                final_seq,
                row_at_seq,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "refusing the append -- this writer's own recorded witness could \
                     not be confirmed, so new work must not chain past it: {e:#}"
                )
            })?;
        }

        // The §6 content chain: hash the canonical encoding of this writer's
        // previous turn (re-derived from the row itself, so a drifted
        // `tip_hash` column can never poison the chain).
        let prev_hash = match own_last {
            Some(ref prev) => prev.content_hash()?,
            None => genesis_hash(&id, &self.writer_fingerprint),
        };

        let row = TurnRow {
            conversation_id: id.clone(),
            writer_fingerprint: self.writer_fingerprint.clone(),
            seq: tick,
            prev_hash,
            user: user.to_string(),
            assistant: assistant.to_string(),
            events: events_json,
            phantom_reaches: phantom_reaches_json,
            sources: sources_json,
            tokens_in: tokens_in.map(i64::from),
            tokens_out: tokens_out.map(i64::from),
            ts_claim: now,
            encoding_version: TURN_ENCODING_VERSION_CURRENT,
        };
        insert_turn_row(&tx, &row)?;
        // Activity tick + chain tip + per-writer witness move together;
        // updated_at_claim is a display claim only (§6) — nothing orders by
        // it. The two witnesses are written in ONE transaction, which is why
        // read-path divergence between them (at the same seq) has no
        // legitimate producer.
        let row_hash = row.content_hash()?;
        tx.execute(
            "INSERT INTO writer_tips (conversation_id, writer_fingerprint, tip_hash, tip_seq)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (conversation_id, writer_fingerprint)
             DO UPDATE SET tip_hash = excluded.tip_hash, tip_seq = excluded.tip_seq",
            rusqlite::params![id, self.writer_fingerprint, row_hash, tick],
        )?;
        tx.execute(
            "UPDATE conversations
                SET writer_fingerprint = ?2, activity_tick = ?3, tip_hash = ?4,
                    updated_at_claim = ?5
              WHERE id = ?1",
            rusqlite::params![id, self.writer_fingerprint, tick, row_hash, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Load a full record (turns in causal `(writer, seq)` order). `id` may
    /// be a unique prefix.
    ///
    /// This is the UNVERIFIED read: it materializes whatever the rows say,
    /// including a tampered chain — which is deliberate, because it is also
    /// the examine-the-evidence path a refused restore points at. The path
    /// that hands history to the model is [`Self::load_verified`].
    pub fn load(&self, id: &str) -> anyhow::Result<ConversationRecord> {
        let id = self.resolve_id(id)?;
        let conn = self.lock_conn();
        Self::load_record_on(&conn, &self.workspace_id, &id)
    }

    /// Verify and materialize `id` from ONE SQLite read snapshot — the only
    /// load the restore/resume path may use.
    ///
    /// The invariant this method exists to hold: **the record returned is
    /// exactly the snapshot whose integrity was verified.** `verify_chain`
    /// followed by `load` as two calls cannot hold it — each call reads its
    /// own database state, so a legitimate concurrent append between them
    /// reads as corruption, and a corruption landing between them reads as
    /// clean. Here the chain walk, the tip-witness comparison, and the
    /// materialization of the returned [`ConversationRecord`] all run inside
    /// a single read transaction (WAL snapshot isolation), so both hazards
    /// are structurally absent rather than merely unlikely.
    ///
    /// Fail-closed: a violation refuses with the integrity diagnosis and
    /// changes nothing — no repair, no re-chain, no witness rebuild. The rows
    /// stay exactly as found, readable through [`Self::load`] for
    /// examination.
    ///
    /// ## Coverage boundary — what "verified" does and does not cover
    ///
    /// The §6 chain covers the canonical turn encoding, except for
    /// `phantom_reaches`; it does not authenticate the surrounding
    /// `conversations` row. The following data inside or beside the returned
    /// record is outside it, stated here so "verified" is never read as more
    /// than it is:
    ///
    /// * **`phantom_reaches`.** This per-turn telemetry is deliberately not a
    ///   canonical-encoding input, so an SQL-level edit passes this gate.
    /// * **Conversation-row metadata.** `title`, workspace metadata,
    ///   `persona`, roadmap/node IDs, and the created/updated time claims are
    ///   materialized from `conversations`, but are not chain inputs. An
    ///   SQL-level edit passes this gate; restore applies the returned persona.
    /// * **`scratchpad` and `plan`** ride the conversations row unhashed
    ///   ("working memory, not provenance" — the schema comments). They are
    ///   rehydrated into the restored session, so an SQL-level edit to them
    ///   passes this gate. Same family as the `phantom_reaches` gap; the
    ///   coverage decision belongs to #1786's encoding bump.
    /// * **Non-tip writers' final turns.** Each writer's chain pins every
    ///   turn except its last (nothing chains onto a final turn), and the
    ///   single recorded witness pins only the RECORDED tip writer's last
    ///   turn. In a multi-writer history (a writer handoff, a fingerprint
    ///   upgrade), the other writers' final turns are pinned by nothing —
    ///   binding them needs a per-writer witness, which is schema work, not
    ///   a read-path fix.
    /// * **The witness columns themselves** (see the erasure bound
    ///   documented on `check_tip_witness`).
    ///
    /// Operational note: under the documented journal_mode=DELETE fallback
    /// (NFS homes where WAL is refused), this read transaction holds SHARED
    /// for the whole verify+materialize, so a very large conversation can
    /// hold off a concurrent writer's COMMIT past its busy_timeout. That is
    /// the price of single-snapshot verification without WAL; accepted, and
    /// bounded by conversation size.
    pub fn load_verified(&self, id: &str) -> anyhow::Result<ConversationRecord> {
        let id = self.resolve_id(id)?;
        let conn = self.lock_conn();
        // A DEFERRED read transaction: under WAL this pins one snapshot at
        // the first read, which is what makes verify-and-materialize atomic
        // against writers on OTHER connections (same-store writers are
        // already serialized by the connection mutex).
        let tx = conn.unchecked_transaction()?;
        // Flattened (not `.context()`) ON PURPOSE: the TUI surfaces restore
        // errors with Display (`{e}`, tab_switch preflight), which shows only
        // the outermost anyhow layer — a context wrapper would bury the
        // diagnosis it wraps. One flat message keeps the whole diagnosis
        // Display-visible; the conformance tests pin this.
        Self::verify_conversation_chain(&tx, &id)
            .map_err(|e| anyhow::anyhow!("refusing the restore — nothing has moved: {e:#}"))?;
        let record = Self::load_record_on(&tx, &self.workspace_id, &id)?;
        tx.commit()?;
        Ok(record)
    }

    /// Materialize a [`ConversationRecord`] on the caller's connection — the
    /// shared body of [`ConversationStore::load`] and
    /// [`ConversationStore::load_verified`]. Taking `&Connection` is what lets
    /// `load_verified` run this inside the SAME read transaction as the chain
    /// verification: one snapshot, verified and returned.
    fn load_record_on(
        conn: &Connection,
        workspace_id: &str,
        id: &str,
    ) -> anyhow::Result<ConversationRecord> {
        let (mut record, scratchpad_json, plan_json) = conn
            .query_row(
                "SELECT id, title, workspace_path, workspace_key, persona,
                    started_at_claim, updated_at_claim, scratchpad, plan,
                    roadmap_id, node_id
               FROM conversations
              WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id, workspace_id],
                |row| {
                    Ok((
                        ConversationRecord {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            workspace: row.get(2)?,
                            workspace_id: row.get(3)?,
                            persona: row.get(4)?,
                            turns: Vec::new(),
                            scratchpad: std::collections::BTreeMap::new(),
                            plan: crate::PlanSnapshot::default(),
                            roadmap_id: row.get(9)?,
                            node_id: row.get(10)?,
                            created_at_unix_nanos: claim_to_u128(row.get(5)?),
                            updated_at_unix_nanos: claim_to_u128(row.get(6)?),
                        },
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("conversation `{id}` not found"))?;
        // #713: the scratchpad <state> snapshot. Strict decode — never hand back
        // garbage (same discipline as the turn `events`/`phantom_reaches`
        // columns). A pre-#713 row carries the `{}` backfill and parses empty.
        record.scratchpad = serde_json::from_str(&scratchpad_json).map_err(|e| {
            anyhow::anyhow!(
                "conversation `{id}`: scratchpad column is not valid <state> JSON \
                 ({e}); refusing to load garbage"
            )
        })?;
        // #715: the plan-ledger snapshot. Same strict decode discipline. A
        // pre-#715 row carries the `{}` backfill and parses to an empty plan.
        record.plan = serde_json::from_str(&plan_json).map_err(|e| {
            anyhow::anyhow!(
                "conversation `{id}`: plan column is not valid <plan> snapshot JSON \
                 ({e}); refusing to load garbage"
            )
        })?;

        // §6: turn order is the causal tick, never ts_claim.
        let mut stmt = conn.prepare(
            "SELECT user, assistant, events, tokens_in, tokens_out, phantom_reaches FROM turns
              WHERE conversation_id = ?1
              ORDER BY seq ASC, writer_fingerprint ASC",
        )?;
        let turns = stmt.query_map([&id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        for turn in turns {
            let (user, assistant, events_json, tokens_in, tokens_out, phantom_reaches_json) = turn?;
            // 17.6: events deserialize strictly — a row whose blob is not
            // ToolEvent-shaped errors clearly (the encoding_version
            // philosophy: never quietly hand back garbage). Pre-17.6 rows
            // carry '[]' and parse to an empty vec; unknown extra keys on
            // future events are ignored (additive growth needs no bump).
            let events: Vec<crate::ToolEvent> =
                serde_json::from_str(&events_json).map_err(|e| {
                    anyhow::anyhow!(
                        "conversation `{id}`: turn events column is not valid tool-event \
                         JSON ({e}); refusing to load garbage"
                    )
                })?;
            // #717: same strict decode as events — never hand back garbage.
            let phantom_reaches: Vec<crate::PhantomReach> =
                serde_json::from_str(&phantom_reaches_json).map_err(|e| {
                    anyhow::anyhow!(
                        "conversation `{id}`: turn phantom_reaches column is not valid \
                         phantom-reach JSON ({e}); refusing to load garbage"
                    )
                })?;
            record.turns.push(ConversationTurn {
                user,
                assistant,
                events,
                phantom_reaches,
                tokens_in: tokens_from_sql(tokens_in)?,
                tokens_out: tokens_from_sql(tokens_out)?,
            });
        }
        Ok(record)
    }

    /// Read ONE past turn by its `(conversation, seq)` address — the by-id
    /// read the `memory_fetch` tool's `turn:<conv>#<seq>` resolver needs
    /// (progressive-disclosure memory, Workstream A MVP, #319). `id` may be a
    /// unique prefix (same `resolve_id` discipline as [`Self::load`]); `seq`
    /// is the §6 per-writer tick the model was shown by a `recall` hit
    /// (`SearchHit::seq`).
    ///
    /// Workspace-fenced: the `conversations` join carries `workspace_key`, so
    /// a `seq` from another workspace's conversation resolves to `None`, never
    /// a cross-workspace leak (§7 fencing). Returns `Ok(None)` when no turn at
    /// that `(conversation, seq)` exists — labelled absence, never an error —
    /// so the tool executor can answer "no such memory item" rather than
    /// aborting the loop.
    pub fn load_turn(&self, id: &str, seq: i64) -> anyhow::Result<Option<ConversationTurn>> {
        // An unknown conversation id is absence, not an error — the tool
        // result must be friendly text, never a loop-aborting backend failure.
        let id = match self.resolve_id(id) {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT t.user, t.assistant, t.events, t.tokens_in, t.tokens_out, t.phantom_reaches
                   FROM turns t
                   JOIN conversations c
                     ON c.id = t.conversation_id AND c.workspace_key = ?3
                  WHERE t.conversation_id = ?1 AND t.seq = ?2",
                rusqlite::params![id, seq, self.workspace_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((user, assistant, events_json, tokens_in, tokens_out, phantom_reaches_json)) = row
        else {
            return Ok(None);
        };
        // Same strict events decode as `load`: never hand back garbage.
        let events: Vec<crate::ToolEvent> = serde_json::from_str(&events_json).map_err(|e| {
            anyhow::anyhow!(
                "conversation `{id}`: turn events column is not valid tool-event \
                 JSON ({e}); refusing to load garbage"
            )
        })?;
        // #717: same strict decode for the phantom-reach telemetry column.
        let phantom_reaches: Vec<crate::PhantomReach> = serde_json::from_str(&phantom_reaches_json)
            .map_err(|e| {
                anyhow::anyhow!(
                    "conversation `{id}`: turn phantom_reaches column is not valid \
                     phantom-reach JSON ({e}); refusing to load garbage"
                )
            })?;
        Ok(Some(ConversationTurn {
            user,
            assistant,
            events,
            phantom_reaches,
            tokens_in: tokens_from_sql(tokens_in)?,
            tokens_out: tokens_from_sql(tokens_out)?,
        }))
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

    /// Every conversation in the store, ACROSS all workspaces, most-recently-
    /// active first — each paired with the absolute `workspace_path` it belongs
    /// to. Unlike [`list`](Self::list) (workspace-fenced), this is the "all my
    /// sessions" view an attach surface needs when the operator runs newt in
    /// many directories. `load`/`resolve_id` fence by workspace, so a follower
    /// re-opens the store *with the returned path* to read that conversation.
    /// The store instance's own workspace is irrelevant here — the query spans
    /// every `workspace_key`.
    pub fn list_all(&self) -> anyhow::Result<Vec<(ConversationSummary, String)>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.title, c.persona, c.updated_at_claim, c.workspace_path,
                    (SELECT COUNT(*) FROM turns t WHERE t.conversation_id = c.id)
               FROM conversations c
              ORDER BY c.activity_tick DESC, c.id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                ConversationSummary {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    persona: row.get(2)?,
                    updated_at_unix_nanos: claim_to_u128(row.get(3)?),
                    turn_count: row.get::<_, i64>(5)?.max(0) as usize,
                },
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// A cheap cross-workspace **change cursor**: every conversation's id paired
    /// with its current `activity_tick` (the §6 per-writer Lamport tick, which is
    /// monotonic *per conversation* because the single-writer claim gives each
    /// conversation one live writer at a time). A follower keeps the previous
    /// snapshot and diffs it against the next: a **changed tick** = new turn(s);
    /// an **id that appeared** = a new session; an **id that vanished** = a
    /// deleted session. This is the one shared "did anything change" signal the
    /// web cockpit and the RichTUI dock overview poll instead of each
    /// independently re-reading whole conversations (W-coequal, newt_web_docking
    /// K6). It is a deliberately narrow projection of the `list_all` scan — id +
    /// tick only, no title/turn-count/path — so the hot refresh path stays cheap.
    /// Ordered by id for a stable, allocation-free diff. Spans all workspaces.
    pub fn session_change_index(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let conn = self.lock_conn();
        let mut stmt =
            conn.prepare("SELECT c.id, c.activity_tick FROM conversations c ORDER BY c.id ASC")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Enqueue a prompt injected from an ATTACH surface (newt-web, A3/W6) for
    /// the running session to consume as its next turn. This is the ONLY store
    /// write an attach surface makes — it never calls `create`/`claim`/
    /// `append_turn` — so the claim-holding REPL stays the SOLE writer of the
    /// transcript (D2). Workspace-fenced exactly like [`begin_prompt`]: a handle
    /// in workspace B cannot inject into workspace A's conversation. Idempotent
    /// on `idem_key` — a double-submit / SSE reconnect that reuses the key is a
    /// no-op ([`InjectOutcome::Duplicate`]), not a second enqueue.
    pub fn inject_prompt(
        &self,
        conversation_id: &str,
        body: &str,
        idem_key: Option<&str>,
    ) -> anyhow::Result<InjectOutcome> {
        validate_record_id(conversation_id)?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        // Same authority boundary as begin_prompt (store.rs): the global id is
        // an authority boundary — never inject across workspaces.
        let owner: Option<String> = tx
            .query_row(
                "SELECT workspace_key FROM conversations WHERE id = ?1",
                [conversation_id],
                |row| row.get(0),
            )
            .optional()?;
        match owner.as_deref() {
            None => anyhow::bail!("cannot inject into unknown conversation `{conversation_id}`"),
            Some(key) if key != self.workspace_id => {
                anyhow::bail!("conversation `{conversation_id}` belongs to another workspace")
            }
            _ => {}
        }
        // Idempotency: a prior row for this (conversation, idem_key) → no-op.
        if let Some(key) = idem_key {
            let seen = tx
                .query_row(
                    "SELECT 1 FROM conversation_inbox
                      WHERE conversation_id = ?1 AND idem_key = ?2",
                    rusqlite::params![conversation_id, key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if seen {
                tx.commit()?;
                return Ok(InjectOutcome::Duplicate);
            }
        }
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM conversation_inbox WHERE conversation_id = ?1",
            [conversation_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO conversation_inbox
               (id, conversation_id, workspace_key, seq, body, idem_key, delivered, injected_at_claim)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
            rusqlite::params![
                new_conversation_id(),
                conversation_id,
                self.workspace_id,
                seq,
                body,
                idem_key,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(InjectOutcome::Enqueued)
    }

    /// Dequeue the next undelivered injected prompt for `conversation_id`,
    /// marking it delivered in the SAME transaction — exactly-once even against
    /// a racing writer (the `BEGIN IMMEDIATE` RESERVED lock serializes it).
    /// Returns `Ok(None)` INSTANTLY on an empty inbox: a bounded, NON-BLOCKING
    /// poll the REPL calls at each turn boundary (it never blocks on input).
    /// Workspace-fenced.
    pub fn take_injected_prompt(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Option<InjectedPrompt>> {
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let row: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT id, body, seq FROM conversation_inbox
                  WHERE conversation_id = ?1 AND workspace_key = ?2 AND delivered = 0
                  ORDER BY seq ASC LIMIT 1",
                rusqlite::params![conversation_id, self.workspace_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((id, body, seq)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE conversation_inbox SET delivered = 1 WHERE id = ?1",
            [&id],
        )?;
        tx.commit()?;
        Ok(Some(InjectedPrompt { id, body, seq }))
    }

    /// Record the durable turn a delivered injection became — the additive,
    /// auditable "entered via web" proof. Lets the receipt itself stay
    /// `origin='operator'`, so no `prompt_receipts` CHECK migration is needed.
    pub fn link_inbox_delivery(&self, inbox_id: &str, receipt_id: &str) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE conversation_inbox SET delivered_receipt_id = ?2 WHERE id = ?1",
            [inbox_id, receipt_id],
        )?;
        Ok(())
    }

    /// TTL for pending permission requests.
    /// How long a published permission request stays answerable.
    ///
    /// Public since B0b-1 (#1842): an interaction offer's `ttl_ticks` is
    /// minted FROM this constant so the two wall clocks that used to drift
    /// independently are one number, and
    /// `b0b::the_gate_timeout_is_shorter_than_the_store_ttl` asserts the
    /// gate's shorter timeout against it.
    pub const PERMISSION_REQUEST_TTL_NANOS: i64 = 5 * 60 * 1_000_000_000;

    /// How long a staged enrollment candidate stays promotable. An enrollment
    /// asks a human to compare two six-word strings on two screens, so it is
    /// given the same 5 minutes as a permission decision — long enough to read
    /// carefully, short enough that an abandoned candidate cannot be promoted
    /// by whoever walks up next.
    pub(crate) const ENROLLMENT_REQUEST_TTL_NANOS: i64 = 5 * 60 * 1_000_000_000;

    /// Stage a candidate binding for terminal confirmation, returning the
    /// unguessable `request_id` the terminal echoes back.
    ///
    /// The web calls this. It confers nothing: the row is a proposal, and only
    /// [`take_enrollment_candidate`](Self::take_enrollment_candidate) followed
    /// by a signed registry append turns it into authority. Workspace-fenced.
    pub fn publish_enrollment_candidate(
        &self,
        conversation_id: &str,
        candidate_json: &str,
    ) -> anyhow::Result<String> {
        validate_record_id(conversation_id)?;
        let now = (self.claim_clock)();
        let request_id = new_conversation_id();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let owner: Option<String> = tx
            .query_row(
                "SELECT workspace_key FROM conversations WHERE id = ?1",
                [conversation_id],
                |row| row.get(0),
            )
            .optional()?;
        match owner.as_deref() {
            None => anyhow::bail!(
                "cannot stage an enrollment for unknown conversation `{conversation_id}`"
            ),
            Some(key) if key != self.workspace_id => {
                anyhow::bail!("conversation `{conversation_id}` belongs to another workspace")
            }
            _ => {}
        }
        tx.execute(
            "INSERT INTO enrollment_requests
               (request_id, conversation_id, workspace_key, candidate_json, resolved, created_tick)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            rusqlite::params![
                request_id,
                conversation_id,
                self.workspace_id,
                candidate_json,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(request_id)
    }

    /// The unexpired staged candidate for `conversation_id`, if any — what the
    /// terminal renders beside its own independently derived word string.
    /// Workspace-fenced, non-blocking read.
    pub fn pending_enrollment_candidate(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Option<PendingEnrollment>> {
        let conn = self.lock_conn();
        let cutoff = (self.claim_clock)().saturating_sub(Self::ENROLLMENT_REQUEST_TTL_NANOS);
        conn.query_row(
            "SELECT request_id, candidate_json FROM enrollment_requests
              WHERE conversation_id = ?1 AND workspace_key = ?2 AND resolved = 0
                AND created_tick > ?3
              ORDER BY created_tick ASC LIMIT 1",
            rusqlite::params![conversation_id, self.workspace_id, cutoff],
            |row| {
                Ok(PendingEnrollment {
                    request_id: row.get(0)?,
                    candidate_json: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// The terminal's confirmation: consume `request_id` exactly once and hand
    /// back the staged candidate for promotion.
    ///
    /// Exactly-once is the point — a candidate that has been taken, declined,
    /// or aged out yields `None`, so a replayed confirmation cannot enroll a
    /// second credential. The caller promotes what it receives; if that append
    /// fails the candidate is spent and the human re-runs the ceremony, which
    /// is the safe direction to fail.
    pub fn take_enrollment_candidate(
        &self,
        conversation_id: &str,
        request_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let cutoff = (self.claim_clock)().saturating_sub(Self::ENROLLMENT_REQUEST_TTL_NANOS);
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let candidate: Option<String> = tx
            .query_row(
                "SELECT candidate_json FROM enrollment_requests
                  WHERE request_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3
                    AND resolved = 0 AND created_tick > ?4",
                rusqlite::params![request_id, conversation_id, self.workspace_id, cutoff],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if candidate.is_some() {
            tx.execute(
                "UPDATE enrollment_requests SET resolved = 1, resolved_by = 'terminal'
                  WHERE request_id = ?1",
                [request_id],
            )?;
        }
        tx.commit()?;
        Ok(candidate)
    }

    /// The terminal declined (or the surface withdrew) — a CAS that retires the
    /// candidate. `true` when this call retired it; `false` when it was already
    /// taken or declined.
    pub fn decline_enrollment_candidate(
        &self,
        conversation_id: &str,
        request_id: &str,
    ) -> anyhow::Result<bool> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE enrollment_requests SET resolved = 1, resolved_by = 'declined'
              WHERE request_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3
                AND resolved = 0",
            rusqlite::params![request_id, conversation_id, self.workspace_id],
        )?;
        Ok(changed == 1)
    }

    /// The most-recently-active **open** conversation in this workspace —
    /// highest `activity_tick` whose `end_reason` is still NULL — or `None`
    /// when every conversation has been ended (or none exist). This is the
    /// auto-resume target: an ended conversation (`/end`, `/restart`, `:wq`)
    /// is skipped here so the next launch does not silently re-enter it, yet
    /// it stays in [`list`](Self::list) / `/recall` because it is not deleted.
    pub fn latest_open(&self) -> anyhow::Result<Option<ConversationSummary>> {
        let conn = self.lock_conn();
        conn.query_row(
            "SELECT c.id, c.title, c.persona, c.updated_at_claim,
                    (SELECT COUNT(*) FROM turns t WHERE t.conversation_id = c.id)
               FROM conversations c
              WHERE c.workspace_key = ?1 AND c.end_reason IS NULL
              ORDER BY c.activity_tick DESC, c.id DESC
              LIMIT 1",
            [&self.workspace_id],
            |row| {
                Ok(ConversationSummary {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    persona: row.get(2)?,
                    updated_at_unix_nanos: claim_to_u128(row.get(3)?),
                    turn_count: row.get::<_, i64>(4)?.max(0) as usize,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Mark a conversation **ended** with a short reason (`"new"`, `"restart"`,
    /// `"wq"`, …). Like [`rename`](Self::rename) this is metadata, not activity:
    /// it does NOT tick the §6 clock, so it cannot perturb MRU ordering — it
    /// only sets `end_reason` (the column reserved at 17.7), which
    /// [`latest_open`](Self::latest_open) reads to skip the row on auto-resume.
    /// The conversation, its turns, and its FTS rows are untouched, so
    /// `/recall` and `/conversation` still find it. Idempotent and
    /// workspace-fenced (an id from another workspace resolves as absent).
    pub fn end_conversation(&self, id: &str, reason: &str) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE conversations SET end_reason = ?2, updated_at_claim = ?3
              WHERE id = ?1 AND workspace_key = ?4",
            rusqlite::params![id, reason.trim(), now, self.workspace_id],
        )?;
        Ok(())
    }

    /// #1030: bind (or clear) a conversation's place in a roadmap tree — the
    /// Plan node whose context window this conversation IS. `roadmap_id` +
    /// `node_id` together locate the [`crate::plan::Subtask`] node; passing
    /// `None`/`None` clears the link (back to an ad-hoc chat). Workspace-fenced
    /// and idempotent. Like [`rename`](Self::rename) this is metadata, not
    /// activity: it does NOT tick the §6 clock, so it cannot perturb MRU
    /// ordering — the pointers ride the row exactly like `scratchpad`/`plan`.
    pub fn link_conversation_to_node(
        &self,
        id: &str,
        roadmap_id: Option<&str>,
        node_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // NOT `resolve_id`: a session can bind its active conversation to a Plan
        // node BEFORE its first turn is saved (so no row exists yet). The exact
        // id is used; the UPDATE is a no-op until the conversation row exists,
        // and the node→conversation forward pointer (on the roadmap side) is what
        // /roadmap next reads either way.
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE conversations SET roadmap_id = ?2, node_id = ?3
              WHERE id = ?1 AND workspace_key = ?4",
            rusqlite::params![id, roadmap_id, node_id, self.workspace_id],
        )?;
        Ok(())
    }

    // ── #1030 roadmap CRUD: the Roadmap→Phase→Plan→Task tree, persisted as a
    //    serialized plan.rs::Plan blob in the workspace-fenced `roadmaps` table ──

    /// Create (or overwrite) a roadmap with `id`, `title`, and `tree` — the
    /// serialized [`crate::plan::Plan`] of Roadmap/Phase/Plan/Task nodes.
    /// **Workspace-fenced on write as well as read (#1086):** the `roadmaps`
    /// primary key is `(id, workspace_key)`, so `INSERT OR REPLACE` can only
    /// ever replace *this* workspace's same-id row — importing an id that
    /// exists under another workspace inserts a separate row, never steals it.
    /// Overwrite-within-a-workspace is intentional (re-create replaces the
    /// tree), matching the conversation store's `create_with_id` semantics.
    pub fn create_roadmap(
        &self,
        id: &str,
        title: &str,
        tree: &crate::plan::Plan,
    ) -> anyhow::Result<()> {
        validate_record_id(id)?;
        let now = (self.claim_clock)();
        let toml = tree.to_toml_string()?;
        let conn = self.lock_conn();
        conn.execute(
            "INSERT OR REPLACE INTO roadmaps
               (id, workspace_key, title, tree, schema_version, created_at_claim, updated_at_claim)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            rusqlite::params![
                id,
                self.workspace_id,
                title.trim(),
                toml,
                ROADMAP_SCHEMA_VERSION,
                now
            ],
        )?;
        Ok(())
    }

    /// Load roadmap `id` (workspace-fenced), deserializing its `tree` blob back
    /// into a [`crate::plan::Plan`]. `None` when absent. A tree column that is
    /// not valid plan TOML is a hard error — never hand back a garbage tree.
    pub fn load_roadmap(&self, id: &str) -> anyhow::Result<Option<Roadmap>> {
        let conn = self.lock_conn();
        let row = conn
            .query_row(
                "SELECT title, tree FROM roadmaps WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id, self.workspace_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((title, toml)) = row else {
            return Ok(None);
        };
        let tree = crate::plan::Plan::from_toml_str(&toml).map_err(|e| {
            anyhow::anyhow!("roadmap `{id}`: tree column is not valid plan TOML ({e})")
        })?;
        Ok(Some(Roadmap {
            id: id.to_string(),
            title,
            tree,
        }))
    }

    /// Replace roadmap `id`'s tree (workspace-fenced). A no-op if absent.
    pub fn update_roadmap(&self, id: &str, tree: &crate::plan::Plan) -> anyhow::Result<()> {
        let now = (self.claim_clock)();
        let toml = tree.to_toml_string()?;
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE roadmaps SET tree = ?2, updated_at_claim = ?3
              WHERE id = ?1 AND workspace_key = ?4",
            rusqlite::params![id, toml, now, self.workspace_id],
        )?;
        Ok(())
    }

    /// This workspace's roadmaps, most-recently-updated first.
    pub fn list_roadmaps(&self) -> anyhow::Result<Vec<RoadmapSummary>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, tree FROM roadmaps
              WHERE workspace_key = ?1
              ORDER BY updated_at_claim DESC, id DESC",
        )?;
        let rows = stmt.query_map([&self.workspace_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, title, toml) = row?;
            let node_count = crate::plan::Plan::from_toml_str(&toml)
                .map(|p| p.subtasks.len())
                .unwrap_or(0);
            out.push(RoadmapSummary {
                id,
                title,
                node_count,
            });
        }
        Ok(out)
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

    /// This conversation's current title, or `None` when no row exists yet
    /// (a fresh session's record is created lazily on the first saved turn).
    /// Workspace-fenced like [`exists`](Self::exists); a cheap single-row read
    /// — the rich footer refreshes it every turn (#1671).
    pub fn title(&self, id: &str) -> anyhow::Result<Option<String>> {
        let conn = self.lock_conn();
        let title = conn
            .query_row(
                "SELECT title FROM conversations WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id, self.workspace_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(title)
    }

    /// Persist a conversation's scratchpad `<state>` snapshot (#713). The map
    /// is serialized to JSON and written to the conversation row's `scratchpad`
    /// column so an interrupt + auto-resume can re-hydrate the live store.
    ///
    /// Like [`rename`](Self::rename) / [`end_conversation`](Self::end_conversation)
    /// this is metadata, not activity: it does **not** tick the §6 clock, so it
    /// cannot perturb MRU ordering, and the scratchpad is NOT part of the §6
    /// content chain (it rides the conversation row, never a turn's canonical
    /// encoding) — working memory, not provenance. Workspace-fenced and
    /// idempotent: an id from another workspace resolves as absent and the
    /// UPDATE matches nothing.
    pub fn update_scratchpad(
        &self,
        id: &str,
        scratchpad: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let json = serde_json::to_string(scratchpad)?;
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE conversations SET scratchpad = ?2 WHERE id = ?1 AND workspace_key = ?3",
            rusqlite::params![id, json, self.workspace_id],
        )?;
        Ok(())
    }

    /// Persist a conversation's plan-ledger snapshot (#715). The
    /// [`crate::PlanSnapshot`] is serialized to JSON and written to the
    /// conversation row's `plan` column so an interrupt + auto-resume can
    /// re-hydrate the live ledger (the `<plan>` block + `plan_get` survive).
    ///
    /// Like [`update_scratchpad`](Self::update_scratchpad) this is metadata, not
    /// activity: it does **not** tick the §6 clock, so it cannot perturb MRU
    /// ordering, and the plan is NOT part of the §6 content chain (it rides the
    /// conversation row, never a turn's canonical encoding) — working memory, not
    /// provenance. Workspace-fenced and idempotent: an id from another workspace
    /// resolves as absent and the UPDATE matches nothing.
    pub fn update_plan_snapshot(&self, id: &str, plan: &crate::PlanSnapshot) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let json = serde_json::to_string(plan)?;
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE conversations SET plan = ?2 WHERE id = ?1 AND workspace_key = ?3",
            rusqlite::params![id, json, self.workspace_id],
        )?;
        Ok(())
    }

    /// Persist a conversation's operator preference pin (#1668) — the
    /// [`crate::OperatorPreferencePin`] is serialized to JSON and written to the
    /// conversation row's `preference_pin` column so resuming the conversation
    /// can re-apply the operator's pinned backend/model/cognition/tenacity.
    ///
    /// Like [`update_scratchpad`](Self::update_scratchpad) /
    /// [`update_plan_snapshot`](Self::update_plan_snapshot) this is metadata,
    /// not activity: it does **not** tick the §6 clock, so it cannot perturb
    /// MRU ordering, and the pin is NOT part of the §6 content chain (it rides
    /// the conversation row, never a turn's canonical encoding) — session
    /// preference, not provenance. Workspace-fenced and idempotent: an id from
    /// another workspace resolves as absent and the UPDATE matches nothing.
    ///
    /// Authority boundary (see [`crate::OperatorPreferencePin`]'s type doc):
    /// this column carries operator PREFERENCE only — never OCAP grants,
    /// caveat clamps, sandbox/capability state, credentials, or endpoints.
    /// `backend` is a NAME resolved against the operator's own `Config` at
    /// apply time, so a row can select among backends the operator already
    /// configured but can never define or reach one.
    pub fn update_preference_pin(
        &self,
        id: &str,
        pin: &crate::OperatorPreferencePin,
    ) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let json = serde_json::to_string(pin)?;
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE conversations SET preference_pin = ?2 WHERE id = ?1 AND workspace_key = ?3",
            rusqlite::params![id, json, self.workspace_id],
        )?;
        Ok(())
    }

    /// Test-only seam (#1668): write RAW text into the `preference_pin` column
    /// so tests — including ones in dependent crates, which cannot reach the
    /// private connection — can exercise the tampered / corrupt-row fail-open
    /// path. Exposed for the same reason [`crate::test_guard`] is: the
    /// behavior under test spans crates. Production code must use
    /// [`update_preference_pin`](Self::update_preference_pin), which can only
    /// ever write a well-formed pin.
    #[doc(hidden)]
    pub fn set_raw_preference_pin_for_test(&self, id: &str, raw: &str) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE conversations SET preference_pin = ?2 WHERE id = ?1",
            rusqlite::params![id, raw],
        )?;
        Ok(())
    }

    /// This conversation's operator preference pin (#1668), or `None` when no
    /// row exists yet (a fresh session's record is created lazily on the first
    /// saved turn). Workspace-fenced like [`title`](Self::title). Strict
    /// decode — never hand back garbage (same discipline as the scratchpad /
    /// plan columns), and `deny_unknown_fields` on the pin makes that strictness
    /// the authority guard too: a row tampered with authority-shaped keys is
    /// REFUSED, not silently narrowed. A pre-#1668 row carries the `'{}'`
    /// backfill and parses to the empty pin, which resume treats as a no-op.
    pub fn preference_pin(&self, id: &str) -> anyhow::Result<Option<crate::OperatorPreferencePin>> {
        let conn = self.lock_conn();
        let json = conn
            .query_row(
                "SELECT preference_pin FROM conversations WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id, self.workspace_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match json {
            Some(json) => {
                let pin = serde_json::from_str(&json).map_err(|e| {
                    anyhow::anyhow!(
                        "conversation `{id}`: preference_pin column is not a valid \
                         operator preference pin ({e}); refusing to load garbage"
                    )
                })?;
                Ok(Some(pin))
            }
            None => Ok(None),
        }
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
        // Byte-case-exact prefix match (review NIT N5 on #261): `LIKE` is
        // ASCII-case-insensitive by default, which silently widened prefix
        // resolution when the JSON backend's `starts_with` was ported.
        // `substr` compares exactly; ids are validated ASCII above, so
        // character positions and byte positions coincide.
        let mut stmt = conn.prepare(
            "SELECT id FROM conversations
              WHERE workspace_key = ?1 AND substr(id, 1, length(?2)) = ?2
              ORDER BY id ASC",
        )?;
        let matches = stmt
            .query_map(rusqlite::params![self.workspace_id, id_or_prefix], |row| {
                row.get::<_, String>(0)
            })?
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
    /// the genesis hash, and the stored tip witness must match the recorded
    /// last writer's final turn (an EMPTY witness — the schema-diff backfill —
    /// is absence of evidence and skips only the tip comparison). A tampered
    /// row (content OR claims — claims are inside the canonical encoding, so
    /// they are tamper-evident too) breaks the chain.
    pub fn verify_chain(&self, id: &str) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let conn = self.lock_conn();
        // Same single-snapshot discipline as `load_verified`: the chain walk
        // and the tip-witness comparison read the database twice, and a
        // writer on another connection between those reads would fabricate a
        // corruption verdict. One DEFERRED read transaction pins one
        // snapshot for both.
        let tx = conn.unchecked_transaction()?;
        Self::verify_conversation_chain(&tx, &id)?;
        tx.commit()?;
        Ok(())
    }

    /// The §6 verification body, on the caller's connection — the shared
    /// kernel of [`Self::verify_chain`] and [`Self::load_verified`]. Taking
    /// `&Connection` is what lets `load_verified` verify and materialize
    /// inside the SAME read transaction: the record it returns is the
    /// snapshot this function checked, not a later one.
    ///
    /// Callers MUST hold a read transaction when other connections may write
    /// concurrently; this function performs multiple reads and does not open
    /// one itself.
    ///
    /// What a failure means, precisely:
    ///
    /// * A per-turn diagnosis (`does not link`, `genesis`, `seq order`) names
    ///   the first turn whose link is broken — that turn or its predecessor
    ///   was altered.
    /// * A tip-witness diagnosis means the per-turn links all held. The final
    ///   turn has no successor linking to it, so its content is pinned ONLY
    ///   by the witness — and a witness mismatch therefore cannot say which
    ///   side was altered: the final turn or the witness itself. The message
    ///   says exactly that and no more.
    /// * An EMPTY `tip_hash` (`''`, the schema-diff backfill for databases
    ///   predating the column — `writer_fingerprint` may still hold a real
    ///   value from an earlier schema epoch) is absence of evidence, not
    ///   evidence of tampering: the per-turn links are still fully verified,
    ///   the tip comparison is skipped, and the next append records a real
    ///   witness. Refusing on absence would lock restores out of exactly the
    ///   oldest histories while asserting a conclusion nothing recorded
    ///   supports — the same policy the append path applies. A PRESENT
    ///   `tip_hash` with a blank writer is the reverse mix, which nothing
    ///   produces, and refuses.
    fn verify_conversation_chain(conn: &Connection, id: &str) -> anyhow::Result<()> {
        // `.optional()` + a named not-found: `resolve_id` ran on an earlier
        // lock acquisition, so a conversation deleted in the gap (another
        // session's `/conversation delete`, retention pruning) reaches this
        // read as an absent row. That is a plain deletion, not corruption —
        // surfacing rusqlite's raw QueryReturnedNoRows here would dress a
        // legitimate concurrent delete as an integrity-shaped refusal and
        // send the operator hunting for tampering that never happened.
        let (tip, tip_writer): (String, String) = conn
            .query_row(
                "SELECT tip_hash, writer_fingerprint FROM conversations WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("conversation `{id}` not found"))?;

        let mut stmt = conn.prepare(
            "SELECT conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                    events, tokens_in, tokens_out, ts_claim, encoding_version,
                    phantom_reaches, sources
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
                // Seqs are PER-WRITER Lamport ticks: without the writer they
                // do not even identify a row in a multi-writer history, so
                // the diagnosis names the writer it already holds.
                if row.seq <= p.seq {
                    anyhow::bail!(
                        "chain violation in `{id}`: writer {} seq {} not strictly \
                         after {}",
                        row.writer_fingerprint,
                        row.seq,
                        p.seq
                    );
                }
                if row.prev_hash != p.content_hash()? {
                    anyhow::bail!(
                        "chain violation in `{id}`: writer {} turn seq {} does not \
                         link to seq {} (row tampered or out of order)",
                        row.writer_fingerprint,
                        row.seq,
                        p.seq
                    );
                }
            } else {
                let genesis = genesis_hash(id, &row.writer_fingerprint);
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

        // #1786 provenance checks, v2 rows ONLY (§3.2: a v1 row's sources
        // bytes are never interpreted — its arm predates the concept and an
        // out-of-band edit to them surfaces as a loud content-id orphan on
        // rows that cite it, never as a silent retarget).
        //
        // Order matters: the per-turn walk above already proved every row's
        // stored bytes are the appended bytes, so the checks below reason
        // about VERIFIED bytes, not attacker-writable ones.
        //
        // NO ORDERING RULE, deliberately (spec §5, r1 finding): membership is
        // the whole test — a cited id may belong to a turn appearing LATER in
        // the conversation. Cross-writer seqs are per-writer Lamport ticks,
        // not a causal order, so an r1 seq-ordering check produced only false
        // positives and permanently bricked legitimate conversations. Dropped.
        // Temporal soundness of a citation is an explicit NON-CLAIM: the
        // record proves what was reachable, not when the producer knew it.
        // What IS unconstructible is self/mutual citation — sources ride in
        // the content-id preimage, so either would need a BLAKE3 fixed point
        // (see `TurnRow::content_id`).
        let content_ids: std::collections::HashSet<String> =
            rows.iter().map(|r| r.content_id()).collect();
        for row in &rows {
            if row.encoding_version < 2 {
                continue;
            }
            let cited = parse_canonical_sources(&row.sources).map_err(|e| {
                anyhow::anyhow!(
                    "chain violation in `{id}`: writer {} turn seq {} carries a \
                     sources column that is not in canonical form ({e}) — \
                     well-formed evidence or none",
                    row.writer_fingerprint,
                    row.seq
                )
            })?;
            if !cited.is_empty() {
                // Derived-row shape invariant (#1786 §3): a derived row is
                // harness-minted — a row claiming BOTH derivation and tool
                // activity would break the witnessed/derived classification
                // provenance consumers depend on.
                if row.events != "[]" || row.phantom_reaches != "[]" {
                    anyhow::bail!(
                        "chain violation in `{id}`: writer {} turn seq {} claims \
                         derivation (non-empty sources) AND tool activity — a \
                         derived row is harness-minted and carries neither events \
                         nor phantom reaches",
                        row.writer_fingerprint,
                        row.seq
                    );
                }
                for source in &cited {
                    if !content_ids.contains(source) {
                        anyhow::bail!(
                            "chain violation in `{id}`: writer {} turn seq {} cites \
                             source `{source}`, which matches no turn in this \
                             conversation — an orphan citation is an unattributable \
                             assertion. Rows are left exactly as found.",
                            row.writer_fingerprint,
                            row.seq
                        );
                    }
                }
            }
        }

        // The stored tip belongs to the conversation row's RECORDED last
        // writer (set at create, updated on every append in the same txn) —
        // not whoever happens to be verifying. This keeps verification
        // writer-agnostic: a store that authored no turns in a
        // migrated/foreign conversation still verifies it correctly
        // (adversarial-review finding N2 on #261). The final row comes from
        // the SAME `rows` read as the walk above — one snapshot, one verdict.
        Self::check_tip_witness(
            id,
            &tip,
            &tip_writer,
            rows.iter().rfind(|r| r.writer_fingerprint == tip_writer),
        )?;

        // #1786 §5 — per-writer witnesses, both directions. (Agreement
        // between the conversations-row tip and writer_tips needs no third
        // check: at equal seq, each is independently compared against the
        // same row's hash, so divergence between them cannot survive both.)
        let mut witness_stmt = conn.prepare(
            "SELECT writer_fingerprint, tip_hash, tip_seq FROM writer_tips
              WHERE conversation_id = ?1",
        )?;
        let witnesses = witness_stmt
            .query_map([&id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (writer, tip_hash, tip_seq) in &witnesses {
            let final_seq = rows
                .iter()
                .rfind(|r| &r.writer_fingerprint == writer)
                .map(|r| r.seq);
            match final_seq {
                Some(final_seq) => {
                    let row_at = rows
                        .iter()
                        .find(|r| &r.writer_fingerprint == writer && r.seq == *tip_seq);
                    Self::check_writer_tip_witness(
                        id, writer, tip_hash, *tip_seq, final_seq, row_at,
                    )?;
                }
                None => {
                    // A witness for a writer with no turns: legitimate ONLY
                    // as a genesis witness (a zero-turn create/import shape).
                    // Anything else means either the writer's turns were all
                    // deleted or the witness was planted.
                    if tip_hash != &genesis_hash(id, writer) || *tip_seq != 0 {
                        anyhow::bail!(
                            "chain violation in `{id}`: a witness exists for writer \
                             {writer}, which has no recorded turns, and it is not \
                             that writer's genesis witness — either the writer's \
                             turns were deleted or the witness was planted. Rows \
                             are left exactly as found."
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// The #1786 §5 per-writer witness verdict — the ONE owner of the
    /// seq-aware semantics, shared by the append path (a writer checking its
    /// OWN witness before chaining) and the read path (verification checking
    /// every writer's witness). `row_at_tip_seq` is the writer's turn at the
    /// seq the witness names, under the caller's snapshot.
    ///
    /// Three verdicts (`tip_seq` vs the writer's final seq):
    /// * equal — the witness pins the final turn; hash must match.
    /// * LOWER — stale but honest: a rolled-back binary appended without
    ///   maintaining `writer_tips`. The witness still pins the interior turn
    ///   it names, so it is verified against THAT row — evidence is spent,
    ///   not discarded — and the writer's next append under a current binary
    ///   repairs it. Not a violation: without the seq column, stale would be
    ///   indistinguishable from tampered and re-upgrading after a rollback
    ///   would refuse legitimate history.
    /// * HIGHER — rows were deleted out from under the witness: violation.
    fn check_writer_tip_witness(
        id: &str,
        writer: &str,
        tip_hash: &str,
        tip_seq: i64,
        final_seq: i64,
        row_at_tip_seq: Option<&TurnRow>,
    ) -> anyhow::Result<()> {
        if tip_seq > final_seq {
            anyhow::bail!(
                "chain violation in `{id}`: writer {writer}'s witness pins seq \
                 {tip_seq}, past its last recorded turn (seq {final_seq}) — turns \
                 were deleted out from under the witness. Rows are left exactly \
                 as found."
            );
        }
        match row_at_tip_seq {
            Some(row) => {
                if tip_hash != row.content_hash()? {
                    anyhow::bail!(
                        "chain violation in `{id}`: writer {writer}'s witness \
                         disagrees with its turn at seq {tip_seq}. The per-turn \
                         links do not cover a writer's final turn, so this check \
                         cannot localize the alteration further. Rows are left \
                         exactly as found."
                    );
                }
            }
            None => anyhow::bail!(
                "chain violation in `{id}`: writer {writer}'s witness pins seq \
                 {tip_seq}, but no such turn exists. Rows are left exactly as \
                 found."
            ),
        }
        Ok(())
    }

    /// The §6 tip-witness comparison — the ONE owner of the witness policy
    /// and its diagnostics, shared by [`Self::verify_conversation_chain`]
    /// (read path) and [`Self::append_turn_full`] (write path). `final_row`
    /// is the tip writer's last recorded turn under the caller's snapshot
    /// (`None` = that writer has no turns, so the witness must equal its
    /// genesis hash).
    ///
    /// An empty `recorded_tip` is the schema-diff backfill — absence of
    /// evidence — and passes. A nonempty tip with an empty `tip_writer`
    /// refuses because no write path or migration produces it; see
    /// [`Self::verify_conversation_chain`] for the policy.
    ///
    /// LIMITATION, stated rather than papered over: this policy means an
    /// attacker who can write SQL can erase the witness — blanking the ONE
    /// `tip_hash` column — along with tampering the final turn, and the
    /// erasure is indistinguishable from a legitimately migrated database
    /// (whose fixture state is exactly writer-set/tip-blank) — so the tamper
    /// passes. That is not a defect of the policy but the boundary of
    /// the mechanism: a hash chain with no secret and no out-of-store anchor
    /// can never bind an adversary who can rewrite the store itself (they
    /// could equally recompute the entire chain). What the chain + witness DO
    /// hold against is accidental mutation, careless migration, and tampering
    /// that does not think to cover its tracks. Binding a stronger adversary
    /// requires an anchor outside the database — #1786's provenance work is
    /// where that boundary moves.
    fn check_tip_witness(
        id: &str,
        recorded_tip: &str,
        tip_writer: &str,
        final_row: Option<&TurnRow>,
    ) -> anyhow::Result<()> {
        // A blank TIP is legitimate absence: the schema-diff backfill blanks
        // `tip_hash` on databases predating the column while an earlier-epoch
        // `writer_fingerprint` may hold a real value — the drifted-schema
        // fixture in tests/store.rs hand-writes exactly that state, and the
        // first post-migration append repairs it.
        if recorded_tip.is_empty() {
            return Ok(());
        }
        // The REVERSE has no producer: every write of `tip_hash` (create,
        // append) writes `writer_fingerprint` in the same statement, and no
        // migration blanks the writer while keeping the tip. A witness hash
        // that names no writer is evidence someone altered the witness
        // columns themselves.
        if tip_writer.is_empty() {
            anyhow::bail!(
                "chain violation in `{id}`: the conversation records a tip witness \
                 but no writer to attribute it to — a state no newt write path or \
                 migration produces. The witness columns themselves appear \
                 altered; rows are left exactly as found."
            );
        }
        match final_row {
            Some(row) => {
                if recorded_tip != row.content_hash()? {
                    anyhow::bail!(
                        "chain violation in `{id}`: the tip witness disagrees with \
                         writer `{tip_writer}` at its final turn (seq {}). The \
                         per-turn links do not cover the final turn — nothing \
                         chains onto it — so this check cannot localize the \
                         alteration further: an altered final turn, an altered \
                         witness, and deleted trailing turns all produce exactly \
                         this disagreement. Rows are left exactly as found.",
                        row.seq
                    );
                }
            }
            None => {
                if recorded_tip != genesis_hash(id, tip_writer) {
                    anyhow::bail!(
                        "chain violation in `{id}`: the tip witness names writer \
                         `{tip_writer}`, which has no recorded turns, yet does not \
                         equal that writer's genesis hash. An altered witness, \
                         altered writer attribution, and deletion of that \
                         writer's turns all produce exactly this state; rows are \
                         left exactly as found."
                    );
                }
            }
        }
        Ok(())
    }

    /// Full-text recall over this workspace's turns (17.3, issue #246).
    ///
    /// The raw query goes through [`sanitize_fts5_query`] (an empty result
    /// after sanitizing is an error, never a match-all), then a `MATCH`
    /// against the trigger-maintained `turns_fts` index, ranked by bm25
    /// (best first), **fenced to this workspace** by joining
    /// `conversations.workspace_key`. Each hit carries a `snippet()` of the
    /// matched column — the match wrapped in `>>>`/`<<<`, roughly ±10
    /// tokens of context, `…` at trimmed edges. Snippets are the whole
    /// payload by design: no full turn content, no aux-LLM recaps (the
    /// design doc explicitly skips those — slow and expensive on local
    /// models; the hermes study's own "snippet is enough, saves tokens").
    pub fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<SearchHit>> {
        let fts_query = sanitize_fts5_query(query)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let conn = self.lock_conn();
        // The JOIN on `turns` is also a safety net: an index entry whose
        // turn row is gone (can't happen while the delete trigger holds,
        // but defense in depth) joins to nothing instead of surfacing a
        // ghost hit. Ties in rank break deterministically by (id, seq).
        let mut stmt = conn.prepare(
            "SELECT t.conversation_id, c.title, t.seq,
                    snippet(turns_fts, -1, '>>>', '<<<', '…', 21),
                    bm25(turns_fts)
               FROM turns_fts
               JOIN turns t ON t.rowid = turns_fts.rowid
               JOIN conversations c
                 ON c.id = t.conversation_id AND c.workspace_key = ?2
              WHERE turns_fts MATCH ?1
              ORDER BY bm25(turns_fts) ASC, t.conversation_id ASC, t.seq ASC
              LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![fts_query, self.workspace_id, limit],
            |row| {
                Ok(SearchHit {
                    conversation_id: row.get(0)?,
                    title: row.get(1)?,
                    seq: row.get(2)?,
                    snippet: row.get(3)?,
                    rank: row.get(4)?,
                })
            },
        )?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    /// Drive the display-claim clock from a test. Hidden, test-only: lets the
    /// §6 clock-skew test write *honestly skewed* claims through the normal
    /// API (clock runs backwards mid-conversation) and prove that ordering,
    /// MRU, and chain verification are all unaffected.
    #[doc(hidden)]
    pub fn set_claim_clock_for_test(&mut self, clock: fn() -> i64) {
        self.claim_clock = clock;
    }

    /// Inject a fake liveness oracle for tests (mirrors
    /// [`set_claim_clock_for_test`](Self::set_claim_clock_for_test)) so #1030
    /// claim contention is unit-testable without touching real OS pids.
    #[doc(hidden)]
    pub fn set_liveness_for_test(&mut self, liveness: LivenessFn) {
        self.liveness = liveness;
    }

    /// Force this store's owner identity for tests — lets one test process
    /// simulate a SECOND newt (a different pid/host) contending for the same
    /// conversation, without spawning a real process.
    #[doc(hidden)]
    pub fn set_owner_for_test(&mut self, host: &str, boot_id: &str, pid: i64) {
        self.host = host.to_string();
        self.boot_id = boot_id.to_string();
        self.pid = pid;
    }

    /// Prune old, inactive conversations without ever deleting the row the
    /// caller just created.
    ///
    /// Victim selection and deletion share one `BEGIN IMMEDIATE`
    /// transaction. The old two-phase implementation selected ids, released
    /// the database lock, then called [`delete`](Self::delete) for each id; a
    /// concurrent `begin_prompt` could refresh a selected conversation in that
    /// gap and then lose its newly committed receipt to the stale delete.
    /// Live-owned conversations are also ineligible: a retention cap must not
    /// erase another running session.
    fn prune_to_cap_excluding(&self, protected_id: &str) -> anyhow::Result<()> {
        if self.max_per_workspace == 0 {
            return Ok(());
        }
        let now = (self.claim_clock)();
        let victims: Vec<String> = {
            let conn = self.lock_conn();
            let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM conversations WHERE workspace_key = ?1",
                [&self.workspace_id],
                |row| row.get(0),
            )?;
            let excess = count - self.max_per_workspace as i64;
            if excess <= 0 {
                return Ok(());
            }
            // Oldest = lowest activity tick (§6 — never a timestamp). Gather
            // candidates first, then consult the same liveness oracle as
            // `claim`: a live owner is protected, while a crashed process's
            // stale row must not defeat retention forever.
            let candidates = {
                let mut stmt = tx.prepare(
                    "SELECT c.id
                       FROM conversations c
                      WHERE c.workspace_key = ?1
                        AND c.id <> ?2
                      ORDER BY c.activity_tick ASC, c.id ASC",
                )?;
                let selected = stmt
                    .query_map(rusqlite::params![self.workspace_id, protected_id], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                selected
            };
            let mut ids = Vec::with_capacity(excess as usize);
            for id in candidates {
                if ids.len() >= excess as usize {
                    break;
                }
                if let Some(owner) = live_owner_row(&tx, &id)? {
                    if (self.liveness)(&owner, now) {
                        continue;
                    }
                    tx.execute("DELETE FROM live_owners WHERE conversation_id = ?1", [&id])?;
                }
                ids.push(id);
            }
            for id in &ids {
                tx.execute(
                    "DELETE FROM conversations WHERE id = ?1 AND workspace_key = ?2",
                    rusqlite::params![id, self.workspace_id],
                )?;
            }
            tx.commit()?;
            ids
        };
        for id in victims {
            // Database deletion is already committed atomically above. Plan
            // directories are best-effort derived state and must never block
            // retention cleanup.
            let plan_dir = self.workspace.join(session_plan_dir(&id));
            let _ = std::fs::remove_dir_all(plan_dir);
        }
        Ok(())
    }

    /// The workspace fence this store was opened against.
    ///
    /// A3 (#1837) keeps its `ResolutionStore` implementation in its own
    /// module rather than growing this file, so it needs the same fence
    /// every table here carries.
    pub(crate) fn workspace_fence(&self) -> &str {
        &self.workspace_id
    }

    /// The display-only wall clock, for A3's `resolved_tick` audit column.
    ///
    /// Display-only on purpose: nothing in the exactly-once decision
    /// consults it, exactly as nothing in prompt ordering does.
    pub(crate) fn claim_tick(&self) -> i64 {
        (self.claim_clock)()
    }

    pub(crate) fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned mutex means another thread panicked mid-operation; the
        // connection itself is still usable (transactions roll back), so
        // recover rather than cascade the panic.
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One full-text recall hit from [`ConversationStore::search`] (17.3).
///
/// `rank` is the raw FTS5 bm25 score: negative, and smaller (more negative)
/// = better. `search` returns hits best-first; the value is exposed so
/// 17.4/17.5 callers can show or threshold it. `snippet` is the matched
/// column's excerpt (`>>>match<<<`, `…`-trimmed) — deliberately the only
/// content returned.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// The conversation the matching turn belongs to.
    pub conversation_id: String,
    /// That conversation's current title.
    pub title: String,
    /// The matching turn's §6 per-writer tick (its position in the chain).
    pub seq: i64,
    /// `snippet()` of the matched column: ±~10 tokens of context around the
    /// match, which is wrapped in `>>>`/`<<<`; `…` marks trimmed edges.
    pub snippet: String,
    /// Raw bm25 rank (negative; more negative = better match).
    pub rank: f64,
}

/// SQLite representation of an immutable prompt receipt. Conversion is kept
/// separate from rusqlite's row callback so address parsing and cryptographic
/// verification can return rich `anyhow` errors.
#[derive(Debug)]
struct PromptRow {
    id: String,
    conversation_id: String,
    writer_fingerprint: String,
    seq: i64,
    previous_prompt_id: Option<String>,
    parent_prompt_id: Option<String>,
    root_prompt_id: String,
    active_operator_id: Option<String>,
    origin: String,
    raw_text: Vec<u8>,
    model_text: Vec<u8>,
    raw_digest: String,
    model_digest: String,
    receipt_hash: String,
    ts_claim: i64,
    encoding_version: i64,
}

impl PromptRow {
    fn into_receipt(self) -> anyhow::Result<PromptReceipt> {
        let receipt = PromptReceipt::from_stored_parts(
            self.id.parse()?,
            self.conversation_id,
            self.writer_fingerprint,
            self.seq,
            self.previous_prompt_id.map(|id| id.parse()).transpose()?,
            self.parent_prompt_id.map(|id| id.parse()).transpose()?,
            self.root_prompt_id.parse()?,
            self.active_operator_id.map(|id| id.parse()).transpose()?,
            PromptOrigin::from_db_str(&self.origin)?,
            self.raw_text,
            self.model_text,
            self.raw_digest,
            self.model_digest,
            self.receipt_hash,
            self.ts_claim,
            self.encoding_version,
        );
        receipt.verify_integrity()?;
        Ok(receipt)
    }
}

fn prompt_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<PromptRow> {
    Ok(PromptRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        writer_fingerprint: row.get(2)?,
        seq: row.get(3)?,
        previous_prompt_id: row.get(4)?,
        parent_prompt_id: row.get(5)?,
        root_prompt_id: row.get(6)?,
        active_operator_id: row.get(7)?,
        origin: row.get(8)?,
        raw_text: row.get(9)?,
        model_text: row.get(10)?,
        raw_digest: row.get(11)?,
        model_digest: row.get(12)?,
        receipt_hash: row.get(13)?,
        ts_claim: row.get(14)?,
        encoding_version: row.get(15)?,
    })
}

fn insert_prompt_receipt(conn: &Connection, receipt: &PromptReceipt) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO prompt_receipts
           (id, conversation_id, writer_fingerprint, seq, previous_prompt_id,
            parent_prompt_id, root_prompt_id, active_operator_id, origin,
            raw_text, model_text, raw_digest, model_digest, receipt_hash,
            ts_claim, encoding_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        rusqlite::params![
            receipt.id().to_string(),
            receipt.conversation_id(),
            receipt.writer_fingerprint(),
            receipt.seq(),
            receipt.previous_prompt_id().map(|id| id.to_string()),
            receipt.parent_prompt_id().map(|id| id.to_string()),
            receipt.root_prompt_id().to_string(),
            receipt.active_operator_id().map(|id| id.to_string()),
            receipt.origin().as_db_str(),
            receipt.raw_text(),
            receipt.model_text(),
            receipt.raw_digest(),
            receipt.model_digest(),
            receipt.receipt_hash(),
            receipt.ts_claim(),
            receipt.encoding_version(),
        ],
    )?;
    Ok(())
}

fn load_prompt_on_conn(
    conn: &Connection,
    prompt_id: PromptId,
    workspace_id: &str,
) -> anyhow::Result<Option<PromptReceipt>> {
    let row = conn
        .query_row(
            "SELECT p.id, p.conversation_id, p.writer_fingerprint, p.seq,
                    p.previous_prompt_id, p.parent_prompt_id, p.root_prompt_id,
                    p.active_operator_id, p.origin, p.raw_text, p.model_text,
                    p.raw_digest, p.model_digest, p.receipt_hash, p.ts_claim,
                    p.encoding_version
               FROM prompt_receipts p
               JOIN conversations c ON c.id = p.conversation_id
              WHERE p.id = ?1 AND c.workspace_key = ?2",
            rusqlite::params![prompt_id.to_string(), workspace_id],
            prompt_row_from_sql,
        )
        .optional()?;
    row.map(PromptRow::into_receipt).transpose()
}

fn load_prompt_in_conversation_on_conn(
    conn: &Connection,
    conversation_id: &str,
    prompt_id: PromptId,
    workspace_id: &str,
) -> anyhow::Result<Option<PromptReceipt>> {
    let row = conn
        .query_row(
            "SELECT p.id, p.conversation_id, p.writer_fingerprint, p.seq,
                    p.previous_prompt_id, p.parent_prompt_id, p.root_prompt_id,
                    p.active_operator_id, p.origin, p.raw_text, p.model_text,
                    p.raw_digest, p.model_digest, p.receipt_hash, p.ts_claim,
                    p.encoding_version
               FROM prompt_receipts p
               JOIN conversations c ON c.id = p.conversation_id
              WHERE p.id = ?1 AND p.conversation_id = ?2
                AND c.workspace_key = ?3",
            rusqlite::params![prompt_id.to_string(), conversation_id, workspace_id],
            prompt_row_from_sql,
        )
        .optional()?;
    row.map(PromptRow::into_receipt).transpose()
}

fn validate_objective_root_on_conn(
    conn: &Connection,
    conversation_id: &str,
    root_prompt_id: PromptId,
    workspace_id: &str,
) -> anyhow::Result<PromptReceipt> {
    let root =
        load_prompt_in_conversation_on_conn(conn, conversation_id, root_prompt_id, workspace_id)?
            .ok_or_else(|| {
            anyhow::anyhow!(
                "prompt root {root_prompt_id} is missing from conversation `{conversation_id}`"
            )
        })?;
    if root.origin() != PromptOrigin::Operator
        || root.root_prompt_id() != root.id()
        || root
            .active_operator_id()
            .is_some_and(|active| active != root.id())
    {
        anyhow::bail!("prompt root {root_prompt_id} is not a self-rooted operator prompt");
    }
    Ok(root)
}

/// Resolve the nearest operator authority through explicit semantic parentage.
///
/// Version-2 receipts persist and hash the expected result, but the parent walk
/// remains the validator: a stored pointer that disagrees with its parent's
/// authority is rejected. Version-1 rows have no pointer and are recovered by
/// the same walk, preserving receipts written before the additive column. The
/// walk is iterative and capped at [`MAX_PROMPT_LINEAGE_DEPTH`] receipts so a
/// corrupt database cannot induce stack growth or unbounded traversal.
fn resolve_active_operator_on_conn(
    conn: &Connection,
    conversation_id: &str,
    submitted: &PromptReceipt,
    workspace_id: &str,
) -> anyhow::Result<(PromptReceipt, usize)> {
    validate_objective_root_on_conn(
        conn,
        conversation_id,
        submitted.root_prompt_id(),
        workspace_id,
    )?;
    let objective_root = submitted.root_prompt_id();
    let mut visited = std::collections::HashSet::new();
    let mut retry_authorities: Vec<(PromptId, Option<PromptId>)> = Vec::new();
    let mut current = submitted.clone();

    for depth in 1..=MAX_PROMPT_LINEAGE_DEPTH {
        if !visited.insert(current.id()) {
            anyhow::bail!("prompt parent cycle detected at {}", current.id());
        }
        if current.conversation_id() != conversation_id
            || current.root_prompt_id() != objective_root
        {
            anyhow::bail!(
                "prompt {} crosses its conversation or objective-root boundary",
                current.id()
            );
        }

        // A continuation hop: an operator DECISION/CLARIFICATION reply (or a
        // harness retry) names its lineage's authority rather than itself
        // (bug/steering-regressions). Both walk to their parent; only an
        // operator prompt that IS its own authority terminates the walk.
        // Parent-bearing is the primary signal so v1 rows (no persisted
        // pointer) recover the same authority by walking explicit parents.
        let is_operator_continuation = current.origin() == PromptOrigin::Operator
            && current.parent_prompt_id().is_some()
            && current.active_operator_id() != Some(current.id());
        if current.origin() == PromptOrigin::HarnessRetry || is_operator_continuation {
            let parent_id = current.parent_prompt_id().ok_or_else(|| {
                anyhow::anyhow!(
                    "prompt {} continues a lineage but has no parent",
                    current.id()
                )
            })?;
            retry_authorities.push((current.id(), current.active_operator_id()));
            current = load_prompt_in_conversation_on_conn(
                conn,
                conversation_id,
                parent_id,
                workspace_id,
            )?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "prompt {} references missing parent {parent_id}",
                    current.id()
                )
            })?;
        } else {
            for (retry_id, stored_authority) in retry_authorities {
                if stored_authority.is_some_and(|stored| stored != current.id()) {
                    anyhow::bail!(
                        "prompt {retry_id} active operator disagrees with parent \
                         authority {}",
                        current.id()
                    );
                }
            }
            return Ok((current, depth));
        }
    }

    anyhow::bail!(
        "prompt lineage from {} exceeds the maximum depth of \
         {MAX_PROMPT_LINEAGE_DEPTH} receipts",
        submitted.id()
    )
}

fn latest_prompt_on_conn(
    conn: &Connection,
    conversation_id: &str,
    workspace_id: &str,
) -> anyhow::Result<Option<PromptReceipt>> {
    // `receipt_order` is serialized database presentation metadata, not part
    // of an immutable receipt hash. Never trust its largest value by itself:
    // validate the complete hashed `previous_prompt_id` chain first, then take
    // its verified tip. This makes both reads and the next append fail closed
    // if mutable row order is corrupt instead of silently forking chronology.
    let mut chain = prompt_chain_on_conn(conn, conversation_id, workspace_id)?;
    Ok(chain.pop())
}

fn prompt_chain_on_conn(
    conn: &Connection,
    conversation_id: &str,
    workspace_id: &str,
) -> anyhow::Result<Vec<PromptReceipt>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.conversation_id, p.writer_fingerprint, p.seq,
                p.previous_prompt_id, p.parent_prompt_id, p.root_prompt_id,
                p.active_operator_id, p.origin, p.raw_text, p.model_text,
                p.raw_digest, p.model_digest, p.receipt_hash, p.ts_claim,
                p.encoding_version
           FROM prompt_receipts p
           JOIN conversations c ON c.id = p.conversation_id
          WHERE p.conversation_id = ?1 AND c.workspace_key = ?2
          ORDER BY p.receipt_order ASC",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![conversation_id, workspace_id],
        prompt_row_from_sql,
    )?;
    let receipts: Vec<PromptReceipt> = rows
        .map(|row| {
            row.map_err(anyhow::Error::from)
                .and_then(PromptRow::into_receipt)
        })
        .collect::<anyhow::Result<_>>()?;
    let mut expected_previous = None;
    for receipt in &receipts {
        if receipt.previous_prompt_id() != expected_previous {
            anyhow::bail!(
                "prompt chronology mismatch in conversation `{conversation_id}` at {}: \
                 expected previous {:?}, stored {:?}",
                receipt.id(),
                expected_previous,
                receipt.previous_prompt_id()
            );
        }
        expected_previous = Some(receipt.id());
    }
    Ok(receipts)
}

/// Is `s` a content id — exactly 64 lowercase hex characters (#1786 spec §2)?
///
/// ONE definition, deliberately shared by both sides of the sources contract:
/// [`canonical_sources_json`] (the write path, which refuses a bad reference
/// at append) and [`parse_canonical_sources`] (verification, which refuses
/// bad stored bytes). Two copies of this predicate could drift apart, and a
/// write path that admits what verification rejects writes rows that can
/// never verify again — so the shape has a single owner.
fn is_content_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Parse a stored `sources` column, REFUSING anything but the canonical
/// byte form `canonical_sources_json` produces: compact JSON array of
/// 64-lowercase-hex ids, sorted, deduplicated. The column is hashed, so its
/// bytes have exactly one legitimate rendering — any other shape reached the
/// database without going through the write path.
fn parse_canonical_sources(stored: &str) -> anyhow::Result<Vec<String>> {
    if stored == "[]" {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = serde_json::from_str(stored)
        .map_err(|e| anyhow::anyhow!("not a JSON string array: {e}"))?;
    for id in &ids {
        if !is_content_id(id) {
            anyhow::bail!("`{id}` is not a 64-lowercase-hex content id");
        }
    }
    for pair in ids.windows(2) {
        if pair[0] >= pair[1] {
            anyhow::bail!("ids are not strictly sorted (duplicates included)");
        }
    }
    let canonical = canonical_sources_json(&ids)?;
    if canonical != stored {
        anyhow::bail!("bytes differ from the canonical rendering");
    }
    Ok(ids)
}

/// Validate and canonicalize a sources list into the ONE stored byte form
/// (#1786 spec §3): a compact JSON array of 64-lowercase-hex content ids,
/// sorted lexicographically, duplicates removed. The column is hashed, so
/// its bytes need exactly one producer-deterministic rendering — and because
/// this write path canonicalizes, only out-of-band SQL can ever produce the
/// non-canonical bytes verification refuses.
fn canonical_sources_json(sources: &[String]) -> anyhow::Result<String> {
    if sources.is_empty() {
        return Ok("[]".to_string());
    }
    for id in sources {
        if !is_content_id(id) {
            anyhow::bail!(
                "refusing the append — source reference `{id}` is not a content id \
                 (64 lowercase hex); a citation that cannot resolve must not be recorded"
            );
        }
    }
    let mut ids: Vec<&str> = sources.iter().map(String::as_str).collect();
    ids.sort_unstable();
    ids.dedup();
    // Compact by construction: fixed-alphabet strings need no escaping.
    let mut out = String::with_capacity(2 + ids.len() * 67);
    out.push('[');
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(id);
        out.push('"');
    }
    out.push(']');
    Ok(out)
}

/// #1086 one-shot table migration: rebuild a legacy `roadmaps` table whose
/// primary key is `id` alone into one keyed by `(id, workspace_key)`.
///
/// Necessary because `create_schema` uses `CREATE TABLE IF NOT EXISTS`, so an
/// existing db keeps its old single-column PK — under which `create_roadmap`'s
/// `INSERT OR REPLACE` (conflict target = the PK) could replace a same-id
/// roadmap owned by a *different* workspace. `/roadmap import` makes that
/// trivially reachable (the id travels in the file), silently stealing another
/// workspace's roadmap. The composite key makes the write path workspace-fenced.
///
/// Idempotent (skips once the PK is composite) and lossless (existing ids are
/// globally unique under the old PK, so every row survives the rebuild). Runs
/// inside the open path's locked-retry, so a rebuild racing another first-open
/// is retried, not surfaced.
fn migrate_roadmaps_pk(conn: &Connection) -> anyhow::Result<()> {
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='roadmaps'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        // No table yet (a brand-new db creates it composite in create_schema).
        return Ok(());
    };
    let normalized = sql
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if normalized.contains("primary key (id, workspace_key)") {
        return Ok(()); // already composite — nothing to do
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE roadmaps_v2 (
             id                 TEXT NOT NULL,
             workspace_key      TEXT NOT NULL,
             title              TEXT NOT NULL DEFAULT '',
             tree               TEXT NOT NULL DEFAULT '',
             schema_version     INTEGER NOT NULL DEFAULT 1,
             created_at_claim   INTEGER NOT NULL DEFAULT 0,
             updated_at_claim   INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (id, workspace_key)
         );
         INSERT INTO roadmaps_v2
             (id, workspace_key, title, tree, schema_version, created_at_claim, updated_at_claim)
             SELECT id, workspace_key, title, tree, schema_version, created_at_claim, updated_at_claim
             FROM roadmaps;
         DROP TABLE roadmaps;
         ALTER TABLE roadmaps_v2 RENAME TO roadmaps;
         CREATE INDEX IF NOT EXISTS idx_roadmaps_ws ON roadmaps (workspace_key);",
    )?;
    tx.commit()?;
    tracing::info!("migrated roadmaps to a composite (id, workspace_key) primary key (#1086)");
    Ok(())
}

/// Clamp a legacy u128 nanosecond claim into the store's i64 claim columns.
/// Saturates at `i64::MAX` — claims are display-only (§6), never compared.
fn clamp_claim(nanos: u128) -> i64 {
    i64::try_from(nanos).unwrap_or(i64::MAX)
}

/// The §6 writer fingerprint, in preference order (module docs — Writer
/// identity): the operator's mesh-key fingerprint from `<root>/identity.pem`
/// when it exists and parses, else the 17.1a per-install nonce.
///
/// The key type comes from `agent-mesh-protocol` (already a direct dep of
/// newt-core) — deliberately NOT from `newt-identity`, which depends on
/// newt-core and would make the coupling a cycle. The path is rooted at the
/// store root rather than resolved from `$HOME` so the derivation stays
/// hermetic (tests, alternate roots); for the production root `~/.newt`
/// the two spellings are the same file.
fn resolve_writer_fingerprint(root: &Path) -> anyhow::Result<String> {
    let pem = root.join(IDENTITY_PEM_FILE);
    if pem.is_file() {
        match agent_mesh_protocol::UserKey::load(&pem) {
            Ok(user) => return Ok(user.fingerprint().hex()),
            Err(e) => {
                // A broken key file must never block the store; the nonce
                // fallback keeps the Lamport clock running (and §6 chains
                // tolerate the writer change as a handoff).
                tracing::warn!(
                    path = %pem.display(),
                    error = %e,
                    "identity.pem exists but did not parse; \
                     falling back to the per-install nonce writer fingerprint"
                );
            }
        }
    }
    load_or_create_writer_fingerprint(root)
}

/// Load (or mint, atomically) the per-install nonce and derive the writer
/// fingerprint as its BLAKE3 hex — the fallback half of
/// [`resolve_writer_fingerprint`].
fn load_or_create_writer_fingerprint(root: &Path) -> anyhow::Result<String> {
    let path = root.join(NONCE_FILE);
    let nonce = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
        _ => {
            // Atomic mint-with-content. Two racing first-run processes must
            // converge on ONE nonce, and the published file must NEVER be
            // observable half-written:
            //   * write-then-RENAME is wrong — rename replaces, so a slow
            //     racer can overwrite the winner's nonce after the winner
            //     already derived its fingerprint (orphaning its rows);
            //   * bare O_EXCL-then-write is wrong — the file exists EMPTY
            //     between create and write, so a racing reader can adopt ""
            //     (caught by CI: the 8-thread convergence test on Windows).
            // hard_link is the primitive with both properties: the name
            // appears only after the temp's content is fully written, and
            // linking FAILS (AlreadyExists) instead of replacing a winner.
            let minted = uuid::Uuid::new_v4().to_string();
            let tmp = root.join(format!(
                "{NONCE_FILE}.{}.{:?}.tmp",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&tmp, &minted)?;
            let publish = std::fs::hard_link(&tmp, &path);
            let _ = std::fs::remove_file(&tmp);
            match publish {
                Ok(()) => minted,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // The winner's link only exists with full content.
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

// ── #1030 collision fix: one live owner per conversation (live_owners) ──────

/// The current on-disk schema version of a roadmap's serialized tree (#1030).
/// Bumped only on a forward-incompatible change to the `plan.rs::Plan` shape;
/// `Subtask`'s `deny_unknown_fields` makes such a change loud, not silent.
const ROADMAP_SCHEMA_VERSION: i64 = 1;

/// A #1030 roadmap loaded from the store: the Roadmap→Phase→Plan→Task tree as a
/// [`crate::plan::Plan`], plus its id and title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roadmap {
    pub id: String,
    pub title: String,
    pub tree: crate::plan::Plan,
}

/// A one-line roadmap summary for listings (#1030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoadmapSummary {
    pub id: String,
    pub title: String,
    pub node_count: usize,
}

/// The outcome of [`ConversationStore::inject_prompt`] (A3/W6 attach-inject).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectOutcome {
    /// A new inbox row was enqueued for the running session to consume.
    Enqueued,
    /// An `idem_key` match already existed — no new row (a safe retry / a
    /// double-submit / an SSE reconnect that re-POSTed the same prompt).
    Duplicate,
}

/// One dequeued injected prompt, from [`ConversationStore::take_injected_prompt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectedPrompt {
    /// The inbox row id — pass to [`ConversationStore::link_inbox_delivery`]
    /// once the turn's receipt is minted, to record what it became.
    pub id: String,
    /// The injected prompt text (becomes the turn's input, tagged as
    /// web-injected so it is inert model text — never a host-shell escape).
    pub body: String,
    /// Per-conversation FIFO sequence (delivery order).
    pub seq: i64,
}

/// A verdict an attach surface can NAME for a pending permission decision
/// (A4/W6). It names a CHOICE the running gate then interprets and mints — the
/// web never carries caveats/authority itself. `AllowSession` is session-scoped
/// (ephemeral); there is deliberately NO durable "always-allow" verdict — the
/// web cannot write durable OCAP policy (that is terminal-audit-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Allow this one denied action, this time only.
    AllowOnce,
    /// Allow it for the rest of the session (in-memory; NOT persisted).
    AllowSession,
    /// Refuse — the model sees the standard structured denial.
    Deny,
}

// B0b-2 (#1846): `Verdict`'s DATABASE serialization (`as_db_str` /
// `from_db_str`) went with the `permission_requests.verdict` column it
// existed for. The enum itself stays — `From<Verdict> for PermissionAction`
// is still live — but nothing persists it any more: an answer is stored as
// the winning `Response` body in `interaction_offers`.
//
// The lenient-read guarantee those functions carried was RETARGETED, not
// retired: `an_unknown_persisted_option_reads_as_none_not_error` asserts the
// same property against the new decode path, where an unreadable stored
// answer reads as "no answer" rather than as an error or an authorization.

/// A staged passkey binding awaiting terminal confirmation
/// ([`ConversationStore::pending_enrollment_candidate`]).
///
/// There is no verdict field, and that absence is the design: the web stages,
/// the terminal promotes. Nothing here is authority until a signed row lands in
/// the credential registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEnrollment {
    /// The unguessable nonce binding a confirmation to THIS candidate.
    pub request_id: String,
    /// Serialized [`crate::enrollment::EnrollmentCandidate`].
    pub candidate_json: String,
}

// Result of permission answer attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerOutcome {
    Answered,
    AlreadyResolved,
    InvalidAction,
    Unknown,
}

fn claim_to_u128(claim: i64) -> u128 {
    claim.max(0) as u128
}

/// Read a token-count column back to the `u32` it was written from (17.6).
/// NULL stays `None` — an unreported count is absence, never zero-dressed-up.
/// A value outside `u32` cannot come from `append_turn_full` (which widens
/// from `u32`), so it errors as tampering/corruption instead of clamping —
/// 18.5 trusts these as measurements.
fn tokens_from_sql(value: Option<i64>) -> anyhow::Result<Option<u32>> {
    value
        .map(|v| {
            u32::try_from(v)
                .map_err(|_| anyhow::anyhow!("token count {v} out of range (tampered row?)"))
        })
        .transpose()
}

/// The conversation-id alphabet (ASCII alphanumeric + '-'), inherited from
/// the JSON backend so every legacy id imports unchanged. SQL parameters
/// make injection moot; the validation also guarantees ids are pure ASCII,
/// which `resolve_id`'s byte-exact `substr` prefix match relies on.
fn validate_record_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        anyhow::bail!("invalid conversation id `{id}`");
    }
    Ok(())
}

#[cfg(test)]
#[path = "store_tests/chain.rs"]
mod chain_tests;
#[cfg(test)]
#[path = "store_tests/claims.rs"]
mod claims_tests;
#[cfg(test)]
#[path = "store_tests/inbox.rs"]
mod inbox_tests;
#[cfg(test)]
#[path = "store_tests/liveness.rs"]
mod liveness_tests;
#[cfg(test)]
#[path = "store_tests/misc.rs"]
mod misc_tests;
#[cfg(test)]
#[path = "store_tests/prompts.rs"]
mod prompts_tests;
#[cfg(test)]
#[path = "store_tests/roadmaps.rs"]
mod roadmaps_tests;
#[cfg(test)]
#[path = "store_tests/schema.rs"]
mod schema_tests;
