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

/// The turn encoding version this build writes, recorded per row in
/// `turns.encoding_version` (review NIT N1 on #261). [`TurnRow::content_hash`]
/// dispatches on the stored value; only v1 exists today, and a row carrying
/// an unknown version errors clearly instead of hashing garbage.
pub(super) const TURN_ENCODING_VERSION_CURRENT: i64 = 1;

/// Domain-separation prefix for the per-(conversation, writer) genesis hash.
pub(super) const GENESIS_PREFIX: &[u8] = b"newt-turn-chain-genesis:v1";

/// One turn row, exactly as stored. Internal: the canonical encoding hashes
/// every field, so this struct is the unit of chain verification.
#[derive(Debug)]
pub(super) struct TurnRow {
    pub(super) conversation_id: String,
    pub(super) writer_fingerprint: String,
    pub(super) seq: i64,
    pub(super) prev_hash: String,
    pub(super) user: String,
    pub(super) assistant: String,
    pub(super) events: String,
    pub(super) tokens_in: Option<i64>,
    pub(super) tokens_out: Option<i64>,
    pub(super) ts_claim: i64,
    /// Which canonical encoding hashed this row (`turns.encoding_version`,
    /// review NIT N1 on #261). Only v1 exists today.
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
            other => anyhow::bail!(
                "turn (conversation `{}`, writer {}, seq {}) carries encoding_version {other}, \
                 which this newt does not understand (known: 1) — upgrade newt to verify \
                 or extend this chain",
                self.conversation_id,
                self.writer_fingerprint,
                self.seq
            ),
        }
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
}

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
    })
}

/// Insert one fully-populated turn row. Must run inside the caller's
/// transaction (shared by the live append path and the one-time import).
///
/// `phantom_reaches_json` (#717) is a separate JSON-string argument, not a
/// `TurnRow` field, precisely because it is NOT part of the §6 canonical
/// encoding — keeping it out of `TurnRow` keeps the content hash untouched.
pub(super) fn insert_turn_row(
    conn: &Connection,
    row: &TurnRow,
    phantom_reaches_json: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO turns
           (conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
            events, tokens_in, tokens_out, ts_claim, encoding_version, phantom_reaches)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
            phantom_reaches_json,
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
                    events, tokens_in, tokens_out, ts_claim, encoding_version
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
            tokens_in: None,
            tokens_out: None,
            ts_claim: 7,
            encoding_version: 1,
        };
        v1.content_hash().expect("v1 must hash");

        let future = TurnRow {
            encoding_version: 2,
            ..clone_row(&v1)
        };
        let err = future.content_hash().unwrap_err().to_string();
        assert!(
            err.contains("encoding_version 2") && err.contains("known: 1"),
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
