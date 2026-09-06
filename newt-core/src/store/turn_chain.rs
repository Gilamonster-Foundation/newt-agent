//! §6 turn-chain encoding, row I/O, and verification.
//! Canonical bytes, content/genesis hashes, per-writer Lamport ticks, and
//! witness verdicts share this owner. Internal primitives stay store-scoped;
//! the public seam is the existing `ConversationStore` methods.
//!
//! The byte-format regression tests live HERE, beside the encoding they pin —
//! a change to the canonical encoding must fail in this file.

use rusqlite::{Connection, OptionalExtension};

use super::{parse_canonical_sources, ConversationStore};
use crate::conversation::ConversationRecord;

/// Domain-separation prefix for the v1 canonical turn encoding (`prev_hash`
/// chain). Versioned so a future encoding change cannot collide with v1.
pub(super) const TURN_ENCODING_V1_PREFIX: &[u8] = b"newt-turn:v1";

/// Domain-separation prefix for the v2 canonical turn encoding (#1786):
/// v1 plus `phantom_reaches` and `sources` in the len-prefixed group. The
/// spec is docs/spec/1786-v2-turn-encoding.md; the pinned vectors below are
/// the compatibility contract.
pub(super) const TURN_ENCODING_V2_PREFIX: &[u8] = b"newt-turn:v2";

/// Domain-separation prefix for the CONTENT ID (#1786 spec §2) — the
/// position-free identity used by provenance citations. Deliberately a
/// different domain from both chain encodings: a content id can never
/// collide with a chain hash by construction.
pub(super) const TURN_CONTENT_ID_PREFIX: &[u8] = b"newt-turn-content:v1";

/// The turn encoding version this build writes, recorded per row in
/// `turns.encoding_version` (review NIT N1 on #261). [`TurnRow::content_hash`]
/// dispatches on the stored value; v1 and v2 exist, and a row carrying an
/// unknown version errors clearly instead of hashing garbage.
///
/// **Downgrade is one-directional from the first v2 append** (#1786 spec
/// §9.1). Once any row in a conversation is v2, an older binary refuses to
/// `verify_chain` or append it ("carries encoding_version 2 … upgrade newt")
/// — and since #1785 the production read path verifies, so that refusal is
/// not cosmetic. Plain `load` still reads. Fail-closed lockout, stated and
/// accepted: binaries sharing a store upgrade together, or the older one
/// goes verified-read-only on every conversation the newer one touched.
///
/// The ONE-TIME legacy JSON import deliberately pins v1 instead of this
/// constant — see `import_one_record` (#1786 spec §9.1): legacy records
/// carry no sources or phantom reaches, so v2 buys them nothing, and a
/// v1-pinned import keeps a post-import rollback able to verify the
/// imported history (the import retires its source tree and cannot re-run).
/// That lever covers imported history only; live appends have no such
/// escape, which is exactly why the lockout above is stated here.
pub(super) const TURN_ENCODING_VERSION_CURRENT: i64 = 2;

/// Domain-separation prefix for the per-(conversation, writer) genesis hash.
pub(super) const GENESIS_PREFIX: &[u8] = b"newt-turn-chain-genesis:v1";

/// One turn row, exactly as stored. Internal: the canonical encoding hashes
/// every field, so this struct is the unit of chain verification.
///
/// `phantom_reaches` and `sources` carry the RAW stored column bytes — the
/// hash-the-stored-bytes rule (#1786 spec §2/§2.1) demands the exact bytes
/// written at append, never a re-serialization, because a struct round-trip
/// changes bytes whenever a serde type gains an additive field and would
/// silently orphan honest citations across build versions. (Pre-#1786 this
/// struct deliberately omitted reaches to keep them out of the v1 hash;
/// v2 hashes them, so the premise inverted — v1 rows simply ignore the
/// field in their encoding arm.)
#[derive(Debug)]
pub(super) struct TurnRow {
    pub(super) conversation_id: String,
    pub(super) writer_fingerprint: String,
    pub(super) seq: i64,
    pub(super) prev_hash: String,
    pub(super) user: String,
    pub(super) assistant: String,
    pub(super) events: String,
    pub(super) phantom_reaches: String,
    pub(super) sources: String,
    pub(super) tokens_in: Option<i64>,
    pub(super) tokens_out: Option<i64>,
    pub(super) ts_claim: i64,
    /// Which canonical encoding hashed this row (`turns.encoding_version`,
    /// review NIT N1 on #261). v1 or v2.
    pub(super) encoding_version: i64,
}

