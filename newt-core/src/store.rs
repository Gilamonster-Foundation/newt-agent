//! SQLite-backed conversation store — Phase 17.1a/17.1b (issue #246).
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
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

mod fts;
use fts::create_fts_index;
#[cfg(test)]
use fts::events_extract_sql;
pub use fts::sanitize_fts5_query;

mod turn_chain;
use turn_chain::{
    genesis_hash, insert_turn_row, last_turn, next_tick, turn_row_from_sql, TurnRow,
    TURN_ENCODING_VERSION_CURRENT,
};

use crate::artifact::{
    ArtifactId, ArtifactKind, ArtifactRelation, NewPromptArtifact, PromptArtifact,
};
use crate::conversation::{
    new_conversation_id, session_plan_dir, ConversationRecord, ConversationSummary,
    ConversationTurn,
};
use crate::prompt::{NewPrompt, PromptId, PromptOrigin, PromptReceipt, TurnPromptContext};

/// Database file name under the store root (`~/.newt/conversations.db`).
const DB_FILE: &str = "conversations.db";

/// The retired JSON backend's tree under the store root: one
/// `<workspace-uuid>/<id>.json` per conversation. Imported once on open.
const LEGACY_JSON_DIR: &str = "conversations";

/// Where the legacy tree is moved after a successful import (kept as a
/// backup, never deleted by newt).
const LEGACY_BACKUP_DIR: &str = "conversations.imported";

/// Per-install nonce file under the store root; its BLAKE3 hex is the
/// `writer_fingerprint` *fallback* when no identity key exists (see
/// module docs — Writer identity).
const NONCE_FILE: &str = "install-nonce";

/// The operator's root identity key under the store root (`~/.newt` in
/// production — the same `~/.newt/identity.pem` newt-identity mints). When
/// present, its fingerprint IS the writer fingerprint.
const IDENTITY_PEM_FILE: &str = "identity.pem";

/// How long a writer waits on a locked database before erroring. Two newts
/// sharing `~/.newt/conversations.db` serialize their write transactions
/// behind this.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of receipts examined while resolving the active operator
/// through a harness-retry parent chain. This includes both the submitted
/// receipt and the terminal operator receipt. A finite bound makes corrupted
/// or adversarial lineage fail closed without unbounded CPU/memory use; 256
/// still permits far more consecutive automatic retries than a useful turn
/// should ever require.
const MAX_PROMPT_LINEAGE_DEPTH: usize = 256;

/// Domain-separation prefix for the per-conversation prompt-artifact chain.
const ARTIFACT_GENESIS_PREFIX: &[u8] = b"newt-prompt-artifact-chain-genesis:v1";

