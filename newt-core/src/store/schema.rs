//! Database schema, additive reconciliation, and journal setup for the conversation store.

use std::time::Duration;

use rusqlite::Connection;

/// How long a writer waits on a locked database before erroring. Two newts
/// sharing `~/.newt/conversations.db` serialize their write transactions
/// behind this.
pub(super) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Try WAL; fall back to DELETE on the known network-filesystem failure
/// modes, returning the captured error text for a user-facing notice.
/// Any other error is real and propagates.
///
/// Under WAL, `synchronous` drops to NORMAL: SQLite documents WAL +
/// NORMAL as corruption-safe (fsync at checkpoints, not per commit), and
/// per-append cost falls from ~2 ms (one fsync per turn) to tens of µs —
/// a power cut can cost the last turns, never the database. The DELETE
/// fallback keeps the FULL default, where NORMAL is *not* corruption-safe.
pub(super) fn apply_journal_mode(conn: &Connection) -> anyhow::Result<Option<String>> {
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
pub(super) fn wal_fallback_eligible(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    lower.contains("locking protocol") || lower.contains("disk i/o error")
}

/// Schema, v17.1a. §6-binding shape — see the module docs. Every `*_claim`
/// column is a DISPLAY-ONLY wall-clock claim (unix nanos): never an ordering
/// key, never compared. Ordering is `(writer_fingerprint, seq)` /
/// `activity_tick`; integrity is the `prev_hash` BLAKE3 chain + `tip_hash`.
/// `events`/`tokens_in`/`tokens_out` are day-one columns filled by 17.6.
pub(super) fn create_schema(conn: &Connection) -> anyhow::Result<()> {
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
             node_id            TEXT,                      -- #1030: the `roadmaps` tree Subtask id this conversation realizes (NULL = ad-hoc chat)
             preference_pin     TEXT NOT NULL DEFAULT '{}' -- JSON OperatorPreferencePin (#1668): operator-pinned backend/model/cognition/tenacity; metadata, NOT hashed (§6 chain unchanged)
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
             phantom_reaches    TEXT NOT NULL DEFAULT '[]', -- JSON phantom reaches (#717); hashed by v2 rows, outside v1 hashes forever (#1786 §3.2)
             sources            TEXT NOT NULL DEFAULT '[]', -- #1786: content ids a derived row cites; canonical bytes, hashed by v2 rows
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
         -- #1786 §5: per-writer tip witnesses. Each writer's chain pins every
         -- turn EXCEPT its last (nothing chains onto a final turn), and the
         -- conversations-row witness pins only the recorded tip writer — so in
         -- a multi-writer history the other writers' final turns were pinned by
         -- nothing (#1794 residual 2). tip_seq names the turn the witness pins:
         -- an equal-seq hash mismatch is a violation, a LOWER tip_seq is a
         -- stale-but-honest witness (a rolled-back binary appended without
         -- maintaining this table) verified at its own seq and repaired by the
         -- writer's next append, a HIGHER tip_seq means rows were deleted.
         -- A missing row is absence of evidence (the writer predates the
         -- table) — never backfilled from the turns it would witness.
         CREATE TABLE IF NOT EXISTS writer_tips (
             conversation_id    TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
             writer_fingerprint TEXT NOT NULL,
             tip_hash           TEXT NOT NULL,
             tip_seq            INTEGER NOT NULL,
             PRIMARY KEY (conversation_id, writer_fingerprint)
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
         -- A3 (#1837): exactly-once resolution of a Newt Markup interaction
         -- offer. `instance_id` is the PRIMARY KEY, so one-offer-resolves-
         -- at-most-once is enforced by the schema rather than by caller
         -- discipline: the INSERT either wins or conflicts, and the
         -- conflicting writer reads back who won. `workspace_key` is stored
         -- for audit and fencing, but the fence is already intrinsic — an
         -- InstanceId is a content id over a record that CONTAINS its
         -- scope, so two workspaces cannot mint the same one.
         -- B0b-2 (#1846): the published interaction OFFER, and its terminal
         -- state. Replaces `permission_requests` as the offer/answer
         -- transport.
         --
         -- Offer and outcome live in ONE row on purpose. The race this table
         -- arbitrates has two kinds of winner — a surface ANSWERS, or the
         -- local operator CANCELS — and only the first has a Response. A
         -- single `outcome IS NULL` CAS serializes both, exactly as the old
         -- `resolved = 0 AND verdict IS NULL` CAS did. Splitting answers into
         -- `interaction_resolutions` (which requires a response id) and
         -- cancellations into a second column would make the decision two
         -- writes across two tables, and the exactly-once race is on this
         -- epic's must-not-change list.
         --
         -- `response_json` is why `answered_by` survives: the winning
         -- Response body carries `responder_provenance.audience`, so the
         -- who-answered fact stays recoverable from this row alone.
         --
         -- `danger_tier` is a PLAIN 'high'|'low', not JSON. The column it
         -- replaces (`permission_requests.danger_json`) was written as a JSON
         -- string and read as a JSON array — #1836.
         CREATE TABLE IF NOT EXISTS interaction_offers (
             instance_id     TEXT PRIMARY KEY,       -- canonical InstanceId
             conversation_id TEXT NOT NULL,
             workspace_key   TEXT NOT NULL,          -- same fence every table carries
             definition_json TEXT NOT NULL,          -- the InteractionDefinition
             instance_json   TEXT NOT NULL,          -- the InteractionInstance
             danger_tier     TEXT NOT NULL,          -- 'high' | 'low'
             published_tick  INTEGER NOT NULL,
             outcome         TEXT,                   -- NULL while answerable
             response_json   TEXT,                   -- the winner's Response
             resolved_tick   INTEGER
         );
         CREATE INDEX IF NOT EXISTS idx_interaction_offers_pending
             ON interaction_offers (conversation_id, workspace_key, outcome, published_tick);

         CREATE TABLE IF NOT EXISTS interaction_resolutions (
             instance_id     TEXT PRIMARY KEY,       -- canonical InstanceId
             workspace_key   TEXT NOT NULL,          -- same fence every table carries
             response_id     TEXT NOT NULL,          -- canonical ResponseId of the winner
             idempotency_key TEXT NOT NULL,          -- the winner's replay key
             resolved_tick   INTEGER NOT NULL
         );

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
            // #1668: the operator PREFERENCE pin (backend/model/cognition/
            // tenacity picks) a conversation carries across resume. Additive —
            // an older db gains it on open with the historically-true empty
            // backfill (`{}` = nothing pinned, so resume changes nothing). It
            // rides the conversation row, NOT a turn, so it is NEVER part of
            // the §6 canonical encoding: session metadata, not provenance.
            // Named `preference_pin`, not `posture`, so it is never confused
            // with #307's `ActivePosture` authority clamp — which is
            // process-lifetime and deliberately NOT persisted anywhere.
            ("preference_pin", "TEXT NOT NULL DEFAULT '{}'"),
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
            // #717: phantom reaches. Additive — an older db gains it on open
            // with the historically-true empty backfill. Outside the v1
            // canonical encoding (existing chains verify byte-for-byte);
            // INSIDE v2 hashes (#1786).
            ("phantom_reaches", "TEXT NOT NULL DEFAULT '[]'"),
            // #1786: provenance sources. Additive; the `'[]'` backfill is the
            // historically-true "witnessed, derived from nothing". On v1 rows
            // these bytes are never interpreted (§3.2) though they are
            // content-id inputs, so out-of-band edits orphan loudly.
            ("sources", "TEXT NOT NULL DEFAULT '[]'"),
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
pub(super) fn reconcile_schema(conn: &Connection) -> anyhow::Result<()> {
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