impl TurnRow {
    /// BLAKE3 hex of this row's canonical encoding — what the *next* turn's
    /// `prev_hash` must equal. Dispatches on the row's recorded
    /// `encoding_version`; a version this build does not understand errors
    /// clearly instead of hashing under the wrong rules (NIT N1 on #261).
    pub(super) fn content_hash(&self) -> anyhow::Result<String> {
        match self.encoding_version {
            1 => Ok(blake3::hash(&self.canonical_encoding_v1())
                .to_hex()
                .to_string()),
            2 => Ok(blake3::hash(&self.canonical_encoding_v2())
                .to_hex()
                .to_string()),
            other => anyhow::bail!(
                "turn (conversation `{}`, writer {}, seq {}) carries encoding_version {other}, \
                 which this newt does not understand (known: 1, 2) — upgrade newt to verify \
                 or extend this chain",
                self.conversation_id,
                self.writer_fingerprint,
                self.seq
            ),
        }
    }

    /// The CONTENT ID (#1786 spec §2): BLAKE3 over the turn's stored content
    /// bytes — user, assistant, events, phantom_reaches, sources — and none
    /// of its chain-position fields (conversation, writer, seq, prev_hash,
    /// ts_claim). It survives re-chaining, import, and export, which is what
    /// provenance citations need; the chain hash pins position separately.
    ///
    /// `sources` is IN the preimage on purpose: it makes an id determine its
    /// outgoing provenance edges (two derivations with identical text but
    /// different sources get different ids), and it makes self- or mutual
    /// citation require a BLAKE3 fixed point — computationally
    /// unconstructible. Witnessed rows with identical content share an id,
    /// which is harmless: they have no outgoing edges.
    ///
    /// Version-agnostic: computable for v1 rows too (their backfilled
    /// `'[]'` columns are identity-bearing bytes here even though the v1
    /// chain arm never reads them — an SQL edit to a v1 row's dead columns
    /// breaks citations of it LOUDLY, as an orphan, rather than silently).
    pub(super) fn content_id(&self) -> String {
        turn_content_id(
            &self.user,
            &self.assistant,
            &self.events,
            &self.phantom_reaches,
            &self.sources,
        )
    }

    /// Canonical v1 byte encoding of a turn: version tag, then every field
    /// length-prefixed (u64 LE) so adjacent fields can never be reparsed
    /// ambiguously (`("ab","c")` ≠ `("a","bc")`). Integers are 8-byte LE
    /// with a presence byte for the optional token counts.
    pub(super) fn canonical_encoding_v1(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            64 + self.conversation_id.len()
                + self.writer_fingerprint.len()
                + self.prev_hash.len()
                + self.user.len()
                + self.assistant.len()
                + self.events.len(),
        );
        out.extend_from_slice(TURN_ENCODING_V1_PREFIX);
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