/// Hard upper bound for one paged artifact read. Internal verification still
/// covers the complete chain before a page is returned.
const MAX_ARTIFACT_PAGE_SIZE: usize = 256;

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
        let active_operator = match prompt.origin {
            PromptOrigin::Operator => None,
            PromptOrigin::HarnessRetry => {
                let parent = parent.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("a harness retry must name an operator-prompt parent")
                })?;
                let (active, parent_depth) = resolve_active_operator_on_conn(
                    &tx,
                    conversation_id,
                    parent,
                    &self.workspace_id,
                )?;
                if parent_depth >= MAX_PROMPT_LINEAGE_DEPTH {
                    anyhow::bail!(
                        "harness retry would exceed the maximum prompt lineage depth of \
                         {MAX_PROMPT_LINEAGE_DEPTH} receipts"
                    );
                }
                Some(active)
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

    /// Atomically append one bounded derived-work artifact to a prompt.
    ///
    /// The objective root is derived from the immutable prompt receipt, never
    /// accepted from the caller. The complete existing artifact chain is
    /// verified under the same `BEGIN IMMEDIATE` transaction that allocates
    /// the next sequence and inserts the row, so concurrent processes cannot
    /// fork the chain and pre-existing tampering fails closed.
    pub fn append_prompt_artifact(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
        content: NewPromptArtifact,
    ) -> anyhow::Result<PromptArtifact> {
        validate_record_id(conversation_id)?;
        // Validate bounded content before taking SQLite's write lock.
        content.validate()?;
        let now = (self.claim_clock)();
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
            Some(owner) if owner == self.workspace_id => {}
            Some(owner) => anyhow::bail!(
                "conversation id `{conversation_id}` belongs to another workspace \
                 (key {owner}); refusing to append an artifact across the workspace fence"
            ),
            None => anyhow::bail!("unknown conversation `{conversation_id}`"),
        }

        // Artifact provenance is meaningful only while the complete prompt
        // chronology remains valid. In particular, fail closed if mutable
        // receipt_order metadata was tampered even though this one receipt's
        // content hash still verifies.
        let prompt_chain = prompt_chain_on_conn(&tx, conversation_id, &self.workspace_id)?;
        let prompt = prompt_chain
            .iter()
            .find(|prompt| prompt.id() == prompt_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                "prompt {prompt_id} is not in conversation `{conversation_id}` in this workspace"
            )
            })?;
        validate_objective_root_on_conn(
            &tx,
            conversation_id,
            prompt.root_prompt_id(),
            &self.workspace_id,
        )?;

        let chain = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?;
        let (seq, prev_hash) = match chain.last() {
            Some(previous) => (
                previous
                    .seq()
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("prompt artifact sequence overflow"))?,
                previous.artifact_hash().to_string(),
            ),
            None => (1, artifact_genesis_hash(conversation_id)),
        };
        let artifact = PromptArtifact::mint(
            ArtifactId::new(),
            conversation_id.to_string(),
            self.writer_fingerprint.clone(),
            seq,
            prev_hash,
            prompt.id(),
            prompt.root_prompt_id(),
            content,
            now,
        )?;
        insert_prompt_artifact(&tx, &artifact)?;
        tx.commit()?;
        Ok(artifact)
    }

    /// Fetch one artifact through both its conversation and workspace fence.
    /// The complete conversation chain is verified before the record is
    /// returned, so deletion/reordering of a predecessor is detected too.
    pub fn load_prompt_artifact(
        &self,
        conversation_id: &str,
        artifact_id: ArtifactId,
    ) -> anyhow::Result<Option<PromptArtifact>> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let found = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?
            .into_iter()
            .find(|artifact| artifact.id() == artifact_id);
        tx.commit()?;
        Ok(found)
    }

    /// Read a bounded page of the verified artifact chain in causal order.
    pub fn list_prompt_artifacts(
        &self,
        conversation_id: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<Vec<PromptArtifact>> {
        self.page_prompt_artifacts(conversation_id, offset, limit)
            .map(|(artifacts, _)| artifacts)
    }

    /// Return a verified causal page and its snapshot-consistent total.
    pub fn page_prompt_artifacts(
        &self,
        conversation_id: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<PromptArtifact>, usize)> {
        validate_record_id(conversation_id)?;
        validate_artifact_page_size(limit)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let chain = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?;
        let total = chain.len();
        let page = chain.into_iter().skip(offset).take(limit).collect();
        tx.commit()?;
        Ok((page, total))
    }

    /// Count the verified artifact chain through the workspace fence.
    pub fn count_prompt_artifacts(&self, conversation_id: &str) -> anyhow::Result<usize> {
        self.page_prompt_artifacts(conversation_id, 0, 0)
            .map(|(_, total)| total)
    }

    /// Read a bounded causal page containing only artifacts attached directly
    /// to `prompt_id`. A prompt outside this conversation/workspace resolves
    /// to an empty page rather than becoming an address-existence oracle.
    pub fn list_prompt_artifacts_for_prompt(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<Vec<PromptArtifact>> {
        self.page_prompt_artifacts_for_prompt(conversation_id, prompt_id, offset, limit)
            .map(|(artifacts, _)| artifacts)
    }

    /// Return a direct-prompt page and its total from one read transaction.
    pub fn page_prompt_artifacts_for_prompt(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<PromptArtifact>, usize)> {
        validate_record_id(conversation_id)?;
        validate_artifact_page_size(limit)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        if load_prompt_in_conversation_on_conn(&tx, conversation_id, prompt_id, &self.workspace_id)?
            .is_none()
        {
            tx.commit()?;
            return Ok((Vec::new(), 0));
        }
        let matching: Vec<_> = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?
            .into_iter()
            .filter(|artifact| artifact.prompt_id() == prompt_id)
            .collect();
        let total = matching.len();
        let page = matching.into_iter().skip(offset).take(limit).collect();
        tx.commit()?;
        Ok((page, total))
    }

    /// Count verified artifacts attached directly to one prompt.
    pub fn count_prompt_artifacts_for_prompt(
        &self,
        conversation_id: &str,
        prompt_id: PromptId,
    ) -> anyhow::Result<usize> {
        self.page_prompt_artifacts_for_prompt(conversation_id, prompt_id, 0, 0)
            .map(|(_, total)| total)
    }

    /// Read a bounded causal page for an entire objective root, including
    /// artifacts attached to later clarifications and harness retries.
    pub fn list_prompt_artifacts_for_root(
        &self,
        conversation_id: &str,
        root_prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<Vec<PromptArtifact>> {
        self.page_prompt_artifacts_for_root(conversation_id, root_prompt_id, offset, limit)
            .map(|(artifacts, _)| artifacts)
    }

    /// Return an objective-root page and its total from one read transaction.
    pub fn page_prompt_artifacts_for_root(
        &self,
        conversation_id: &str,
        root_prompt_id: PromptId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<PromptArtifact>, usize)> {
        validate_record_id(conversation_id)?;
        validate_artifact_page_size(limit)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        let Some(root) = load_prompt_in_conversation_on_conn(
            &tx,
            conversation_id,
            root_prompt_id,
            &self.workspace_id,
        )?
        else {
            tx.commit()?;
            return Ok((Vec::new(), 0));
        };
        if root.id() != root.root_prompt_id() {
            anyhow::bail!("prompt {root_prompt_id} is not an objective root");
        }
        validate_objective_root_on_conn(&tx, conversation_id, root_prompt_id, &self.workspace_id)?;
        let matching: Vec<_> = artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?
            .into_iter()
            .filter(|artifact| artifact.root_prompt_id() == root_prompt_id)
            .collect();
        let total = matching.len();
        let page = matching.into_iter().skip(offset).take(limit).collect();
        tx.commit()?;
        Ok((page, total))
    }

    /// Count verified artifacts in one objective-root lineage.
    pub fn count_prompt_artifacts_for_root(
        &self,
        conversation_id: &str,
        root_prompt_id: PromptId,
    ) -> anyhow::Result<usize> {
        self.page_prompt_artifacts_for_root(conversation_id, root_prompt_id, 0, 0)
            .map(|(_, total)| total)
    }

    /// Verify every artifact hash, causal sequence, and predecessor link in a
    /// conversation. Unknown and cross-workspace conversations expose an empty
    /// valid chain, matching the fenced read APIs.
    pub fn verify_prompt_artifact_chain(&self, conversation_id: &str) -> anyhow::Result<()> {
        validate_record_id(conversation_id)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Deferred)?;
        artifact_chain_on_conn(&tx, conversation_id, &self.workspace_id)?;
        tx.commit()?;
        Ok(())
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
        self.append_turn_full(id, user, assistant, &[], &[], None, None)
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
    /// **Phantom reaches (#717):** the per-turn alias-seam telemetry persists
    /// alongside `events` in its own `phantom_reaches` column. It is deliberately
    /// NOT part of the §6 canonical encoding (telemetry, not provenance), so an
    /// older db gains the column on open and existing content chains verify
    /// byte-for-byte unchanged. Folding it into the hash would require a v2
    /// encoding bump — a deliberate follow-up, not this additive change.
    #[allow(clippy::too_many_arguments)]
    pub fn append_turn_full(
        &self,
        id: &str,
        user: &str,
        assistant: &str,
        events: &[crate::ToolEvent],
        phantom_reaches: &[crate::PhantomReach],
        tokens_in: Option<u32>,
        tokens_out: Option<u32>,
    ) -> anyhow::Result<()> {
        let id = self.resolve_id(id)?;
        let now = (self.claim_clock)();
        let events_json = serde_json::to_string(events)?;
        let phantom_reaches_json = serde_json::to_string(phantom_reaches)?;
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let tick = next_tick(&tx, &self.writer_fingerprint)?;

        // The §6 content chain: hash the canonical encoding of this writer's
        // previous turn (re-derived from the row itself, so a drifted
        // `tip_hash` column can never poison the chain).
        let prev_hash = match last_turn(&tx, &id, &self.writer_fingerprint)? {
            Some(prev) => prev.content_hash()?,
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
            tokens_in: tokens_in.map(i64::from),
            tokens_out: tokens_out.map(i64::from),
            ts_claim: now,
            encoding_version: TURN_ENCODING_VERSION_CURRENT,
        };
        insert_turn_row(&tx, &row, &phantom_reaches_json)?;
        // Activity tick + chain tip move together; updated_at_claim is a
        // display claim only (§6) — nothing orders by it.
        tx.execute(
            "UPDATE conversations
                SET writer_fingerprint = ?2, activity_tick = ?3, tip_hash = ?4,
                    updated_at_claim = ?5
              WHERE id = ?1",
            rusqlite::params![id, self.writer_fingerprint, tick, row.content_hash()?, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Load a full record (turns in causal `(writer, seq)` order). `id` may
    /// be a unique prefix.
    pub fn load(&self, id: &str) -> anyhow::Result<ConversationRecord> {
        let id = self.resolve_id(id)?;
        let conn = self.lock_conn();
        let (mut record, scratchpad_json, plan_json) = conn
            .query_row(
                "SELECT id, title, workspace_path, workspace_key, persona,
                        started_at_claim, updated_at_claim, scratchpad, plan,
                        roadmap_id, node_id
                   FROM conversations
                  WHERE id = ?1 AND workspace_key = ?2",
                rusqlite::params![id, self.workspace_id],
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

    // ---- A4 (W6) permission-decision channel -------------------------------
    // The RUNNING gate (the sole authority minter) PUBLISHES a pending decision;
    // an ATTACH surface RENDERS it and ANSWERS a VERDICT (never caveats); the
    // gate RACES that answer against the TTY and RESOLVES it exactly-once. Same
    // one-BEGIN-IMMEDIATE, workspace-fenced discipline as the A3 inbox.

    /// How long an unanswered permission-decision nonce stays consumable before
    /// the store treats it as gone (#1356). The gate's own deadline is the
    /// primary resolver; this is the store-layer backstop that bounds the window
    /// in which a published-but-never-answered `request_id` can still be
    /// surfaced or answered. 5 minutes — comfortably longer than an interactive
    /// decision, short enough that an abandoned request does not linger.
    pub(crate) const PERMISSION_REQUEST_TTL_NANOS: i64 = 5 * 60 * 1_000_000_000;

    /// Publish a pending permission decision for the attach surface to render.
    /// `requests_json` is a serialized `&[PermissionRequest]`; `danger_json` is
    /// the GATE-STAMPED per-target tier (the web never classifies danger).
    /// Returns an unguessable `request_id` the gate polls and the web echoes.
    /// Workspace-fenced like [`begin_prompt`]; the caller must own the claim.
    pub fn publish_permission_request(
        &self,
        conversation_id: &str,
        requests_json: &str,
        danger_json: &str,
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
                "cannot publish a permission request for unknown conversation `{conversation_id}`"
            ),
            Some(key) if key != self.workspace_id => {
                anyhow::bail!("conversation `{conversation_id}` belongs to another workspace")
            }
            _ => {}
        }
        tx.execute(
            "INSERT INTO permission_requests
               (request_id, conversation_id, workspace_key, requests_json, danger_json, resolved, created_tick)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            rusqlite::params![
                request_id,
                conversation_id,
                self.workspace_id,
                requests_json,
                danger_json,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(request_id)
    }

    /// The unresolved pending decision for `conversation_id`, if any — what the
    /// attach surface renders (request_id + the serialized requests + the
    /// gate-stamped danger). Workspace-fenced, non-blocking read.
    pub fn pending_permission_request(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Option<PendingPermission>> {
        let conn = self.lock_conn();
        let cutoff = (self.claim_clock)().saturating_sub(Self::PERMISSION_REQUEST_TTL_NANOS);
        conn.query_row(
            "SELECT request_id, requests_json, danger_json FROM permission_requests
              WHERE conversation_id = ?1 AND workspace_key = ?2 AND resolved = 0 AND verdict IS NULL
                AND created_tick > ?3
              ORDER BY created_tick ASC LIMIT 1",
            rusqlite::params![conversation_id, self.workspace_id, cutoff],
            |row| {
                Ok(PendingPermission {
                    request_id: row.get(0)?,
                    requests_json: row.get(1)?,
                    danger_json: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Record a surface's VERDICT for a pending request (the web calls this).
    /// Idempotent on `request_id`: a double-submit / SSE reconnect that names
    /// the same still-unresolved request is a safe no-op. Workspace-fenced.
    /// This does NOT resolve the request — the gate's poll consumes it (so a
    /// TTY answer can still win the race first).
    pub fn answer_permission_request(
        &self,
        conversation_id: &str,
        request_id: &str,
        verdict: Verdict,
    ) -> anyhow::Result<AnswerOutcome> {
        let cutoff = (self.claim_clock)().saturating_sub(Self::PERMISSION_REQUEST_TTL_NANOS);
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let state: Option<(i64, Option<String>, i64)> = tx
            .query_row(
                "SELECT resolved, verdict, created_tick FROM permission_requests
                  WHERE request_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3",
                rusqlite::params![request_id, conversation_id, self.workspace_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let outcome = match state {
            None => AnswerOutcome::Unknown,
            // Expired nonce (#1356): an aged-out request is gone — never newly
            // answerable, even while still unresolved.
            Some((_, _, created_tick)) if created_tick <= cutoff => AnswerOutcome::Unknown,
            Some((1, _, _)) | Some((_, Some(_), _)) => AnswerOutcome::AlreadyResolved,
            Some((_, None, _)) => {
                tx.execute(
                    "UPDATE permission_requests SET verdict = ?2, answered_by = 'web'
                      WHERE request_id = ?1 AND resolved = 0 AND verdict IS NULL",
                    rusqlite::params![request_id, verdict.as_db_str()],
                )?;
                AnswerOutcome::Answered
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// The gate's poll: if a surface has answered `request_id` and it is still
    /// unresolved, RESOLVE it (exactly-once) and return the verdict. Returns
    /// `Ok(None)` instantly when no answer is waiting — a bounded, non-blocking
    /// poll the gate calls each tick while also watching the TTY.
    pub fn take_permission_decision(
        &self,
        conversation_id: &str,
        request_id: &str,
    ) -> anyhow::Result<Option<Verdict>> {
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let verdict: Option<String> = tx
            .query_row(
                "SELECT verdict FROM permission_requests
                  WHERE request_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3
                    AND resolved = 0 AND verdict IS NOT NULL",
                rusqlite::params![request_id, conversation_id, self.workspace_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(v) = verdict else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE permission_requests SET resolved = 1 WHERE request_id = ?1",
            [request_id],
        )?;
        tx.commit()?;
        Ok(Verdict::from_db_str(&v))
    }

    /// The gate RESOLVES a request itself (the TTY answered, or the deadline
    /// fired) — a CAS that flips `resolved` 0→1 only if still unresolved.
    /// Returns `true` if THIS call won (the TTY/timeout beat any web answer);
    /// `false` means a web answer already resolved it (its verdict stands, and
    /// the caller must discard the TTY/timeout choice). `by` is 'tty'|'expired'.
    pub fn resolve_permission_request(
        &self,
        conversation_id: &str,
        request_id: &str,
        by: &str,
    ) -> anyhow::Result<bool> {
        let conn = self.lock_conn();
        let changed = conn.execute(
            "UPDATE permission_requests SET resolved = 1, answered_by = ?4
              WHERE request_id = ?1 AND conversation_id = ?2 AND workspace_key = ?3
                AND resolved = 0 AND verdict IS NULL",
            rusqlite::params![request_id, conversation_id, self.workspace_id, by],
        )?;
        Ok(changed == 1)
    }

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

    /// #1030 collision fix: attempt to become the SINGLE live owner of `id`.
    /// Atomic (`BEGIN IMMEDIATE`): if the conversation is unclaimed, or its
    /// claim is our own, or its claim is STALE (the owner is not live — a
    /// crashed or rebooted process), this process takes ownership and returns
    /// [`Claimed`](ClaimOutcome::Claimed). If a DIFFERENT, LIVE process owns it,
    /// returns [`HeldBy`](ClaimOutcome::HeldBy) and writes nothing — the caller
    /// must not attach (attaching is exactly the turn-interleaving bug #1030
    /// fixes). Identity is `host`+`boot_id`+`pid`, never the (machine-shared)
    /// writer fingerprint.
    pub fn claim(&self, id: &str) -> anyhow::Result<ClaimOutcome> {
        // NOT `resolve_id`: a session claims its freshly-minted id at startup,
        // BEFORE the conversation row is lazily created on the first turn.
        // `live_owners` is keyed by the (globally-unique) conversation id with
        // no FK, so the exact id is all that is needed.
        validate_record_id(id)?;
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)?;
        let existing = live_owner_row(&tx, id)?;
        if let Some(owner) = &existing {
            let is_ours =
                owner.host == self.host && owner.boot_id == self.boot_id && owner.pid == self.pid;
            if !is_ours && (self.liveness)(owner, now) {
                return Ok(ClaimOutcome::HeldBy {
                    host: owner.host.clone(),
                    pid: owner.pid,
                });
            }
            // Ours, or a stale remnant of a dead session → fall through and take it.
        }
        tx.execute(
            "INSERT OR REPLACE INTO live_owners
               (conversation_id, host, boot_id, pid, writer_fingerprint, heartbeat_tick)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                self.host,
                self.boot_id,
                self.pid,
                self.writer_fingerprint,
                now
            ],
        )?;
        tx.commit()?;
        Ok(ClaimOutcome::Claimed)
    }

    /// Release THIS process's claim on `id` (best-effort). Only deletes a claim
    /// this exact process holds (`host`+`boot_id`+`pid`), so it can never free
    /// another live session's conversation. Called on clean exit / conversation
    /// switch; a crash simply leaves a stale claim the next [`claim`](Self::claim)
    /// reclaims. A missing / foreign id is a silent no-op.
    pub fn release(&self, id: &str) -> anyhow::Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            "DELETE FROM live_owners
              WHERE conversation_id = ?1 AND host = ?2 AND boot_id = ?3 AND pid = ?4",
            rusqlite::params![id, self.host, self.boot_id, self.pid],
        )?;
        Ok(())
    }

    /// Refresh THIS process's heartbeat on `id` — the freshness signal a
    /// cross-host / post-reboot liveness check reads. Cheap; meant to piggyback
    /// the per-turn save. No-op if this process does not hold the claim.
    pub fn heartbeat(&self, id: &str) -> anyhow::Result<()> {
        let now = (self.claim_clock)();
        let conn = self.lock_conn();
        conn.execute(
            "UPDATE live_owners SET heartbeat_tick = ?2
              WHERE conversation_id = ?1 AND host = ?3 AND boot_id = ?4 AND pid = ?5",
            rusqlite::params![id, now, self.host, self.boot_id, self.pid],
        )?;
        Ok(())
    }

    /// The raw `live_owners` row for `id`, WITHOUT a liveness judgement — `None`
    /// when unclaimed. `/resume` pairs this with [`is_owner_live`](Self::is_owner_live)
    /// to render each conversation's ● live / ○ open marker.
    pub fn live_owner(&self, id: &str) -> anyhow::Result<Option<StoredOwner>> {
        let conn = self.lock_conn();
        live_owner_row(&conn, id)
    }

    /// Whether `owner` is a running process right now, per the store's (injected)
    /// liveness oracle — the SAME judgement [`claim`](Self::claim) uses, exposed
    /// so `/resume` renders a consistent marker.
    #[must_use]
    pub fn is_owner_live(&self, owner: &StoredOwner) -> bool {
        (self.liveness)(owner, (self.claim_clock)())
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
                    events, tokens_in, tokens_out, ts_claim, encoding_version
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
                if row.prev_hash != p.content_hash()? {
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
        let expected_tip = match rows.iter().rfind(|r| r.writer_fingerprint == tip_writer) {
            Some(row) => row.content_hash()?,
            None => genesis_hash(&id, &tip_writer),
        };
        if tip != expected_tip {
            anyhow::bail!("chain violation in `{id}`: stored tip_hash does not match the chain");
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

    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
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

        match current.origin() {
            PromptOrigin::Operator => {
                if current
                    .active_operator_id()
                    .is_some_and(|active| active != current.id())
                {
                    anyhow::bail!(
                        "operator prompt {} names a different active authority",
                        current.id()
                    );
                }
                for (retry_id, stored_authority) in retry_authorities {
                    if stored_authority.is_some_and(|stored| stored != current.id()) {
                        anyhow::bail!(
                            "harness retry {retry_id} active operator disagrees with parent \
                             authority {}",
                            current.id()
                        );
                    }
                }
                return Ok((current, depth));
            }
            PromptOrigin::HarnessRetry => {
                let parent_id = current.parent_prompt_id().ok_or_else(|| {
                    anyhow::anyhow!("harness retry {} has no parent", current.id())
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
                        "harness retry {} references missing parent {parent_id}",
                        current.id()
                    )
                })?;
            }
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

/// SQLite representation of one immutable prompt artifact. Rich parsing and
/// hash verification happen outside rusqlite's row callback.
#[derive(Debug)]
struct ArtifactRow {
    id: String,
    conversation_id: String,
    writer_fingerprint: String,
    seq: i64,
    prev_hash: String,
    prompt_id: String,
    root_prompt_id: String,
    kind: String,
    relation: String,
    locator: Option<String>,
    body: Option<String>,
    metadata_json: String,
    ts_claim: i64,
    encoding_version: i64,
    artifact_hash: String,
}

impl ArtifactRow {
    fn into_artifact(self) -> anyhow::Result<PromptArtifact> {
        let artifact = PromptArtifact::from_stored_parts(
            self.id.parse()?,
            self.conversation_id,
            self.writer_fingerprint,
            self.seq,
            self.prev_hash,
            self.prompt_id.parse()?,
            self.root_prompt_id.parse()?,
            ArtifactKind::from_db_str(&self.kind)?,
            ArtifactRelation::from_db_str(&self.relation)?,
            self.locator,
            self.body,
            self.metadata_json,
            self.ts_claim,
            self.encoding_version,
            self.artifact_hash,
        )?;
        artifact.verify_integrity()?;
        Ok(artifact)
    }
}

fn artifact_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        writer_fingerprint: row.get(2)?,
        seq: row.get(3)?,
        prev_hash: row.get(4)?,
        prompt_id: row.get(5)?,
        root_prompt_id: row.get(6)?,
        kind: row.get(7)?,
        relation: row.get(8)?,
        locator: row.get(9)?,
        body: row.get(10)?,
        metadata_json: row.get(11)?,
        ts_claim: row.get(12)?,
        encoding_version: row.get(13)?,
        artifact_hash: row.get(14)?,
    })
}

fn insert_prompt_artifact(conn: &Connection, artifact: &PromptArtifact) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO prompt_artifacts
           (id, conversation_id, writer_fingerprint, seq, prev_hash,
            prompt_id, root_prompt_id, kind, relation, locator, body, metadata,
            ts_claim, encoding_version, artifact_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![
            artifact.id().to_string(),
            artifact.conversation_id(),
            artifact.writer_fingerprint(),
            artifact.seq(),
            artifact.prev_hash(),
            artifact.prompt_id().to_string(),
            artifact.root_prompt_id().to_string(),
            artifact.kind().as_db_str(),
            artifact.relation().as_db_str(),
            artifact.locator(),
            artifact.body(),
            serde_json::to_string(artifact.metadata())?,
            artifact.ts_claim(),
            artifact.encoding_version(),
            artifact.artifact_hash(),
        ],
    )?;
    conn.execute(
        "INSERT INTO prompt_artifact_tips (conversation_id, last_seq, tip_hash)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(conversation_id) DO UPDATE SET
             last_seq = excluded.last_seq,
             tip_hash = excluded.tip_hash",
        rusqlite::params![
            artifact.conversation_id(),
            artifact.seq(),
            artifact.artifact_hash(),
        ],
    )?;
    Ok(())
}

fn artifact_chain_on_conn(
    conn: &Connection,
    conversation_id: &str,
    workspace_id: &str,
) -> anyhow::Result<Vec<PromptArtifact>> {
    // Never return or extend a derived-work chain whose prompt authority is
    // itself corrupt. Prompt receipt_order is intentionally outside each
    // receipt content hash, so the full chronology validator is required here.
    let _ = prompt_chain_on_conn(conn, conversation_id, workspace_id)?;
    let mut stmt = conn.prepare(
        "SELECT a.id, a.conversation_id, a.writer_fingerprint, a.seq,
                a.prev_hash, a.prompt_id, a.root_prompt_id, a.kind, a.relation,
                a.locator, a.body, a.metadata, a.ts_claim, a.encoding_version,
                a.artifact_hash
           FROM prompt_artifacts a
           JOIN conversations c ON c.id = a.conversation_id
          WHERE a.conversation_id = ?1 AND c.workspace_key = ?2
          ORDER BY a.seq ASC",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![conversation_id, workspace_id],
        artifact_row_from_sql,
    )?;
    let artifacts: Vec<PromptArtifact> = rows
        .map(|row| {
            row.map_err(anyhow::Error::from)
                .and_then(ArtifactRow::into_artifact)
        })
        .collect::<anyhow::Result<_>>()?;

    let mut expected_seq = 1_i64;
    let mut expected_prev = artifact_genesis_hash(conversation_id);
    for artifact in &artifacts {
        let prompt = load_prompt_in_conversation_on_conn(
            conn,
            conversation_id,
            artifact.prompt_id(),
            workspace_id,
        )?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "prompt artifact {} is orphaned from prompt {}",
                artifact.id(),
                artifact.prompt_id()
            )
        })?;
        if prompt.root_prompt_id() != artifact.root_prompt_id() {
            anyhow::bail!(
                "prompt artifact {} claims objective root {}, but prompt {} is rooted at {}",
                artifact.id(),
                artifact.root_prompt_id(),
                prompt.id(),
                prompt.root_prompt_id()
            );
        }
        validate_objective_root_on_conn(
            conn,
            conversation_id,
            artifact.root_prompt_id(),
            workspace_id,
        )?;
        if artifact.seq() != expected_seq {
            anyhow::bail!(
                "prompt artifact sequence mismatch in conversation `{conversation_id}` at {}: \
                 expected {expected_seq}, stored {}",
                artifact.id(),
                artifact.seq()
            );
        }
        if artifact.prev_hash() != expected_prev {
            anyhow::bail!(
                "prompt artifact predecessor mismatch in conversation `{conversation_id}` at {}",
                artifact.id()
            );
        }
        expected_seq = expected_seq
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("prompt artifact sequence overflow"))?;
        expected_prev = artifact.artifact_hash().to_string();
    }

    let stored_tip: Option<(i64, String)> = conn
        .query_row(
            "SELECT t.last_seq, t.tip_hash
               FROM prompt_artifact_tips t
               JOIN conversations c ON c.id = t.conversation_id
              WHERE t.conversation_id = ?1 AND c.workspace_key = ?2",
            rusqlite::params![conversation_id, workspace_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match (artifacts.last(), stored_tip) {
        (None, None) => {}
        (Some(last), Some((last_seq, tip_hash)))
            if last.seq() == last_seq && last.artifact_hash() == tip_hash => {}
        (None, Some(_)) => anyhow::bail!(
            "prompt artifact chain tip exists without rows in conversation `{conversation_id}`"
        ),
        (Some(_), None) => anyhow::bail!(
            "prompt artifact rows exist without a chain tip in conversation `{conversation_id}`"
        ),
        (Some(last), Some((last_seq, tip_hash))) => anyhow::bail!(
            "prompt artifact chain tip mismatch in conversation `{conversation_id}`: \
             row tip is ({}, {}), stored tip is ({last_seq}, {tip_hash})",
            last.seq(),
            last.artifact_hash()
        ),
    }
    Ok(artifacts)
}

fn artifact_genesis_hash(conversation_id: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ARTIFACT_GENESIS_PREFIX);
    hasher.update(&(conversation_id.len() as u64).to_le_bytes());
    hasher.update(conversation_id.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn validate_artifact_page_size(limit: usize) -> anyhow::Result<()> {
    if limit > MAX_ARTIFACT_PAGE_SIZE {
        anyhow::bail!("prompt artifact page limit exceeds {MAX_ARTIFACT_PAGE_SIZE}");
    }
    Ok(())
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
             workspace_key      TEXT NOT NULL,            -- scoping key: workspace_key_v2 (17.2 — blake3 remote+branch, path fallback)
             persona            TEXT,
             end_reason         TEXT,                     -- set by 17.7
             writer_fingerprint TEXT NOT NULL,            -- §6 ordering key, half 1
             activity_tick      INTEGER NOT NULL,         -- §6 ordering key, half 2 (per-writer Lamport tick)
             tip_hash           TEXT NOT NULL,            -- §6 chain tip (BLAKE3)
             started_at_claim   INTEGER NOT NULL,         -- DISPLAY ONLY (wall-clock claim, unix nanos)
             updated_at_claim   INTEGER NOT NULL,         -- DISPLAY ONLY
             scratchpad         TEXT NOT NULL DEFAULT '{}', -- JSON scratchpad <state> snapshot (#713); working memory, NOT hashed (§6 chain unchanged)
             plan               TEXT NOT NULL DEFAULT '{}', -- JSON plan-ledger snapshot (#715); working memory, NOT hashed (§6 chain unchanged)
             roadmap_id         TEXT,                      -- #1030: roadmap this conv's Plan node belongs to (NULL = ad-hoc chat); thin pointer, tree lives in `roadmaps`
             node_id            TEXT                       -- #1030: the `roadmaps` tree Subtask id this conversation realizes (NULL = ad-hoc chat)
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
             encoding_version   INTEGER NOT NULL DEFAULT 1, -- canonical-encoding dispatch (N1 on #261)
             phantom_reaches    TEXT NOT NULL DEFAULT '[]', -- JSON phantom-reach telemetry (#717); NOT hashed (§6 chain unchanged)
             PRIMARY KEY (conversation_id, writer_fingerprint, seq)
         );
         -- Immutable prompt receipts are written before inference/tool work.
         -- They are deliberately separate from `turns`: a failed turn still
         -- has a prompt, while the existing turn-chain writer/tip pair remains
         -- byte-for-byte compatible with all prior databases.
         CREATE TABLE IF NOT EXISTS prompt_receipts (
             receipt_order      INTEGER PRIMARY KEY AUTOINCREMENT, -- serialized receipt chronology
             id                 TEXT NOT NULL UNIQUE,              -- prompt:<uuid>
             conversation_id    TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
             writer_fingerprint TEXT NOT NULL,
             seq                INTEGER NOT NULL,                  -- writer Lamport tick
             previous_prompt_id TEXT REFERENCES prompt_receipts(id), -- automatic chronology
             parent_prompt_id   TEXT REFERENCES prompt_receipts(id), -- explicit semantic ancestry
             root_prompt_id     TEXT NOT NULL REFERENCES prompt_receipts(id),
             active_operator_id TEXT REFERENCES prompt_receipts(id), -- nearest operator authority; v1 rows are NULL
             origin             TEXT NOT NULL CHECK (origin IN ('operator', 'harness_retry')),
             raw_text           BLOB NOT NULL,
             model_text         BLOB NOT NULL,
             raw_digest         TEXT NOT NULL,
             model_digest       TEXT NOT NULL,
             receipt_hash       TEXT NOT NULL,
             ts_claim           INTEGER NOT NULL,                  -- display-only wall clock
             encoding_version   INTEGER NOT NULL DEFAULT 1,
             UNIQUE (conversation_id, writer_fingerprint, seq)
         );
         -- The per-writer Lamport clock (§6 'each agent is its own clock').
         CREATE TABLE IF NOT EXISTS writer_clock (
             writer_fingerprint TEXT PRIMARY KEY,
             last_tick          INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_conversations_ws_tick
             ON conversations (workspace_key, activity_tick);
         CREATE INDEX IF NOT EXISTS idx_prompt_receipts_conversation_order
             ON prompt_receipts (conversation_id, receipt_order);
         CREATE INDEX IF NOT EXISTS idx_prompt_receipts_root
             ON prompt_receipts (root_prompt_id);
         -- Composite uniqueness lets artifact foreign keys enforce that both
         -- the direct prompt and objective root belong to the artifact's own
         -- conversation, not merely that those globally-unique ids exist.
         CREATE UNIQUE INDEX IF NOT EXISTS idx_prompt_receipts_conversation_id
             ON prompt_receipts (conversation_id, id);
         -- Bounded derived-work lineage. This is not a duplicate transcript:
         -- bodies and structured metadata have hard byte ceilings, while file
         -- and commit records retain locators/digests rather than raw output.
         CREATE TABLE IF NOT EXISTS prompt_artifacts (
             id                 TEXT PRIMARY KEY,                 -- artifact:<uuid>
             conversation_id    TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
             writer_fingerprint TEXT NOT NULL,
             seq                INTEGER NOT NULL CHECK (seq > 0), -- per-conversation causal order
             prev_hash          TEXT NOT NULL,
             prompt_id          TEXT NOT NULL,
             root_prompt_id     TEXT NOT NULL,
             kind               TEXT NOT NULL CHECK (kind IN (
                                    'plan_revision', 'compaction_checkpoint',
                                    'file_change', 'turn_outcome', 'commit', 'decision')),
             relation           TEXT NOT NULL CHECK (relation IN (
                                    'derived_from', 'updates', 'summarizes', 'realizes')),
             locator            TEXT CHECK (locator IS NULL OR length(CAST(locator AS BLOB)) <= 4096),
             body               TEXT CHECK (body IS NULL OR length(CAST(body AS BLOB)) <= 65536),
             metadata           TEXT NOT NULL CHECK (
                                    json_valid(metadata)
                                    AND json_type(metadata) = 'object'
                                    AND length(CAST(metadata AS BLOB)) <= 16384),
             ts_claim           INTEGER NOT NULL,                 -- display-only wall clock
             encoding_version   INTEGER NOT NULL DEFAULT 1,
             artifact_hash      TEXT NOT NULL,
             UNIQUE (conversation_id, seq),
             FOREIGN KEY (conversation_id, prompt_id)
                 REFERENCES prompt_receipts(conversation_id, id) ON DELETE CASCADE,
             FOREIGN KEY (conversation_id, root_prompt_id)
                 REFERENCES prompt_receipts(conversation_id, id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_prompt_artifacts_prompt
             ON prompt_artifacts (conversation_id, prompt_id, seq);
         CREATE INDEX IF NOT EXISTS idx_prompt_artifacts_root
             ON prompt_artifacts (conversation_id, root_prompt_id, seq);
         -- A separately stored tip detects truncation of the final row. Like
         -- the existing conversation tip, this is anti-naive-edit integrity,
         -- not a signature against an adversary who can rewrite every table.
         CREATE TABLE IF NOT EXISTS prompt_artifact_tips (
             conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
             last_seq         INTEGER NOT NULL CHECK (last_seq > 0),
             tip_hash         TEXT NOT NULL
         );
         -- #1030 Plans within Plans: the roadmap tree, persisted as a serialized
         -- plan.rs::Plan blob (Roadmap->Phase->Plan->Task Subtask nodes). A Plan
         -- node binds a conversations row via conversations.node_id; the tree's
         -- shape lives HERE, never on the hash-chained transcript rows.
         CREATE TABLE IF NOT EXISTS roadmaps (
             id                 TEXT NOT NULL,
             workspace_key      TEXT NOT NULL,
             title              TEXT NOT NULL DEFAULT '',
             tree               TEXT NOT NULL DEFAULT '',   -- serialized plan.rs::Plan (TOML); empty = no nodes yet
             schema_version     INTEGER NOT NULL DEFAULT 1,
             created_at_claim   INTEGER NOT NULL DEFAULT 0,
             updated_at_claim   INTEGER NOT NULL DEFAULT 0,
             -- #1086: a roadmap's identity is (id, workspace_key), NOT id alone.
             -- With id-only PK an `INSERT OR REPLACE` from /roadmap import could
             -- REPLACE a same-id row owned by ANOTHER workspace (the read fence
             -- held, the write fence did not). The composite key makes the write
             -- path workspace-fenced too: importing an id that lives under a
             -- different workspace inserts a separate row, never steals.
             PRIMARY KEY (id, workspace_key)
         );
         CREATE INDEX IF NOT EXISTS idx_roadmaps_ws
             ON roadmaps (workspace_key);
         -- #1030 collision fix: at most ONE live process owns a conversation.
         -- conversation_id is the global PK, so a second live newt cannot claim a
         -- conversation another holds; a stale claim (dead pid / new boot_id) is
         -- reclaimed on the next claim. Also the source of the /resume liveness column.
         CREATE TABLE IF NOT EXISTS live_owners (
             conversation_id    TEXT PRIMARY KEY,
             host               TEXT NOT NULL,
             boot_id            TEXT NOT NULL,
             pid                INTEGER NOT NULL,
             writer_fingerprint TEXT NOT NULL,
             heartbeat_tick     INTEGER NOT NULL
         );
         -- A3 (W6) attach-inject inbox: prompts an ATTACH surface (newt-web)
         -- enqueues for the RUNNING session to consume as its next turn. The
         -- attach surface writes ONLY here — never a turn/claim — so the
         -- claim-holding REPL stays the sole writer of the transcript (D2). A
         -- brand-new table: `IF NOT EXISTS` creates it on every existing db, no
         -- rebuild. `delivered_receipt_id` back-links the durable turn the
         -- injection became (the additive, auditable 'entered via web' proof
         -- that avoids a prompt_receipts CHECK migration).
         CREATE TABLE IF NOT EXISTS conversation_inbox (
             id                   TEXT PRIMARY KEY,
             conversation_id      TEXT NOT NULL,
             workspace_key        TEXT NOT NULL,          -- same fence every table carries
             seq                  INTEGER NOT NULL,       -- per-conversation FIFO order (ASC)
             body                 TEXT NOT NULL,          -- the injected prompt text
             idem_key             TEXT,                   -- idempotency (double-submit / SSE reconnect)
             delivered            INTEGER NOT NULL DEFAULT 0,
             delivered_receipt_id TEXT,                   -- back-link → prompt_receipts.id
             injected_at_claim    INTEGER NOT NULL,       -- DISPLAY ONLY (wall-clock, unix nanos)
             UNIQUE(conversation_id, idem_key)
         );
         CREATE INDEX IF NOT EXISTS idx_conversation_inbox_poll
             ON conversation_inbox (conversation_id, workspace_key, delivered, seq);
         -- A4 (W6) permission-decision channel: the RUNNING gate (sole authority
         -- minter) publishes a pending permission decision here; an ATTACH
         -- surface (newt-web) renders it and writes back a VERDICT (never
         -- caveats). The gate races this against the TTY and resolves it
         -- exactly-once. A brand-new table: `IF NOT EXISTS` on every existing db,
         -- no rebuild. `danger_json` is GATE-STAMPED (the web never classifies
         -- danger) so a high-danger row cannot be forged to a lower tier.
         CREATE TABLE IF NOT EXISTS permission_requests (
             request_id      TEXT PRIMARY KEY,        -- unguessable nonce (new_conversation_id)
             conversation_id TEXT NOT NULL,
             workspace_key   TEXT NOT NULL,           -- same fence every table carries
             requests_json   TEXT NOT NULL,           -- serialized &[PermissionRequest]
             danger_json     TEXT NOT NULL,           -- gate-stamped per-target tier
             verdict         TEXT,                    -- NULL until a surface answers
             answered_by     TEXT,                    -- audit: 'web' | 'tty' | 'expired'
             resolved        INTEGER NOT NULL DEFAULT 0,
             created_tick    INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_permission_requests_pending
             ON permission_requests (conversation_id, workspace_key, resolved);

         -- Passkey enrollment staging (#1369). The WEB stages a candidate
         -- binding here; the TERMINAL promotes it to the signed credential
         -- registry. Deliberately UNLIKE permission_requests, there is no
         -- `verdict` column and no web-writable answer: a web actor can only
         -- ever propose, never confer authority. Nothing in this table is
         -- authority — a row is a proposal that expires.
         CREATE TABLE IF NOT EXISTS enrollment_requests (
             request_id      TEXT PRIMARY KEY,        -- unguessable nonce (new_conversation_id)
             conversation_id TEXT NOT NULL,
             workspace_key   TEXT NOT NULL,           -- same fence every table carries
             candidate_json  TEXT NOT NULL,           -- serialized EnrollmentCandidate
             resolved        INTEGER NOT NULL DEFAULT 0,
             resolved_by     TEXT,                    -- audit: 'terminal' | 'declined'
             created_tick    INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_enrollment_requests_pending
             ON enrollment_requests (conversation_id, workspace_key, resolved);",
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
            // #713: scratchpad <state> snapshot. Additive — an older db gains it
            // on open with the historically-true empty backfill (`{}`). It rides
            // the conversation row, NOT a turn, so it is NEVER part of the §6
            // canonical encoding: working memory, not provenance.
            ("scratchpad", "TEXT NOT NULL DEFAULT '{}'"),
            // #715: plan-ledger snapshot. Additive — an older db gains it on open
            // with the empty backfill (`{}`, parsed via PlanSnapshot's serde
            // default). It rides the conversation row, NOT a turn, so it is NEVER
            // part of the §6 canonical encoding: working memory, not provenance.
            ("plan", "TEXT NOT NULL DEFAULT '{}'"),
            // #1030: thin pointers locating this conversation in a roadmap tree.
            // Additive — an older db gains them on open with the NULL backfill
            // (an ad-hoc chat). Metadata, NOT part of the §6 canonical encoding,
            // so every existing tip_hash chain still verifies byte-for-byte.
            ("roadmap_id", "TEXT"),
            ("node_id", "TEXT"),
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
            // N1 on #261: rows written before this column exist only as v1,
            // so DEFAULT 1 is the historically-true backfill.
            ("encoding_version", "INTEGER NOT NULL DEFAULT 1"),
            // #717: phantom-reach telemetry. Additive — an older db gains it on
            // open with the historically-true empty backfill. NOT part of the §6
            // canonical encoding, so existing chains verify byte-for-byte.
            ("phantom_reaches", "TEXT NOT NULL DEFAULT '[]'"),
        ],
    ),
    (
        "writer_clock",
        &[
            ("writer_fingerprint", "TEXT"),
            ("last_tick", "INTEGER NOT NULL DEFAULT 0"),
        ],
    ),
    (
        "prompt_receipts",
        &[
            // Prompt receipt v2: v1 hashes did not include an active authority
            // pointer. NULL is therefore the only honest additive backfill;
            // reads reconstruct v1 authority through the validated parent
            // chain, while every new v2 row stores and hashes this field.
            ("active_operator_id", "TEXT REFERENCES prompt_receipts(id)"),
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

/// SQL expression deriving a space-joined list of `$.{key}` string values
/// from the `events` JSON array carried by `source` (e.g. `new.events`,
/// `old.events`, or the bare column in the content view).
/// One-time import of the retired JSON backend's tree (see the module docs).
///
/// Runs on every open and is a fast no-op when `<root>/conversations/` does
/// not exist (i.e. always, after the first successful import renames it to
/// the backup dir). Records are imported oldest-first by the legacy MRU
/// ordering (`updated_at`, then `created_at`, then id — the JSON backend's
/// own sort), so ticks assigned in import order reproduce the conversation
/// ordering users saw before the migration.
fn import_legacy_json(
    conn: &Connection,
    root: &Path,
    writer_fingerprint: &str,
) -> anyhow::Result<()> {
    let legacy_root = root.join(LEGACY_JSON_DIR);
    if !legacy_root.is_dir() {
        return Ok(());
    }
    let mut records = collect_legacy_records(&legacy_root)?;
    records.sort_by(|a, b| {
        a.updated_at_unix_nanos
            .cmp(&b.updated_at_unix_nanos)
            .then_with(|| a.created_at_unix_nanos.cmp(&b.created_at_unix_nanos))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut imported = 0usize;
    for record in &records {
        if import_one_record(conn, record, writer_fingerprint)? {
            imported += 1;
        }
    }
    let backup = retire_legacy_dir(root, &legacy_root)?;
    tracing::info!(
        imported,
        found = records.len(),
        backup = %backup.display(),
        "one-time import of legacy JSON conversations complete; \
         the original tree is kept as a backup"
    );
    Ok(())
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

/// 17.2 one-shot row migration (see module docs — Workspace identity v2):
/// re-key every conversation that carries THIS workspace's retired UUIDv5
/// key to the v2 key, in one UPDATE inside an immediate transaction.
///
/// Idempotent by construction — once no rows carry the old key the UPDATE
/// matches nothing. Scoped by construction — a UUIDv5 key is derived from
/// one canonical path, so the WHERE clause can only ever select rows that
/// belonged to this workspace; every other workspace's rows are left for
/// their own open to migrate. Re-keying is metadata, not activity: no tick
/// is allocated, and the §6 chain is untouched (`workspace_key` is not part
/// of the turn encoding or the genesis hash).
fn migrate_workspace_key(conn: &Connection, workspace: &Path, v2_key: &str) -> anyhow::Result<()> {
    // The deprecated v1 derivation is retained exactly for this lookup.
    #[allow(deprecated)]
    let old_key = ConversationStore::workspace_id_for_path(workspace)?;
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let migrated = tx.execute(
        "UPDATE conversations SET workspace_key = ?2 WHERE workspace_key = ?1",
        rusqlite::params![old_key, v2_key],
    )?;
    tx.commit()?;
    if migrated > 0 {
        tracing::info!(
            migrated,
            workspace = %workspace.display(),
            "re-keyed conversations from the retired UUIDv5 workspace key to v2"
        );
    }
    Ok(())
}

/// Walk `<legacy_root>/<workspace-uuid>/<id>.json` and parse every readable
/// record — all workspaces, not just the opening store's. Corrupt or
/// unreadable records are skipped with a warning (the legacy store's own
/// semantics); whatever is skipped survives untouched in the backup dir.
fn collect_legacy_records(legacy_root: &Path) -> anyhow::Result<Vec<ConversationRecord>> {
    let mut records = Vec::new();
    for ws_entry in std::fs::read_dir(legacy_root)? {
        let ws_dir = ws_entry?.path();
        if !ws_dir.is_dir() {
            // Stray file at the workspace level — not a record; the backup
            // rename preserves it.
            continue;
        }
        let dir_key = ws_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for entry in std::fs::read_dir(&ws_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                // The legacy store only ever read `.json` (crash-leftover
                // `.tmp` files were invisible to it).
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(serde_json::from_str::<ConversationRecord>(&text)?));
            let record = match parsed {
                Ok(record) => record,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "skipping unreadable legacy conversation record \
                         (the file is kept in the import backup)"
                    );
                    continue;
                }
            };
            // The legacy store only served records whose body workspace_id
            // matched their dir name; a mismatched record was invisible to
            // every workspace. Importing it would resurrect data no store
            // could see — skip it (it stays in the backup).
            if record.workspace_id != dir_key {
                tracing::warn!(
                    path = %path.display(),
                    body_workspace = %record.workspace_id,
                    dir_workspace = %dir_key,
                    "skipping legacy record whose workspace id does not match its dir"
                );
                continue;
            }
            if let Err(e) = validate_record_id(&record.id) {
                tracing::warn!(path = %path.display(), error = %e, "skipping legacy record");
                continue;
            }
            records.push(record);
        }
    }
    Ok(records)
}

/// Import one legacy record inside its own `BEGIN IMMEDIATE` transaction:
/// conversation row first (one tick, genesis tip), then each turn through
/// the normal tick + chain path, then the activity/tip update — exactly the
/// shape the live write path produces, so `verify_chain` holds post-import.
/// Returns `false` (and writes nothing) when the id already exists in the
/// database — in any workspace: that means an earlier pass imported it (or
/// the id collides), and the import never overwrites.
fn import_one_record(
    conn: &Connection,
    record: &ConversationRecord,
    writer_fingerprint: &str,
) -> anyhow::Result<bool> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let already: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM conversations WHERE id = ?1",
            [&record.id],
            |row| row.get(0),
        )
        .optional()?;
    if already.is_some() {
        tracing::debug!(id = %record.id, "legacy conversation already in the db; skipping");
        return Ok(false);
    }

    // §6: the legacy unix_nanos enter ONLY as display claims.
    let started_claim = clamp_claim(record.created_at_unix_nanos);
    let updated_claim = clamp_claim(record.updated_at_unix_nanos);
    let create_tick = next_tick(&tx, writer_fingerprint)?;
    tx.execute(
        "INSERT INTO conversations
           (id, title, workspace_path, workspace_key, persona, end_reason,
            writer_fingerprint, activity_tick, tip_hash,
            started_at_claim, updated_at_claim)
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            record.id,
            record.title,
            record.workspace,
            record.workspace_id,
            record.persona,
            writer_fingerprint,
            create_tick,
            genesis_hash(&record.id, writer_fingerprint),
            started_claim,
            updated_claim,
        ],
    )?;

    let mut prev_hash = genesis_hash(&record.id, writer_fingerprint);
    let mut last_tick = create_tick;
    for turn in &record.turns {
        let seq = next_tick(&tx, writer_fingerprint)?;
        let row = TurnRow {
            conversation_id: record.id.clone(),
            writer_fingerprint: writer_fingerprint.to_string(),
            seq,
            prev_hash,
            user: turn.user.clone(),
            assistant: turn.assistant.clone(),
            events: "[]".to_string(),
            tokens_in: None,
            tokens_out: None,
            // The legacy format recorded no per-turn time; the record-level
            // updated_at is the only available claim (display only, §6).
            ts_claim: updated_claim,
            encoding_version: TURN_ENCODING_VERSION_CURRENT,
        };
        // #717: the legacy JSON backend recorded no phantom reaches (it predates
        // the column), exactly as it recorded no tool events (`events: "[]"`).
        insert_turn_row(&tx, &row, "[]")?;
        prev_hash = row.content_hash()?;
        last_tick = seq;
    }
    if !record.turns.is_empty() {
        tx.execute(
            "UPDATE conversations SET activity_tick = ?2, tip_hash = ?3 WHERE id = ?1",
            rusqlite::params![record.id, last_tick, prev_hash],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

/// Move the legacy tree to the backup name (`conversations.imported/`,
/// suffixed if that already exists). A concurrent opener may win the rename;
/// finding the source already gone is success, not an error.
fn retire_legacy_dir(root: &Path, legacy_root: &Path) -> anyhow::Result<PathBuf> {
    for attempt in 0u32..100 {
        let candidate = if attempt == 0 {
            root.join(LEGACY_BACKUP_DIR)
        } else {
            root.join(format!("{LEGACY_BACKUP_DIR}.{attempt}"))
        };
        if candidate.exists() {
            continue;
        }
        return match std::fs::rename(legacy_root, &candidate) {
            Ok(()) => Ok(candidate),
            Err(_) if !legacy_root.exists() => Ok(candidate),
            Err(e) => Err(anyhow::Error::from(e).context(format!(
                "imported legacy conversations but could not move {} aside to {}",
                legacy_root.display(),
                candidate.display()
            ))),
        };
    }
    anyhow::bail!(
        "no free backup name for {} (conversations.imported* all taken)",
        legacy_root.display()
    )
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

/// A `live_owners` row (#1030) — a process that has a conversation open —
/// handed to the [`LivenessFn`] to decide whether it is still LIVE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOwner {
    /// Hostname of the owning process's machine.
    pub host: String,
    /// Kernel boot id at claim time. A different boot id on the same host means
    /// the machine rebooted, so every prior pid is gone (the claim is stale).
    pub boot_id: String,
    /// OS process id of the owner.
    pub pid: i64,
    /// The owner's writer fingerprint. Shared per machine (from `identity.pem`),
    /// so it is NOT a process-unique key — stored for provenance, not identity.
    pub writer_fingerprint: String,
    /// Claim-clock tick of the owner's last heartbeat — the freshness signal a
    /// cross-host / post-reboot liveness check falls back to.
    pub heartbeat_tick: i64,
}

/// The outcome of [`ConversationStore::claim`] (#1030 collision fix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This process now owns the conversation — a fresh claim, a re-affirmation
    /// of its own claim, or a reclaim of a stale (crashed/rebooted) owner.
    Claimed,
    /// A DIFFERENT, LIVE process owns it. The fields drive an honest message
    /// ("open in another newt, pid N on host H"); the caller must NOT attach.
    HeldBy { host: String, pid: i64 },
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

impl Verdict {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowSession => "allow_session",
            Self::Deny => "deny",
        }
    }
    fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "allow_once" => Some(Self::AllowOnce),
            "allow_session" => Some(Self::AllowSession),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// A pending permission decision as the attach surface renders it
/// ([`ConversationStore::pending_permission_request`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    /// The unguessable nonce that binds a verdict to THIS request (not a later
    /// one — a turn issues several `gate.ask` calls).
    pub request_id: String,
    /// Serialized `&[PermissionRequest]` (the web deserializes to render).
    pub requests_json: String,
    /// Gate-stamped per-target danger tier (the web renders, never classifies).
    pub danger_json: String,
}

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

/// The outcome of [`ConversationStore::answer_permission_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// The verdict was recorded (awaiting the gate's poll to resolve it).
    Answered,
    /// The request was already answered or resolved — a safe no-op.
    AlreadyResolved,
    /// No such open request for this conversation (stale/unknown request_id).
    Unknown,
}

/// Liveness oracle: is `owner` still a running process, as of `now`? Injectable
/// (like the claim clock) so the unit tier is fully mocked — the production
/// [`system_liveness`] touches the OS (pid probe + boot id); a test double
/// decides from the row alone. A plain `fn`, so it carries no captured state.
pub type LivenessFn = fn(owner: &StoredOwner, now: i64) -> bool;

/// A held conversation whose owner's last heartbeat is older than this is
/// treated as stale (reclaimable) — but ONLY on the fallback path where the pid
/// probe is not authoritative (a foreign host, or the same host after a reboot).
/// One hour: comfortably longer than the gap between a live session's per-turn
/// heartbeats, short enough that a genuinely dead cross-host session frees its
/// conversation the same day.
const LIVENESS_STALE_AFTER_NANOS: i64 = 3_600 * 1_000_000_000;

/// The production [`LivenessFn`]. Same machine and boot: the pid probe is
/// authoritative. Otherwise (a foreign host, or this host after a reboot — where
/// the stored pid is meaningless) fall back to heartbeat freshness.
fn system_liveness(owner: &StoredOwner, now: i64) -> bool {
    let (host, boot_id) = current_host_boot();
    if owner.host == host && owner.boot_id == boot_id {
        return pid_is_alive(owner.pid);
    }
    now.saturating_sub(owner.heartbeat_tick) < LIVENESS_STALE_AFTER_NANOS
}

/// Is `pid` a currently-running process? `kill(pid, 0)` delivers no signal but
/// performs the existence + permission check: `0` = alive; `EPERM` = alive but
/// owned by another user (still alive); `ESRCH` = gone.
#[cfg(unix)]
fn pid_is_alive(pid: i64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 only probes a pid; it never delivers a signal.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Windows analogue of the `kill(pid, 0)` probe above: `OpenProcess` with the
/// lightest query right performs the same "does this pid exist" check without
/// acting on the process. A live pid opens; a dead/reused pid fails with
/// `ERROR_INVALID_PARAMETER`. `ERROR_ACCESS_DENIED` also means alive — the
/// process exists but this process lacks rights to query it, the Windows
/// analogue of the Unix `EPERM` case above.
#[cfg(windows)]
fn pid_is_alive(pid: i64) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    // SAFETY: `OpenProcess` only queries a handle; it takes no action on the
    // target process.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    }
    // SAFETY: `handle` was just returned by the successful `OpenProcess` call
    // above and is not used again after this call.
    unsafe { CloseHandle(handle) };
    true
}

/// This machine's `(hostname, kernel boot id)`. Both come from `/proc` (Linux —
/// the dev + CI + deploy target) and degrade to `("localhost", "")` off-Linux,
/// which simply makes the pid probe the sole liveness signal on the local host.
fn current_host_boot() -> (String, String) {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    (host, boot_id)
}

/// Read a raw `live_owners` row (no liveness judgement). Shared by `claim`
/// (inside its `BEGIN IMMEDIATE` txn) and `live_owner`.
fn live_owner_row(conn: &Connection, conversation_id: &str) -> anyhow::Result<Option<StoredOwner>> {
    conn.query_row(
        "SELECT host, boot_id, pid, writer_fingerprint, heartbeat_tick
           FROM live_owners WHERE conversation_id = ?1",
        rusqlite::params![conversation_id],
        |row| {
            Ok(StoredOwner {
                host: row.get(0)?,
                boot_id: row.get(1)?,
                pid: row.get(2)?,
                writer_fingerprint: row.get(3)?,
                heartbeat_tick: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
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
mod tests {
    use super::*;

    fn insert_prompt_lineage_for_test(
        store: &ConversationStore,
        conversation_id: &str,
        depth: usize,
    ) -> (PromptId, PromptId) {
        assert!(depth >= 1);
        store
            .create_with_id(conversation_id, "lineage test", None)
            .unwrap();
        let writer = store.writer_fingerprint().to_string();
        let root_id = PromptId::new();
        let root = PromptReceipt::new(
            root_id,
            conversation_id.to_string(),
            writer.clone(),
            1,
            None,
            None,
            root_id,
            root_id,
            PromptOrigin::Operator,
            b"root".to_vec(),
            b"root".to_vec(),
            1,
        );
        let conn = store.lock_conn();
        let tx =
            rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).unwrap();
        insert_prompt_receipt(&tx, &root).unwrap();
        let mut previous_id = root_id;
        for index in 1..depth {
            let id = PromptId::new();
            let text = format!("retry-{index}").into_bytes();
            let retry = PromptReceipt::new(
                id,
                conversation_id.to_string(),
                writer.clone(),
                i64::try_from(index + 1).unwrap(),
                Some(previous_id),
                Some(previous_id),
                root_id,
                root_id,
                PromptOrigin::HarnessRetry,
                text.clone(),
                text,
                i64::try_from(index + 1).unwrap(),
            );
            insert_prompt_receipt(&tx, &retry).unwrap();
            previous_id = id;
        }
        tx.commit().unwrap();
        (root_id, previous_id)
    }

    // Durable prompt receipts: write-before-work provenance. These tests are
    // intentionally store-level because a receipt must survive even when no
    // assistant turn is ever appended.
    #[test]
    fn prompt_receipt_is_byte_exact_and_survives_an_incomplete_turn() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let conversation_id = "prompt-byte-exact";

        let receipt = {
            let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
            let prompt = crate::prompt::NewPrompt::operator(
                b"raw\0bytes\xff".to_vec(),
                "model text\nwith Unicode: \u{1f9ad}".as_bytes().to_vec(),
            );
            store
                .begin_prompt(conversation_id, "prompt title", None, prompt)
                .unwrap()
                .submitted()
                .receipt()
                .clone()
        };

        // Reopen the database: the prompt is durable despite there being no
        // completed `turns` row at all.
        let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let loaded = reopened
            .load_prompt_in_conversation(conversation_id, receipt.id())
            .unwrap()
            .expect("prompt receipt survives a failed/interrupted turn");
        assert_eq!(loaded.raw_text(), b"raw\0bytes\xff");
        assert_eq!(
            loaded.model_text_utf8().unwrap(),
            "model text\nwith Unicode: \u{1f9ad}"
        );
        loaded.verify_integrity().unwrap();
        assert!(reopened.load(conversation_id).unwrap().turns.is_empty());
    }

    #[test]
    fn prompt_chronology_is_automatic_but_objective_parentage_is_explicit() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conv = "prompt-ancestry";

        let first = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("first", "first"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        assert_eq!(first.root_prompt_id(), first.id());
        assert_eq!(first.previous_prompt_id(), None);
        assert_eq!(first.parent_prompt_id(), None);

        // A normal new operator prompt is chronologically after `first`, but
        // is a new objective root. Chronology must never silently become
        // semantic parentage.
        let second = store
            .begin_prompt(
                conv,
                "ignored on existing conversation",
                None,
                crate::prompt::NewPrompt::operator("second", "second"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        assert_eq!(second.previous_prompt_id(), Some(first.id()));
        assert_eq!(second.parent_prompt_id(), None);
        assert_eq!(second.root_prompt_id(), second.id());

        // A harness retry is an explicit child. It inherits the validated
        // parent root, while the active operator prompt remains `first`.
        let retry = store
            .begin_prompt(
                conv,
                "ignored",
                None,
                crate::prompt::NewPrompt::harness_retry("retry", "retry", first.id()),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        assert_eq!(retry.previous_prompt_id(), Some(second.id()));
        assert_eq!(retry.parent_prompt_id(), Some(first.id()));
        assert_eq!(retry.root_prompt_id(), first.id());

        let context = store
            .turn_prompt_context(conv, retry.id())
            .unwrap()
            .expect("retry context");
        assert_eq!(context.submitted_prompt().id(), retry.id());
        assert_eq!(context.active_operator_prompt().id(), first.id());

        assert_eq!(
            store.prompt_chain(conv).unwrap(),
            vec![first, second, retry]
        );
    }

    #[test]
    fn mutable_receipt_order_cannot_reparent_the_verified_prompt_chain() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conv = "prompt-order-tamper";
        let first = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("first", "first"),
            )
            .unwrap()
            .submitted()
            .id();
        let second = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("second", "second"),
            )
            .unwrap()
            .submitted()
            .id();

        // Swap only the unhashed SQLite presentation order. Both receipts and
        // their hashed predecessor links remain individually valid.
        store
            .lock_conn()
            .execute(
                "UPDATE prompt_receipts
                    SET receipt_order = CASE id WHEN ?1 THEN 2002 WHEN ?2 THEN 2001 END
                  WHERE id IN (?1, ?2)",
                rusqlite::params![first.to_string(), second.to_string()],
            )
            .unwrap();

        let latest_error = store.latest_prompt(conv).unwrap_err().to_string();
        assert!(
            latest_error.contains("prompt chronology mismatch"),
            "{latest_error}"
        );
        let append_error = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("third", "third"),
            )
            .unwrap_err()
            .to_string();
        assert!(
            append_error.contains("prompt chronology mismatch"),
            "{append_error}"
        );
        let receipt_count: i64 = store
            .lock_conn()
            .query_row(
                "SELECT COUNT(*) FROM prompt_receipts WHERE conversation_id = ?1",
                [conv],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(receipt_count, 2, "failed append must roll back completely");
    }

    #[test]
    fn concurrent_store_connections_serialize_prompt_predecessors() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let conv = "concurrent-prompt-append";
        let seed_store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let seed = seed_store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("seed", "seed"),
            )
            .unwrap()
            .submitted()
            .id();
        drop(seed_store);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for label in ["left", "right"] {
            let root_path = root.path().to_path_buf();
            let workspace_path = workspace.path().to_path_buf();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let store = ConversationStore::new(root_path, workspace_path, 100).unwrap();
                barrier.wait();
                store
                    .begin_prompt(
                        conv,
                        "title",
                        None,
                        crate::prompt::NewPrompt::operator(label, label),
                    )
                    .unwrap()
                    .submitted()
                    .id()
            }));
        }
        barrier.wait();
        let appended: Vec<PromptId> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();

        let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let chain = reopened.prompt_chain(conv).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id(), seed);
        assert_eq!(chain[1].previous_prompt_id(), Some(seed));
        assert_eq!(chain[2].previous_prompt_id(), Some(chain[1].id()));
        assert!(appended.contains(&chain[1].id()));
        assert!(appended.contains(&chain[2].id()));
    }

    #[test]
    fn prompt_reads_are_conversation_and_workspace_fenced_and_delete_cascades() {
        let root = tempfile::tempdir().unwrap();
        let ws_a = tempfile::tempdir().unwrap();
        let ws_b = tempfile::tempdir().unwrap();
        let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
        let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

        let a = store_a
            .begin_prompt(
                "conversation-a",
                "A",
                None,
                crate::prompt::NewPrompt::operator("secret-a", "secret-a"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        let cross_workspace_append = store_b
            .begin_prompt(
                "conversation-a",
                "foreign",
                None,
                crate::prompt::NewPrompt::operator("intruder", "intruder"),
            )
            .unwrap_err()
            .to_string();
        assert!(
            cross_workspace_append.contains("belongs to another workspace"),
            "{cross_workspace_append}"
        );
        let b = store_b
            .begin_prompt(
                "conversation-b",
                "B",
                None,
                crate::prompt::NewPrompt::operator("secret-b", "secret-b"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();

        assert!(store_b.load_prompt(a.id()).unwrap().is_none());
        assert!(store_a
            .load_prompt_in_conversation("conversation-a", b.id())
            .unwrap()
            .is_none());
        assert!(store_b.latest_prompt("conversation-a").unwrap().is_none());
        assert!(store_b.prompt_chain("conversation-a").unwrap().is_empty());
        assert!(store_b
            .turn_prompt_context("conversation-a", a.id())
            .unwrap()
            .is_none());
        assert!(store_a.previous_prompt(b.id()).unwrap().is_none());

        store_a.delete("conversation-a").unwrap();
        assert!(store_a.load_prompt(a.id()).unwrap().is_none());
    }

    #[test]
    fn prompt_lineage_accepts_the_documented_depth_boundary() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conversation_id = "lineage-at-boundary";
        let (root_id, leaf_id) =
            insert_prompt_lineage_for_test(&store, conversation_id, MAX_PROMPT_LINEAGE_DEPTH);

        let context = store
            .turn_prompt_context(conversation_id, leaf_id)
            .unwrap()
            .expect("a lineage exactly at the documented limit is valid");
        assert_eq!(context.active().id(), root_id);
    }

    #[test]
    fn prompt_lineage_rejects_a_deeper_retry_before_inserting_it() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conversation_id = "lineage-over-boundary";
        let (_, leaf_id) =
            insert_prompt_lineage_for_test(&store, conversation_id, MAX_PROMPT_LINEAGE_DEPTH);

        let error = store
            .begin_prompt(
                conversation_id,
                "ignored",
                None,
                crate::prompt::NewPrompt::harness_retry("too deep", "too deep", leaf_id),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("maximum prompt lineage depth"), "{error}");
        assert_eq!(
            store.prompt_chain(conversation_id).unwrap().len(),
            MAX_PROMPT_LINEAGE_DEPTH
        );
    }

    #[test]
    fn prompt_lineage_rejects_a_persisted_chain_over_the_depth_limit() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conversation_id = "persisted-lineage-over-boundary";
        let (_, leaf_id) =
            insert_prompt_lineage_for_test(&store, conversation_id, MAX_PROMPT_LINEAGE_DEPTH + 1);

        let error = store
            .turn_prompt_context(conversation_id, leaf_id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds the maximum depth"), "{error}");
    }

    #[test]
    fn prompt_lineage_cycle_is_detected_without_recursion() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conversation_id = "lineage-cycle";
        store
            .create_with_id(conversation_id, "cycle test", None)
            .unwrap();

        let writer = store.writer_fingerprint().to_string();
        let root_id = PromptId::new();
        let retry_a_id = PromptId::new();
        let retry_b_id = PromptId::new();
        let root_receipt = PromptReceipt::new(
            root_id,
            conversation_id.to_string(),
            writer.clone(),
            1,
            None,
            None,
            root_id,
            root_id,
            PromptOrigin::Operator,
            b"root".to_vec(),
            b"root".to_vec(),
            1,
        );
        let retry_a = PromptReceipt::new(
            retry_a_id,
            conversation_id.to_string(),
            writer.clone(),
            2,
            Some(root_id),
            Some(retry_b_id),
            root_id,
            root_id,
            PromptOrigin::HarnessRetry,
            b"retry-a".to_vec(),
            b"retry-a".to_vec(),
            2,
        );
        let retry_b = PromptReceipt::new(
            retry_b_id,
            conversation_id.to_string(),
            writer,
            3,
            Some(retry_a_id),
            Some(retry_a_id),
            root_id,
            root_id,
            PromptOrigin::HarnessRetry,
            b"retry-b".to_vec(),
            b"retry-b".to_vec(),
            3,
        );
        {
            let conn = store.lock_conn();
            let tx = rusqlite::Transaction::new_unchecked(&conn, TransactionBehavior::Immediate)
                .unwrap();
            tx.execute_batch("PRAGMA defer_foreign_keys = ON").unwrap();
            insert_prompt_receipt(&tx, &root_receipt).unwrap();
            insert_prompt_receipt(&tx, &retry_a).unwrap();
            insert_prompt_receipt(&tx, &retry_b).unwrap();
            tx.commit().unwrap();
        }

        let error = store
            .turn_prompt_context(conversation_id, retry_b_id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("prompt parent cycle detected"), "{error}");
    }

    #[test]
    fn prompt_ticks_reseed_the_writer_clock_without_moving_the_turn_chain_tip() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conv = "prompt-clock";

        let first = store
            .begin_prompt(
                conv,
                "clock",
                None,
                crate::prompt::NewPrompt::operator("one", "one"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        let (writer_before, tip_before): (String, String) = {
            let conn = store.lock_conn();
            conn.query_row(
                "SELECT writer_fingerprint, tip_hash FROM conversations WHERE id = ?1",
                [conv],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };

        // Simulate a lost writer_clock row. Reseeding must observe prompt seq,
        // not only completed turns and conversation activity.
        {
            let conn = store.lock_conn();
            conn.execute(
                "DELETE FROM writer_clock WHERE writer_fingerprint = ?1",
                [store.writer_fingerprint()],
            )
            .unwrap();
        }
        let second = store
            .begin_prompt(
                conv,
                "clock",
                None,
                crate::prompt::NewPrompt::operator("two", "two"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        assert!(second.seq() > first.seq());

        let (writer_after, tip_after): (String, String) = {
            let conn = store.lock_conn();
            conn.query_row(
                "SELECT writer_fingerprint, tip_hash FROM conversations WHERE id = ?1",
                [conv],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(writer_after, writer_before);
        assert_eq!(tip_after, tip_before);
    }

    #[test]
    fn prompt_receipts_do_not_backfill_historical_turns() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conv = store.create("historical", None).unwrap();
        store
            .append_turn(&conv, "old user text", "old answer")
            .unwrap();

        // Opening the prompt-capable store adds only the empty table. Existing
        // completed turns are not silently reinterpreted as receipts because
        // their ingress/raw representation is unknowable after the fact.
        let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        assert!(reopened.prompt_chain(&conv).unwrap().is_empty());
        assert!(reopened.latest_prompt(&conv).unwrap().is_none());
    }

    #[test]
    fn begin_prompt_rolls_back_lazy_conversation_when_ancestry_is_invalid() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let missing = crate::prompt::PromptId::new();

        let err = store
            .begin_prompt(
                "atomic-invalid-parent",
                "must roll back",
                None,
                crate::prompt::NewPrompt::harness_retry("raw", "model", missing),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not in conversation"), "{err}");
        assert!(!store.exists("atomic-invalid-parent").unwrap());
        assert!(store.load_prompt(missing).unwrap().is_none());
    }

    #[test]
    fn begin_prompt_rejects_non_utf8_model_bytes_before_creating_a_conversation() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

        let error = store
            .begin_prompt(
                "invalid-model-encoding",
                "must not persist",
                None,
                crate::prompt::NewPrompt::operator(b"raw may be bytes".to_vec(), vec![0xff]),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid UTF-8"), "{error}");
        assert!(!store.exists("invalid-model-encoding").unwrap());
    }

    #[test]
    fn prompt_retention_never_prunes_the_receipt_it_just_accepted() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 1).unwrap();

        store
            .begin_prompt(
                "older-conversation",
                "older",
                None,
                crate::prompt::NewPrompt::operator("older", "older"),
            )
            .unwrap();

        // Simulate an existing writer clock lagging another writer's observed
        // activity. Without an explicit exclusion, the newly accepted row's
        // low tick makes it the apparent oldest retention victim.
        {
            let conn = store.lock_conn();
            conn.execute(
                "UPDATE conversations SET activity_tick = 100 WHERE id = ?1",
                ["older-conversation"],
            )
            .unwrap();
            conn.execute(
                "UPDATE writer_clock SET last_tick = 0 WHERE writer_fingerprint = ?1",
                [store.writer_fingerprint()],
            )
            .unwrap();
        }

        let accepted = store
            .begin_prompt(
                "newly-accepted",
                "new",
                None,
                crate::prompt::NewPrompt::operator("new", "new"),
            )
            .unwrap();

        assert!(store.exists("newly-accepted").unwrap());
        assert!(store
            .load_prompt_in_conversation("newly-accepted", accepted.submitted().id())
            .unwrap()
            .is_some());
        assert!(!store.exists("older-conversation").unwrap());
    }

    #[test]
    fn prompt_retention_skips_live_owners_but_reclaims_stale_claims() {
        fn never_live(_owner: &StoredOwner, _now: i64) -> bool {
            false
        }

        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut store = ConversationStore::new(root.path(), workspace.path(), 1).unwrap();
        let first = store
            .begin_prompt(
                "live-retention-owner",
                "first",
                None,
                crate::prompt::NewPrompt::operator("first", "first"),
            )
            .unwrap();
        assert_eq!(
            store.claim("live-retention-owner").unwrap(),
            ClaimOutcome::Claimed
        );

        store
            .begin_prompt(
                "protected-new-prompt",
                "second",
                None,
                crate::prompt::NewPrompt::operator("second", "second"),
            )
            .unwrap();
        assert!(store.exists("live-retention-owner").unwrap());
        assert!(store.exists("protected-new-prompt").unwrap());
        assert!(store.load_prompt(first.submitted().id()).unwrap().is_some());

        // A crashed owner's row must not pin the conversation forever. The
        // next retention transaction uses the same liveness judgement as
        // `claim`, removes the stale owner, and reclaims the oldest rows.
        store.set_liveness_for_test(never_live);
        store
            .begin_prompt(
                "third-prompt",
                "third",
                None,
                crate::prompt::NewPrompt::operator("third", "third"),
            )
            .unwrap();
        assert!(!store.exists("live-retention-owner").unwrap());
        assert!(!store.exists("protected-new-prompt").unwrap());
        assert!(store.exists("third-prompt").unwrap());
        assert!(store.live_owner("live-retention-owner").unwrap().is_none());
    }

    #[test]
    fn create_with_id_cannot_implicitly_erase_an_accepted_prompt() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conversation_id = "prompt-cannot-be-replaced";
        let accepted = store
            .begin_prompt(
                conversation_id,
                "original",
                None,
                crate::prompt::NewPrompt::operator("raw", "model"),
            )
            .unwrap();

        let error = store
            .create_with_id(conversation_id, "replacement", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("immutable prompt receipts"), "{error}");
        assert!(store
            .load_prompt_in_conversation(conversation_id, accepted.submitted().id())
            .unwrap()
            .is_some());
        assert_eq!(store.load(conversation_id).unwrap().title, "original");
    }

    #[test]
    fn operator_continuation_inherits_root_but_is_itself_active() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conv = "operator-continuation";
        let root_prompt = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("root", "root"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        let continuation = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator_continuation(
                    "continue",
                    "continue",
                    root_prompt.id(),
                ),
            )
            .unwrap();
        assert_eq!(continuation.submitted().root_prompt_id(), root_prompt.id());
        assert_eq!(continuation.active().id(), continuation.submitted().id());
    }

    #[test]
    fn retries_preserve_nearest_operator_authority_across_reopen() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let conv = "continuation-retry-authority";

        let (a_id, b_id, retry_id, retry_again_id) = {
            let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
            let a = store
                .begin_prompt(
                    conv,
                    "title",
                    None,
                    crate::prompt::NewPrompt::operator("A", "A root objective"),
                )
                .unwrap();
            let b = store
                .begin_prompt(
                    conv,
                    "title",
                    None,
                    crate::prompt::NewPrompt::operator_continuation(
                        "B",
                        "B locked clarification",
                        a.active().id(),
                    ),
                )
                .unwrap();
            let retry = store
                .begin_prompt(
                    conv,
                    "title",
                    None,
                    crate::prompt::NewPrompt::harness_retry(
                        "retry B",
                        "retry B",
                        b.submitted().id(),
                    ),
                )
                .unwrap();
            let retry_again = store
                .begin_prompt(
                    conv,
                    "title",
                    None,
                    crate::prompt::NewPrompt::harness_retry(
                        "retry retry B",
                        "retry retry B",
                        retry.submitted().id(),
                    ),
                )
                .unwrap();

            assert_eq!(b.submitted().root_prompt_id(), a.submitted().id());
            assert_eq!(b.active().id(), b.submitted().id());
            assert_eq!(retry.submitted().root_prompt_id(), a.submitted().id());
            assert_eq!(retry.active().id(), b.submitted().id());
            assert_eq!(retry_again.active().id(), b.submitted().id());

            // Simulate receipts written by the v1 schema: no persisted active
            // pointer, and the canonical v1 hash. Reopen must recover the same
            // nearest authority by walking explicit parents. Keep the final
            // retry at v2 to prove mixed-version ancestry works too.
            for id in [b.submitted().id(), retry.submitted().id()] {
                let legacy = store
                    .load_prompt(id)
                    .unwrap()
                    .unwrap()
                    .into_legacy_v1_for_test();
                let conn = store.lock_conn();
                conn.execute(
                    "UPDATE prompt_receipts
                        SET active_operator_id = NULL, receipt_hash = ?2,
                            encoding_version = 1
                      WHERE id = ?1",
                    rusqlite::params![id.to_string(), legacy.receipt_hash()],
                )
                .unwrap();
            }
            (
                a.submitted().id(),
                b.submitted().id(),
                retry.submitted().id(),
                retry_again.submitted().id(),
            )
        };

        let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        for retry_id in [retry_id, retry_again_id] {
            let context = reopened
                .turn_prompt_context(conv, retry_id)
                .unwrap()
                .expect("retry receipt survives reopen");
            assert_eq!(context.active().id(), b_id);
            assert_eq!(context.submitted().root_prompt_id(), a_id);
        }
    }

    #[test]
    fn retry_rejects_hashed_active_pointer_that_disagrees_with_parent() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conv = "tampered-retry-authority";
        let a = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator("A", "A"),
            )
            .unwrap();
        let b = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::operator_continuation("B", "B", a.submitted().id()),
            )
            .unwrap();
        let retry = store
            .begin_prompt(
                conv,
                "title",
                None,
                crate::prompt::NewPrompt::harness_retry("retry", "retry", b.submitted().id()),
            )
            .unwrap();

        // Rehash the row after pointing it at A. Cryptographic row integrity
        // alone therefore passes; semantic validation against the explicit
        // parent B must still reject the authority substitution.
        let forged = retry
            .submitted()
            .receipt()
            .clone()
            .with_active_operator_for_test(a.submitted().id());
        {
            let conn = store.lock_conn();
            conn.execute(
                "UPDATE prompt_receipts
                    SET active_operator_id = ?2, receipt_hash = ?3
                  WHERE id = ?1",
                rusqlite::params![
                    forged.id().to_string(),
                    forged.active_operator_id().unwrap().to_string(),
                    forged.receipt_hash(),
                ],
            )
            .unwrap();
        }
        let error = store
            .turn_prompt_context(conv, forged.id())
            .unwrap_err()
            .to_string();
        assert!(error.contains("disagrees with parent authority"), "{error}");
    }

    #[test]
    fn post_commit_prune_failure_does_not_report_prompt_failure() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 1).unwrap();
        store
            .begin_prompt(
                "retention-first",
                "first",
                None,
                crate::prompt::NewPrompt::operator("first", "first"),
            )
            .unwrap();

        // Make the cap's live-owner exclusion query fail deterministically
        // after the next receipt commits. Prompt acceptance must remain Ok.
        {
            let conn = store.lock_conn();
            conn.execute_batch("ALTER TABLE live_owners RENAME TO broken_live_owners")
                .unwrap();
        }
        let accepted = store
            .begin_prompt(
                "retention-second",
                "second",
                None,
                crate::prompt::NewPrompt::operator("second", "second"),
            )
            .expect("post-commit housekeeping cannot negate prompt acceptance");
        let loaded = store
            .load_prompt(accepted.submitted().id())
            .unwrap()
            .expect("committed receipt remains readable");
        assert_eq!(loaded.model_text_utf8().unwrap(), "second");
    }

    #[test]
    fn opening_v1_prompt_schema_adds_authority_column_without_backfill_guessing() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let conv = "v1-authority-migration";
        let (b_id, retry_id) = {
            let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
            let a = store
                .begin_prompt(
                    conv,
                    "title",
                    None,
                    crate::prompt::NewPrompt::operator("A", "A"),
                )
                .unwrap();
            let b = store
                .begin_prompt(
                    conv,
                    "title",
                    None,
                    crate::prompt::NewPrompt::operator_continuation("B", "B", a.submitted().id()),
                )
                .unwrap();
            let retry = store
                .begin_prompt(
                    conv,
                    "title",
                    None,
                    crate::prompt::NewPrompt::harness_retry("retry", "retry", b.submitted().id()),
                )
                .unwrap();

            // Rewrite every row exactly as the v1 writer did, then remove the
            // v2-only column. Reconciliation must add it back as NULL rather
            // than fabricating authority that was never part of the v1 hash.
            for receipt in store.prompt_chain(conv).unwrap() {
                let legacy = receipt.into_legacy_v1_for_test();
                let conn = store.lock_conn();
                conn.execute(
                    "UPDATE prompt_receipts
                        SET active_operator_id = NULL, receipt_hash = ?2,
                            encoding_version = 1
                      WHERE id = ?1",
                    rusqlite::params![legacy.id().to_string(), legacy.receipt_hash()],
                )
                .unwrap();
            }
            {
                let conn = store.lock_conn();
                conn.execute_batch("ALTER TABLE prompt_receipts DROP COLUMN active_operator_id")
                    .unwrap();
            }
            (b.submitted().id(), retry.submitted().id())
        };

        let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let context = reopened
            .turn_prompt_context(conv, retry_id)
            .unwrap()
            .unwrap();
        assert_eq!(context.active().id(), b_id);
        assert_eq!(context.submitted().receipt().active_operator_id(), None);
        let columns: Vec<String> = {
            let conn = reopened.lock_conn();
            let mut stmt = conn.prepare("PRAGMA table_info(prompt_receipts)").unwrap();
            let selected = stmt
                .query_map([], |row| row.get(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            selected
        };
        assert!(columns.iter().any(|column| column == "active_operator_id"));
    }

    #[test]
    fn prompt_load_rejects_tampered_exact_bytes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let receipt = store
            .begin_prompt(
                "tamper",
                "title",
                None,
                crate::prompt::NewPrompt::operator("raw", "model"),
            )
            .unwrap()
            .submitted()
            .receipt()
            .clone();
        {
            let conn = store.lock_conn();
            conn.execute(
                "UPDATE prompt_receipts SET model_text = ?2 WHERE id = ?1",
                rusqlite::params![receipt.id().to_string(), b"changed".as_slice()],
            )
            .unwrap();
        }
        let err = store.load_prompt(receipt.id()).unwrap_err().to_string();
        assert!(err.contains("model-text digest mismatch"), "{err}");
    }

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
    fn clamp_claim_saturates_oversized_legacy_nanos() {
        assert_eq!(clamp_claim(0), 0);
        assert_eq!(clamp_claim(42), 42);
        assert_eq!(clamp_claim(u128::MAX), i64::MAX);
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

    // --- load_turn: the by-(conv, seq) read for memory_fetch (#319) --------

    /// `load_turn` returns one past turn verbatim, addressed by the §6 seq the
    /// model saw in a recall hit; an unknown seq / conversation is `Ok(None)`
    /// (labelled absence, never an error — the `memory_fetch` tool contract).
    #[test]
    fn load_turn_reads_one_turn_by_seq_and_misses_are_none() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let conv = store.create("t", None).unwrap();
        store
            .append_turn(&conv, "the question", "the answer")
            .unwrap();

        // The seq the model would paste comes from a recall hit.
        let hits = store.search("question", 5).unwrap();
        assert_eq!(hits.len(), 1);
        let seq = hits[0].seq;

        let turn = store.load_turn(&conv, seq).unwrap().expect("turn exists");
        assert_eq!(turn.user, "the question");
        assert_eq!(turn.assistant, "the answer");

        // Unknown seq → None, not an error.
        assert!(store.load_turn(&conv, seq + 9_999).unwrap().is_none());
        // Unknown conversation id → None, not an error (no cross-ws leak path).
        assert!(store.load_turn("no-such-conv", seq).unwrap().is_none());
    }

    // --- end_reason: /end · /restart · :wq close-out (17.7 wiring) ---------

    /// `end_conversation` marks the row so `latest_open` skips it on
    /// auto-resume, while `list` (and therefore `/recall`/`/conversation`)
    /// still sees it — ended, not deleted.
    #[test]
    fn end_conversation_hides_row_from_latest_open_but_not_from_list() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

        let c1 = store.create("first", None).unwrap();
        store.append_turn(&c1, "q1", "a1").unwrap();
        let c2 = store.create("second", None).unwrap();
        store.append_turn(&c2, "q2", "a2").unwrap();

        // c2 was written last → highest activity tick → the resume target.
        assert_eq!(store.latest_open().unwrap().unwrap().id, c2);

        // End c2: latest_open falls back to the prior OPEN conversation…
        store.end_conversation(&c2, "wq").unwrap();
        assert_eq!(
            store.latest_open().unwrap().unwrap().id,
            c1,
            "an ended conversation is skipped on auto-resume"
        );
        // …but both rows are still listed (ended ≠ deleted).
        assert_eq!(store.list().unwrap().len(), 2);
        // …and the ended conversation is still recall-searchable.
        assert!(
            store
                .search("q2", 5)
                .unwrap()
                .iter()
                .any(|h| h.conversation_id == c2),
            "ended conversation stays in the FTS index for /recall"
        );

        // End the last open one too → nothing left to auto-resume → fresh.
        store.end_conversation(&c1, "end").unwrap();
        assert!(store.latest_open().unwrap().is_none());
        assert_eq!(store.list().unwrap().len(), 2, "still listed after ending");
    }

    /// `list_all` spans every workspace (the fenced `list` does not) and pairs
    /// each conversation with the `workspace_path` a follower re-opens the store
    /// at — the exact mechanism a cross-workspace attach surface needs.
    #[test]
    fn list_all_spans_workspaces_and_carries_their_paths() {
        let root = tempfile::tempdir().unwrap();
        let ws_a = tempfile::tempdir().unwrap();
        let ws_b = tempfile::tempdir().unwrap();
        let canon = |d: &std::path::Path| {
            std::fs::canonicalize(d)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };

        // One store root (one db); two different workspaces.
        let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
        let a = store_a.create("in A", None).unwrap();
        store_a.append_turn(&a, "q", "a").unwrap();
        let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
        let b = store_b.create("in B", None).unwrap();
        store_b.append_turn(&b, "q", "a").unwrap();

        // The fenced list only sees its own workspace.
        assert_eq!(store_a.list().unwrap().len(), 1);
        assert_eq!(store_b.list().unwrap().len(), 1);

        // list_all (from EITHER handle) sees both, each with its real path.
        let all = store_a.list_all().unwrap();
        assert_eq!(all.len(), 2, "both workspaces' conversations");
        let path_of = |id: &str| {
            all.iter()
                .find(|(s, _)| s.id == id)
                .map(|(_, p)| p.clone())
                .unwrap()
        };
        assert_eq!(path_of(&a), canon(ws_a.path()));
        assert_eq!(path_of(&b), canon(ws_b.path()));

        // The returned path is exactly what lets a follower load a conversation
        // from ANOTHER workspace: re-open at B's path, load B's conversation.
        let follower = ConversationStore::new(root.path(), path_of(&b), 100).unwrap();
        assert_eq!(follower.load(&b).unwrap().title, "in B");
    }

    /// The A3/W6 attach-inject inbox: enqueue is FIFO and idempotent, dequeue is
    /// exactly-once and non-blocking on empty, and both are workspace-fenced —
    /// the properties the interactive-attach seam rests on (D2: the web writes
    /// only the inbox; the REPL alone writes turns).
    #[test]
    fn inbox_inject_take_is_exactly_once_fifo_idempotent_and_fenced() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conv = store.create("session", None).unwrap();

        // Empty inbox → non-blocking None (the REPL poll never stalls).
        assert_eq!(store.take_injected_prompt(&conv).unwrap(), None);

        // Enqueue two; dequeue FIFO, exactly-once.
        assert_eq!(
            store.inject_prompt(&conv, "first", None).unwrap(),
            InjectOutcome::Enqueued
        );
        assert_eq!(
            store.inject_prompt(&conv, "second", None).unwrap(),
            InjectOutcome::Enqueued
        );
        assert_eq!(
            store.take_injected_prompt(&conv).unwrap().unwrap().body,
            "first"
        );
        assert_eq!(
            store.take_injected_prompt(&conv).unwrap().unwrap().body,
            "second"
        );
        assert_eq!(
            store.take_injected_prompt(&conv).unwrap(),
            None,
            "drained exactly once"
        );

        // Idempotency: the same idem_key is a no-op, not a second enqueue.
        assert_eq!(
            store.inject_prompt(&conv, "again", Some("k1")).unwrap(),
            InjectOutcome::Enqueued
        );
        assert_eq!(
            store.inject_prompt(&conv, "again", Some("k1")).unwrap(),
            InjectOutcome::Duplicate
        );
        assert_eq!(
            store.take_injected_prompt(&conv).unwrap().unwrap().body,
            "again"
        );
        assert_eq!(
            store.take_injected_prompt(&conv).unwrap(),
            None,
            "the idem duplicate did not enqueue twice"
        );

        // link_inbox_delivery records the receipt back-link without error.
        store.inject_prompt(&conv, "linked", None).unwrap();
        let taken = store.take_injected_prompt(&conv).unwrap().unwrap();
        store.link_inbox_delivery(&taken.id, "receipt-123").unwrap();

        // Workspace fence: a store on ANOTHER workspace can neither inject into
        // nor take from this conversation.
        let ws_b = tempfile::tempdir().unwrap();
        let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
        assert!(
            store_b.inject_prompt(&conv, "cross-ws", None).is_err(),
            "cross-workspace inject is rejected"
        );
        store.inject_prompt(&conv, "mine", None).unwrap();
        assert_eq!(
            store_b.take_injected_prompt(&conv).unwrap(),
            None,
            "cross-workspace take sees nothing"
        );
        assert!(
            store.take_injected_prompt(&conv).unwrap().is_some(),
            "the owning workspace still dequeues it"
        );
    }

    /// The A4/W6 permission channel: publish -> pending; a web verdict is taken
    /// exactly-once by the gate; a TTY resolve wins the race so a late web
    /// answer is discarded; verdicts bind to their own request_id; and the
    /// whole channel is workspace-fenced.
    #[test]
    fn permission_request_expires_on_created_tick_ttl() {
        // #1356: an unanswered permission-decision nonce must expire on its
        // `created_tick` — once older than the TTL the store treats it as gone
        // (not surfaced by `pending`, rejected by `answer`), so a stale
        // request_id cannot linger indefinitely consumable. Injected clock.
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let mut store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conv = store.create("session", None).unwrap();

        // Publish at t=0 — within TTL, so it is pending.
        store.set_claim_clock_for_test(|| 0);
        let r1 = store
            .publish_permission_request(&conv, "[]", r#"["low"]"#)
            .unwrap();
        assert!(
            store.pending_permission_request(&conv).unwrap().is_some(),
            "a fresh request is pending"
        );

        // Advance the clock past the TTL: the nonce is gone.
        store.set_claim_clock_for_test(|| ConversationStore::PERMISSION_REQUEST_TTL_NANOS + 1);
        assert_eq!(
            store.pending_permission_request(&conv).unwrap(),
            None,
            "an aged-out request is not surfaced"
        );
        assert_eq!(
            store
                .answer_permission_request(&conv, &r1, Verdict::AllowOnce)
                .unwrap(),
            AnswerOutcome::Unknown,
            "answering an expired request is rejected as gone"
        );

        // A request published at the new 'now' is within TTL and still answers.
        let r2 = store
            .publish_permission_request(&conv, "[]", r#"["low"]"#)
            .unwrap();
        assert!(store.pending_permission_request(&conv).unwrap().is_some());
        assert_eq!(
            store
                .answer_permission_request(&conv, &r2, Verdict::AllowOnce)
                .unwrap(),
            AnswerOutcome::Answered,
            "a within-TTL request still answers"
        );
    }

    #[test]
    fn permission_channel_publish_answer_take_race_and_fence() {
        let root = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conv = store.create("session", None).unwrap();

        // Nothing pending initially.
        assert_eq!(store.pending_permission_request(&conv).unwrap(), None);

        // Publish -> the web sees exactly this pending row.
        let r1 = store
            .publish_permission_request(&conv, r#"[{"tool":"run_command"}]"#, r#"["low"]"#)
            .unwrap();
        let pending = store.pending_permission_request(&conv).unwrap().unwrap();
        assert_eq!(pending.request_id, r1);
        assert_eq!(pending.danger_json, r#"["low"]"#);

        // The gate polls before any answer -> None (non-blocking).
        assert_eq!(store.take_permission_decision(&conv, &r1).unwrap(), None);

        // The web answers; the gate takes it exactly once and it resolves.
        assert_eq!(
            store
                .answer_permission_request(&conv, &r1, Verdict::AllowSession)
                .unwrap(),
            AnswerOutcome::Answered
        );
        assert_eq!(
            store.take_permission_decision(&conv, &r1).unwrap(),
            Some(Verdict::AllowSession)
        );
        assert_eq!(
            store.take_permission_decision(&conv, &r1).unwrap(),
            None,
            "a resolved request yields the verdict exactly once"
        );
        assert_eq!(
            store.pending_permission_request(&conv).unwrap(),
            None,
            "an answered request is no longer pending"
        );
        // A double-answer after resolution is a safe no-op.
        assert_eq!(
            store
                .answer_permission_request(&conv, &r1, Verdict::Deny)
                .unwrap(),
            AnswerOutcome::AlreadyResolved
        );

        // TTY wins the race: publish, the gate resolves-as-tty BEFORE a web
        // answer -> the win is recorded and a late web verdict never applies.
        let r2 = store.publish_permission_request(&conv, "[]", "[]").unwrap();
        assert!(
            store.resolve_permission_request(&conv, &r2, "tty").unwrap(),
            "the TTY resolve wins when unresolved"
        );
        assert!(
            !store.resolve_permission_request(&conv, &r2, "tty").unwrap(),
            "resolving an already-resolved request loses"
        );
        assert_eq!(
            store
                .answer_permission_request(&conv, &r2, Verdict::AllowOnce)
                .unwrap(),
            AnswerOutcome::AlreadyResolved,
            "a web answer after the TTY won is a no-op"
        );
        assert_eq!(
            store.take_permission_decision(&conv, &r2).unwrap(),
            None,
            "no web verdict applies once the TTY resolved it"
        );

        // A resolver from another workspace cannot consume this request, and
        // a resolver cannot overwrite the web attribution after the web has
        // recorded its verdict but before the gate takes it.
        let ws_b = tempfile::tempdir().unwrap();
        let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
        let r3 = store.publish_permission_request(&conv, "[]", "[]").unwrap();
        assert!(
            !store_b
                .resolve_permission_request(&conv, &r3, "expired")
                .unwrap(),
            "a cross-workspace resolver must not win"
        );
        assert_eq!(
            store
                .answer_permission_request(&conv, &r3, Verdict::AllowOnce)
                .unwrap(),
            AnswerOutcome::Answered
        );
        let answered_by: String = store
            .lock_conn()
            .query_row(
                "SELECT answered_by FROM permission_requests WHERE request_id = ?1",
                [&r3],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(answered_by, "web");
        assert!(
            !store
                .resolve_permission_request(&conv, &r3, "expired")
                .unwrap(),
            "timeout must not clobber a recorded web answer"
        );
        assert_eq!(
            store.take_permission_decision(&conv, &r3).unwrap(),
            Some(Verdict::AllowOnce)
        );

        // request_id binding: a verdict for r4 never resolves r5.
        let r4 = store.publish_permission_request(&conv, "[]", "[]").unwrap();
        let r5 = store.publish_permission_request(&conv, "[]", "[]").unwrap();
        store
            .answer_permission_request(&conv, &r4, Verdict::AllowOnce)
            .unwrap();
        assert_eq!(store.take_permission_decision(&conv, &r5).unwrap(), None);
        assert_eq!(
            store.take_permission_decision(&conv, &r4).unwrap(),
            Some(Verdict::AllowOnce)
        );

        // Answering an unknown request is Unknown, not a crash.
        assert_eq!(
            store
                .answer_permission_request(&conv, "no-such-id", Verdict::Deny)
                .unwrap(),
            AnswerOutcome::Unknown
        );

        // Workspace fence: another workspace can neither publish into nor read
        // this conversation's channel.
        let ws_b = tempfile::tempdir().unwrap();
        let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();
        assert!(
            store_b
                .publish_permission_request(&conv, "[]", "[]")
                .is_err(),
            "cross-workspace publish is rejected"
        );
        let r5 = store.publish_permission_request(&conv, "[]", "[]").unwrap();
        assert_eq!(
            store_b.pending_permission_request(&conv).unwrap(),
            None,
            "cross-workspace pending read sees nothing"
        );
        assert_eq!(
            store_b
                .answer_permission_request(&conv, &r5, Verdict::AllowOnce)
                .unwrap(),
            AnswerOutcome::Unknown,
            "cross-workspace answer cannot resolve it"
        );
    }

    /// Ending is metadata, not activity: it must not tick the §6 clock (so it
    /// cannot perturb MRU ordering), and re-ending is harmless.
    #[test]
    fn end_conversation_does_not_tick_activity_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();

        let older = store.create("older", None).unwrap();
        store.append_turn(&older, "q", "a").unwrap();
        let newer = store.create("newer", None).unwrap();
        store.append_turn(&newer, "q", "a").unwrap();

        let tick_of = |id: &str| -> i64 {
            let conn = store.lock_conn();
            conn.query_row(
                "SELECT activity_tick FROM conversations WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        let before = tick_of(&older);
        store.end_conversation(&older, "new").unwrap();
        assert_eq!(tick_of(&older), before, "ending must not bump the tick");
        // Idempotent: re-ending an already-ended conversation is fine.
        store.end_conversation(&older, "new").unwrap();
        // `newer` is still open and remains the resume target.
        assert_eq!(store.latest_open().unwrap().unwrap().id, newer);
    }

    // --- 17.3: the query-sanitizer adversarial matrix ---------------------

    /// Shorthand: sanitize and unwrap (the input is expected to survive).
    fn s(raw: &str) -> String {
        sanitize_fts5_query(raw).unwrap()
    }

    /// The hermes examples: dotted / hyphenated / path-like / colon tokens
    /// are auto-quoted so FTS5 reads them as text, not syntax.
    #[test]
    fn sanitizer_auto_quotes_dotted_hyphenated_and_path_tokens() {
        assert_eq!(s("chat-send"), "\"chat-send\"");
        assert_eq!(s("P2.2"), "\"P2.2\"");
        assert_eq!(s("my-app.config.ts"), "\"my-app.config.ts\"");
        assert_eq!(s("src/store.rs"), "\"src/store.rs\"");
        assert_eq!(s("tcp:p4d.p4d-ascii:1666"), "\"tcp:p4d.p4d-ascii:1666\"");
        assert_eq!(s("issue #246"), "issue \"#246\"");
        // Clean barewords pass through untouched — including underscores
        // (in FTS5's bareword alphabet) and non-ASCII text.
        assert_eq!(s("hello world"), "hello world");
        assert_eq!(s("writer_clock"), "writer_clock");
        assert_eq!(s("schlüssel wörter"), "schlüssel wörter");
    }

    #[test]
    fn sanitizer_preserves_balanced_phrases_and_drops_dangling_quotes() {
        assert_eq!(s("\"exact phrase\" extra"), "\"exact phrase\" extra");
        assert_eq!(s("say \"hello world\" now"), "say \"hello world\" now");
        // Unbalanced quote: the quote dies, its text survives as terms.
        assert_eq!(s("foo \"bar"), "foo bar");
        assert_eq!(s("\"unclosed"), "unclosed");
        assert_eq!(s("\"a b\" \"c"), "\"a b\" c");
        // Phrase content keeps operators/metachars as text (FTS5 allows
        // anything but a quote inside a phrase).
        assert_eq!(s("\"AND OR\""), "\"AND OR\"");
        assert_eq!(s("\"P2.2 chat-send\""), "\"P2.2 chat-send\"");
        // Empty / unindexable phrases are dropped, not emitted as "".
        let err = sanitize_fts5_query("\"\"").unwrap_err().to_string();
        assert!(err.contains("reduced to nothing"), "{err}");
        let err = sanitize_fts5_query("\"--\"").unwrap_err().to_string();
        assert!(err.contains("reduced to nothing"), "{err}");
    }

    #[test]
    fn sanitizer_trims_dangling_operators() {
        assert_eq!(s("foo AND"), "foo");
        assert_eq!(s("OR foo"), "foo");
        assert_eq!(s("NOT foo"), "foo");
        assert_eq!(s("foo AND AND bar"), "foo AND bar");
        assert_eq!(s("foo AND OR bar"), "foo AND bar");
        assert_eq!(s("AND foo OR"), "foo");
        // Valid binary positions survive.
        assert_eq!(s("foo OR bar"), "foo OR bar");
        assert_eq!(s("foo NOT bar"), "foo NOT bar");
        assert_eq!(s("a OR b OR c"), "a OR b OR c");
        // Lowercase forms are ordinary terms, not operators.
        assert_eq!(s("foo and bar"), "foo and bar");
        // Bare AND reduces to nothing → error, not an FTS5 syntax error.
        let err = sanitize_fts5_query("AND").unwrap_err().to_string();
        assert!(err.contains("reduced to nothing"), "{err}");
        // NEAR is reserved by FTS5 — it survives only as a quoted term.
        assert_eq!(s("NEAR"), "\"NEAR\"");
        assert_eq!(s("near"), "near");
    }

    #[test]
    fn sanitizer_strips_metacharacter_injection() {
        assert_eq!(s("(foo OR bar) AND baz"), "foo OR bar AND baz");
        assert_eq!(s("foo* ^bar"), "foo bar");
        assert_eq!(s("col*umn"), "column");
        // A lone quote / star / caret / paren reduces to nothing.
        for q in ["\"", "*", "^", "( )", "*^()"] {
            let err = sanitize_fts5_query(q).unwrap_err().to_string();
            assert!(err.contains("reduced to nothing"), "{q:?}: {err}");
        }
        // Mid-token quote: unbalanced → stripped; the halves survive.
        assert_eq!(s("fo\"o bar"), "fo o bar");
        // Punctuation-only tokens are dropped, indexable ones kept.
        assert_eq!(s("?? foo !!"), "foo");
        assert_eq!(s("foo \u{a0} "), "foo"); // unicode whitespace handled
    }

    #[test]
    fn sanitizer_handles_mixed_phrases_terms_and_operators() {
        assert_eq!(
            s("\"tuning writeback\" OR coverage-floor"),
            "\"tuning writeback\" OR \"coverage-floor\""
        );
        assert_eq!(
            s("error \"chain violation\" NOT P2.2"),
            "error \"chain violation\" NOT \"P2.2\""
        );
        // Operator directly before a phrase works too.
        assert_eq!(s("AND \"lead phrase\" tail"), "\"lead phrase\" tail");
    }

    #[test]
    fn sanitizer_errors_on_empty_and_whitespace_queries() {
        for q in ["", "   ", "\t\n"] {
            let err = sanitize_fts5_query(q).unwrap_err().to_string();
            assert!(err.contains("reduced to nothing"), "{q:?}: {err}");
        }
    }

    /// The events-extraction SQL is shared between the triggers and the
    /// content view; pin its shape (json_valid guard + coalesce to '').
    #[test]
    fn events_extract_sql_guards_and_targets_the_seam_keys() {
        let sql = events_extract_sql("new.events", "tool");
        assert!(sql.contains("json_valid(new.events)"));
        assert!(sql.contains("json_each(new.events)"));
        assert!(sql.contains("'$.tool'"));
        assert!(sql.contains("ELSE '' END"));
    }

    // ── #1086: roadmap import must not steal another workspace's row ──────────

    /// Two workspaces sharing one `conversations.db` (different workspace
    /// keys, same store root) must own their same-id roadmaps independently.
    /// Reproduces the steal: before the composite PK, `create_roadmap` in
    /// workspace B `INSERT OR REPLACE`d workspace A's row out from under it.
    #[test]
    fn create_roadmap_is_workspace_fenced_and_never_steals() {
        let root = tempfile::TempDir::new().unwrap();
        let ws_a = tempfile::TempDir::new().unwrap();
        let ws_b = tempfile::TempDir::new().unwrap();
        let store_a = ConversationStore::new(root.path(), ws_a.path(), 100).unwrap();
        let store_b = ConversationStore::new(root.path(), ws_b.path(), 100).unwrap();

        // Same roadmap id in both workspaces (exactly what /roadmap import of a
        // shared file into an unrelated workspace does).
        let id = "1783727322129749288-shared";
        store_a
            .create_roadmap(id, "A's roadmap", &crate::plan::Plan::default())
            .unwrap();
        store_b
            .create_roadmap(id, "B's roadmap", &crate::plan::Plan::default())
            .unwrap();

        // Neither clobbered the other: each workspace still sees its own.
        assert_eq!(
            store_a.load_roadmap(id).unwrap().unwrap().title,
            "A's roadmap",
            "workspace A's roadmap must survive B's import of the same id"
        );
        assert_eq!(
            store_b.load_roadmap(id).unwrap().unwrap().title,
            "B's roadmap"
        );
        // Each workspace lists exactly one.
        assert_eq!(store_a.list_roadmaps().unwrap().len(), 1);
        assert_eq!(store_b.list_roadmaps().unwrap().len(), 1);
    }

    /// Re-creating a roadmap with the SAME id in the SAME workspace still
    /// overwrites in place (the intended `INSERT OR REPLACE` semantics), so the
    /// fence does not break same-repo re-import.
    #[test]
    fn create_roadmap_overwrites_within_the_same_workspace() {
        let root = tempfile::TempDir::new().unwrap();
        let ws = tempfile::TempDir::new().unwrap();
        let store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let id = "rm-1";
        store
            .create_roadmap(id, "first", &crate::plan::Plan::default())
            .unwrap();
        store
            .create_roadmap(id, "second", &crate::plan::Plan::default())
            .unwrap();
        assert_eq!(store.load_roadmap(id).unwrap().unwrap().title, "second");
        assert_eq!(store.list_roadmaps().unwrap().len(), 1);
    }

    /// The migration rebuilds a legacy id-only-PK `roadmaps` table into the
    /// composite key, preserving rows, and is idempotent.
    #[test]
    fn migrate_roadmaps_pk_rebuilds_legacy_table_losslessly() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("t.db")).unwrap();
        // Stand up the OLD schema (id-only PK) and a row.
        conn.execute_batch(
            "CREATE TABLE roadmaps (
                 id TEXT PRIMARY KEY,
                 workspace_key TEXT NOT NULL,
                 title TEXT NOT NULL DEFAULT '',
                 tree TEXT NOT NULL DEFAULT '',
                 schema_version INTEGER NOT NULL DEFAULT 1,
                 created_at_claim INTEGER NOT NULL DEFAULT 0,
                 updated_at_claim INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO roadmaps (id, workspace_key, title) VALUES ('x', 'wsA', 'kept');",
        )
        .unwrap();

        migrate_roadmaps_pk(&conn).unwrap();

        // The row survived and the PK is now composite.
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='roadmaps'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.to_ascii_lowercase()
                .contains("primary key (id, workspace_key)"),
            "PK must be composite after migration: {sql}"
        );
        let title: String = conn
            .query_row("SELECT title FROM roadmaps WHERE id='x'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "kept");

        // Composite key now admits the same id under a second workspace…
        conn.execute(
            "INSERT INTO roadmaps (id, workspace_key, title) VALUES ('x', 'wsB', 'other')",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM roadmaps WHERE id='x'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 2, "same id can coexist across workspaces");

        // …and a second run is a no-op (idempotent).
        migrate_roadmaps_pk(&conn).unwrap();
        let n2: i64 = conn
            .query_row("SELECT COUNT(*) FROM roadmaps", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n2, 2);
    }

    /// A store opened on a db that already went through the migration (or was
    /// created fresh, hence composite) leaves the table untouched.
    #[test]
    fn fresh_store_roadmaps_table_is_already_composite() {
        let root = tempfile::TempDir::new().unwrap();
        let ws = tempfile::TempDir::new().unwrap();
        let _store = ConversationStore::new(root.path(), ws.path(), 100).unwrap();
        let conn = Connection::open(root.path().join(DB_FILE)).unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='roadmaps'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(sql
            .to_ascii_lowercase()
            .contains("primary key (id, workspace_key)"));
    }
}
