//! §6 turn-chain primitives: the canonical byte encoding, content/genesis
//! hashing, the per-writer Lamport clock, and the raw `turns` row I/O — carved
//! from `store.rs` (kernel-first decomposition, handoff §D; follows the
//! `fts.rs` sibling precedent). Everything here is `pub(super)`: the public
//! seam is `ConversationStore`, unchanged.
//!
//! The byte-format regression tests live HERE, beside the encoding they pin —
//! a change to the canonical encoding must fail in this file.

use rusqlite::{Connection, OptionalExtension};

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

/// Domain-separation prefix for a CONTEXT WINDOW manifest id (#1786 §5b) —
/// a third domain, distinct from both chain encodings and the turn content
/// id, so a window id can never be mistaken for (or collide with) a turn's.
pub(super) const WINDOW_ID_PREFIX: &[u8] = b"newt-window:v1";

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

/// The self-certifying id of a context-window manifest (#1786 §5b): BLAKE3
/// over the manifest's own fields, so a stored manifest can be checked
/// against its recorded id without trusting anything else.
///
/// `conversation_id` is IN the preimage even though it is also the table's
/// key: without it a manifest could be moved between conversations and still
/// recompute — the same reason the turn chain hashes its conversation id.
/// `parent_id` is the empty string at a conversation's first seal.
pub(super) fn window_manifest_id(
    conversation_id: &str,
    parent_id: &str,
    summary_turn_id: &str,
    carried: &str,
    elided: &str,
    sealed_at_seq: i64,
) -> String {
    let mut buf = Vec::with_capacity(
        64 + conversation_id.len()
            + parent_id.len()
            + summary_turn_id.len()
            + carried.len()
            + elided.len(),
    );
    buf.extend_from_slice(WINDOW_ID_PREFIX);
    for field in [
        conversation_id.as_bytes(),
        parent_id.as_bytes(),
        summary_turn_id.as_bytes(),
        carried.as_bytes(),
        elided.as_bytes(),
    ] {
        buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
        buf.extend_from_slice(field);
    }
    buf.extend_from_slice(&sealed_at_seq.to_le_bytes());
    blake3::hash(&buf).to_hex().to_string()
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