    /// Canonical v2 byte encoding (#1786 spec §2.1): the v1 spine with
    /// `phantom_reaches` and `sources` appended to the len-prefixed group.
    /// Same length-prefix discipline (`("ab","c")` ≠ `("a","bc")`), same
    /// domain separation via the version tag. The two new fields are the
    /// STORED column bytes (§2.1's hash-the-stored-bytes rule).
    pub(super) fn canonical_encoding_v2(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            64 + self.conversation_id.len()
                + self.writer_fingerprint.len()
                + self.prev_hash.len()
                + self.user.len()
                + self.assistant.len()
                + self.events.len()
                + self.phantom_reaches.len()
                + self.sources.len(),
        );
        out.extend_from_slice(TURN_ENCODING_V2_PREFIX);
        for field in [
            self.conversation_id.as_bytes(),
            self.writer_fingerprint.as_bytes(),
            self.prev_hash.as_bytes(),
            self.user.as_bytes(),
            self.assistant.as_bytes(),
            self.events.as_bytes(),
            self.phantom_reaches.as_bytes(),
            self.sources.as_bytes(),
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

/// The content id as a free function, over the five stored-byte fields —
/// callable where no [`TurnRow`] exists (record materialization computes ids
/// for [`crate::ConversationRecord`] turns from the same stored columns).
/// See [`TurnRow::content_id`] for the semantics.
pub(super) fn turn_content_id(
    user: &str,
    assistant: &str,
    events: &str,
    phantom_reaches: &str,
    sources: &str,
) -> String {
    let mut buf = Vec::with_capacity(
        32 + user.len() + assistant.len() + events.len() + phantom_reaches.len() + sources.len(),
    );
    buf.extend_from_slice(TURN_CONTENT_ID_PREFIX);
    for field in [
        user.as_bytes(),
        assistant.as_bytes(),
        events.as_bytes(),
        phantom_reaches.as_bytes(),
        sources.as_bytes(),
    ] {
        buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
        buf.extend_from_slice(field);
    }
    blake3::hash(&buf).to_hex().to_string()
}

/// Column order contract: every SELECT feeding this function lists
/// `conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
/// events, tokens_in, tokens_out, ts_claim, encoding_version,
/// phantom_reaches, sources` — the raw stored bytes of the last two ride
/// along because v2 hashes them (hash-the-stored-bytes, #1786 §2.1).
pub(super) fn turn_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnRow> {
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
        encoding_version: row.get(10)?,
        phantom_reaches: row.get(11)?,
        sources: row.get(12)?,
    })
}

/// Insert one fully-populated turn row. Must run inside the caller's
/// transaction (shared by the live append path and the one-time import).
///
/// `phantom_reaches` moved INTO `TurnRow` at the v2 bump (#1786): v2 hashes
/// it, so the struct now carries the exact bytes the encoding covers.
pub(super) fn insert_turn_row(conn: &Connection, row: &TurnRow) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO turns
           (conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
            events, tokens_in, tokens_out, ts_claim, encoding_version,
            phantom_reaches, sources)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
            row.encoding_version,
            row.phantom_reaches,
            row.sources,
        ],
    )?;
    Ok(())
}

/// The §6 genesis hash anchoring a writer's chain within a conversation.
pub(super) fn genesis_hash(conversation_id: &str, writer_fingerprint: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(GENESIS_PREFIX);
    for field in [conversation_id.as_bytes(), writer_fingerprint.as_bytes()] {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    hasher.finalize().to_hex().to_string()
}

/// This writer's most recent turn in a conversation (chain tip source).
pub(super) fn last_turn(
    conn: &Connection,
    conversation_id: &str,
    writer_fingerprint: &str,
) -> anyhow::Result<Option<TurnRow>> {
    Ok(conn
        .query_row(
            "SELECT conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                    events, tokens_in, tokens_out, ts_claim, encoding_version,
                    phantom_reaches, sources
               FROM turns
              WHERE conversation_id = ?1 AND writer_fingerprint = ?2
              ORDER BY seq DESC
              LIMIT 1",
            rusqlite::params![conversation_id, writer_fingerprint],
            turn_row_from_sql,
        )
        .optional()?)
}

/// One writer's turn at an exact seq — the row a per-writer witness pins
/// (#1786 §5). `None` when no such turn exists.
pub(super) fn turn_at_seq(
    conn: &Connection,
    conversation_id: &str,
    writer_fingerprint: &str,
    seq: i64,
) -> anyhow::Result<Option<TurnRow>> {
    Ok(conn
        .query_row(
            "SELECT conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                    events, tokens_in, tokens_out, ts_claim, encoding_version,
                    phantom_reaches, sources
               FROM turns
              WHERE conversation_id = ?1 AND writer_fingerprint = ?2 AND seq = ?3",
            rusqlite::params![conversation_id, writer_fingerprint, seq],
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
pub(super) fn next_tick(conn: &Connection, writer_fingerprint: &str) -> anyhow::Result<i64> {
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
                 -- A prompt is allocated before its turn and survives when
                 -- that turn fails. The clock must never reuse its sequence
                 -- after a lost/recreated writer_clock row.
                 SELECT MAX(seq) AS t FROM prompt_receipts
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

impl ConversationStore {
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
    pub(super) fn check_writer_tip_witness(
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
    pub(super) fn check_tip_witness(
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clone_row(row: &TurnRow) -> TurnRow {
        TurnRow {
            conversation_id: row.conversation_id.clone(),
            writer_fingerprint: row.writer_fingerprint.clone(),
            seq: row.seq,
            prev_hash: row.prev_hash.clone(),
            user: row.user.clone(),
            assistant: row.assistant.clone(),
            events: row.events.clone(),
            phantom_reaches: row.phantom_reaches.clone(),
            sources: row.sources.clone(),
            tokens_in: row.tokens_in,
            tokens_out: row.tokens_out,
            ts_claim: row.ts_claim,
            encoding_version: row.encoding_version,
        }
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
            phantom_reaches: "[]".into(),
            sources: "[]".into(),
            tokens_in: None,
            tokens_out: None,
            ts_claim: 7,
            encoding_version: 1,
        };
        let shifted = TurnRow {
            user: "a".into(),
            assistant: "bc".into(),
            ..clone_row(&base)
        };
        assert_ne!(
            base.canonical_encoding_v1(),
            shifted.canonical_encoding_v1(),
            "length prefixes must prevent (ab,c) == (a,bc)"
        );
        // Every field participates in the hash — including the claims and
        // token counts, which makes even display fields tamper-evident.
        let skewed = TurnRow {
            ts_claim: 8,
            ..clone_row(&base)
        };
        assert_ne!(base.content_hash().unwrap(), skewed.content_hash().unwrap());
        let tokens = TurnRow {
            tokens_in: Some(5),
            ..clone_row(&base)
        };
        assert_ne!(base.content_hash().unwrap(), tokens.content_hash().unwrap());
    }

    /// Pinned known-answer vectors (#1786 spec §10.2 — "a repair, not an
    /// addition"): before this test, NO fixed input→hash vectors existed for
    /// the canonical encoding — only relative assertions, which all keep
    /// passing if the encoding drifts in a way that changes every hash
    /// uniformly. These bytes are the compatibility contract: every v1 row
    /// ever written must verify under exactly these rules FOREVER, so this
    /// test failing means stored conversations stop verifying. It must never
    /// be "fixed" by updating the constants; a mismatch means the encoding
    /// changed, which for a shipped version is a data-loss bug.
    ///
    /// Minting provenance: hex captured from this codebase at the commit
    /// introducing the test, before any v2 work touched this file.
    #[test]
    fn v1_known_answer_vectors() {
        let row = TurnRow {
            conversation_id: "kat-conv".into(),
            writer_fingerprint: "kat-writer".into(),
            seq: 3,
            prev_hash: "kat-prev".into(),
            user: "kat user".into(),
            assistant: "kat assistant".into(),
            events: "[]".into(),
            phantom_reaches: "[]".into(),
            sources: "[]".into(),
            tokens_in: Some(11),
            tokens_out: None,
            ts_claim: 1234567890,
            encoding_version: 1,
        };
        assert_eq!(
            row.content_hash().unwrap(),
            "a2166c898d5f471dab2c2cee07d85d192de3a8756d7a404b54dae49168f98a3f",
            "the v1 canonical encoding changed — every stored v1 row would stop verifying"
        );
        assert_eq!(
            genesis_hash("kat-conv", "kat-writer"),
            "054ea5340e2930244bf81b23aedd809870e52a2ceb5cc6f4c42777fa5c9e92e8",
            "the genesis hash changed — every stored chain root would stop verifying"
        );
    }

    /// Pinned v2 + content-id vectors (#1786 spec §10.2), same contract and
    /// same rules as the v1 vectors above: these bytes are permanent. The
    /// content-id vector doubles as the cross-store portability pin — any
    /// implementation (an importer, a mesh peer, agent-frame) that computes
    /// content ids must reproduce this exact hex from these exact inputs.
    #[test]
    fn v2_and_content_id_known_answer_vectors() {
        let row = TurnRow {
            conversation_id: "kat-conv".into(),
            writer_fingerprint: "kat-writer".into(),
            seq: 3,
            prev_hash: "kat-prev".into(),
            user: "kat user".into(),
            assistant: "kat assistant".into(),
            events: "[]".into(),
            phantom_reaches: "[]".into(),
            sources: "[]".into(),
            tokens_in: Some(11),
            tokens_out: None,
            ts_claim: 1234567890,
            encoding_version: 2,
        };
        assert_eq!(
            row.content_hash().unwrap(),
            "5c0f48e872ee477620df1fe8b3f764e2abe446547303de9748a5e60652cbb789",
            "the v2 canonical encoding changed — every stored v2 row would stop verifying"
        );
        assert_eq!(
            row.content_id(),
            "e8974dcd81f696582b2b693570ec4fb7c0935703c4035eaecb2d934f3643f8fc",
            "the content id changed — every recorded citation would orphan"
        );
        // The v1 arm of the SAME bytes must differ from v2 (domain
        // separation), and the content id must differ from both (its own
        // domain): three digests, three values.
        let v1 = TurnRow {
            encoding_version: 1,
            ..clone_row(&row)
        };
        let v1_hash = v1.content_hash().unwrap();
        assert_ne!(v1_hash, row.content_hash().unwrap());
        assert_ne!(row.content_id(), v1_hash);
        assert_ne!(row.content_id(), row.content_hash().unwrap());
        // Position-freedom: re-chaining (new conversation, writer, seq,
        // prev, ts) preserves the content id — the property citations need
        // and chain hashes cannot give.
        let rechained = TurnRow {
            conversation_id: "elsewhere".into(),
            writer_fingerprint: "other-writer".into(),
            seq: 999,
            prev_hash: "other-prev".into(),
            ts_claim: 1,
            ..clone_row(&row)
        };
        assert_eq!(rechained.content_id(), row.content_id());
        assert_ne!(
            rechained.content_hash().unwrap(),
            row.content_hash().unwrap()
        );
        // Sources are in the content-id preimage: same text, different
        // derivation, different identity — an id determines its edges.
        let derived = TurnRow {
            sources: "[\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"]"
                .into(),
            ..clone_row(&row)
        };
        assert_ne!(derived.content_id(), row.content_id());
    }

    /// N1 (#261): `content_hash` dispatches on the row's recorded encoding
    /// version — v1 hashes, anything else errors clearly rather than hashing
    /// under the wrong rules.
    #[test]
    fn content_hash_rejects_unknown_encoding_versions() {
        let v1 = TurnRow {
            conversation_id: "c".into(),
            writer_fingerprint: "w".into(),
            seq: 1,
            prev_hash: "p".into(),
            user: "u".into(),
            assistant: "a".into(),
            events: "[]".into(),
            phantom_reaches: "[]".into(),
            sources: "[]".into(),
            tokens_in: None,
            tokens_out: None,
            ts_claim: 7,
            encoding_version: 1,
        };
        v1.content_hash().expect("v1 must hash");

        // The future vector was 2 until #1786 consumed it; re-pinned at 3
        // (spec §9.1) so this test keeps guarding the dispatch's refusal arm.
        let future = TurnRow {
            encoding_version: 3,
            ..clone_row(&v1)
        };
        let err = future.content_hash().unwrap_err().to_string();
        assert!(
            err.contains("encoding_version 3") && err.contains("known: 1, 2"),
            "unknown version must error clearly: {err}"
        );
    }

    #[test]
    fn genesis_hash_is_deterministic_and_writer_scoped() {
        assert_eq!(genesis_hash("conv", "w1"), genesis_hash("conv", "w1"));
        assert_ne!(genesis_hash("conv", "w1"), genesis_hash("conv", "w2"));
        assert_ne!(genesis_hash("conv", "w1"), genesis_hash("other", "w1"));
        // Length-prefixing: ("ab","c") must not collide with ("a","bc").
        assert_ne!(genesis_hash("ab", "c"), genesis_hash("a", "bc"));
    }
}
