use super::*;

// =========================================================================
// #1786 Phase B — per-writer tip witnesses (spec §5)
// =========================================================================

/// Mint a SECOND writer on the same store root: the fingerprint comes from
/// `<root>/install-nonce`, so replacing it and reopening is the documented
/// fingerprint-upgrade / writer-handoff shape (§6 module docs).
fn reopen_as_new_writer(root: &std::path::Path, workspace: &std::path::Path) -> ConversationStore {
    std::fs::write(root.join("install-nonce"), "a-different-install\n").unwrap();
    ConversationStore::new(root, workspace, 100).unwrap()
}

/// #1786 §5, the #1794-residual-2 scenario AT READ: in a multi-writer
/// conversation, the NON-tip writer's final turn was previously pinned by
/// nothing — no successor links to it and the conversations-row witness
/// follows the recorded tip writer. The per-writer witness closes it.
#[test]
fn non_tip_writers_final_turn_tamper_is_caught_at_read() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store_w = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store_w.create("handoff", None).unwrap();
    store_w.append_turn(&id, "w asks", "w answer").unwrap();
    let old_writer = store_w.writer_fingerprint().to_string();
    drop(store_w);

    // Handoff: a new fingerprint takes over and appends.
    let store_x = reopen_as_new_writer(root.path(), workspace.path());
    assert_ne!(store_x.writer_fingerprint(), old_writer);
    store_x.append_turn(&id, "x asks", "x answer").unwrap();
    store_x
        .verify_chain(&id)
        .expect("the untampered handoff conversation must verify");

    // Tamper the OLD writer's final (and only) turn — the row the
    // conversations-row witness no longer covers.
    raw(root.path())
        .execute(
            "UPDATE turns SET assistant = 'forged' WHERE conversation_id = ?1
               AND writer_fingerprint = ?2",
            rusqlite::params![&id, &old_writer],
        )
        .unwrap();
    let err = store_x.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation") && err.contains("witness"),
        "the non-tip writer's final turn must be pinned by ITS witness: {err}"
    );
}

/// #1786 §5 step 1, the laundering path AT WRITE: without the own-witness
/// check, the tampered final turn above would be silently adopted as the
/// chain tip by that writer's next append — and the same transaction's
/// upsert would overwrite the only evidence. The append must refuse instead.
#[test]
fn own_next_append_refuses_to_launder_a_tampered_final_turn() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("laundering", None).unwrap();
    store.append_turn(&id, "one", "1").unwrap();
    store.append_turn(&id, "two", "2").unwrap();

    // Tamper this writer's final turn AND repair the conversations-row tip
    // to match, modelling an attacker who knows about the old single
    // witness. Only writer_tips still disagrees.
    let conn = raw(root.path());
    conn.execute(
        "UPDATE turns SET assistant = 'forged' WHERE conversation_id = ?1
           AND seq = (SELECT MAX(seq) FROM turns WHERE conversation_id = ?1)",
        rusqlite::params![&id],
    )
    .unwrap();
    // Recompute what the forged row hashes to, byte-for-byte as the store
    // would, and plant it as the conversations tip.
    let forged_tip: String = {
        let mut stmt = conn
            .prepare(
                "SELECT conversation_id, writer_fingerprint, seq, prev_hash, user, assistant,
                        events, phantom_reaches, sources, tokens_in, tokens_out, ts_claim
                   FROM turns WHERE conversation_id = ?1
                  ORDER BY seq DESC LIMIT 1",
            )
            .unwrap();
        stmt.query_row(rusqlite::params![&id], |row| {
            let conv: String = row.get(0)?;
            let writer: String = row.get(1)?;
            let seq: i64 = row.get(2)?;
            let prev: String = row.get(3)?;
            let user: String = row.get(4)?;
            let assistant: String = row.get(5)?;
            let events: String = row.get(6)?;
            let reaches: String = row.get(7)?;
            let sources: String = row.get(8)?;
            let tin: Option<i64> = row.get(9)?;
            let tout: Option<i64> = row.get(10)?;
            let ts: i64 = row.get(11)?;
            let mut buf = Vec::new();
            buf.extend_from_slice(b"newt-turn:v2");
            for f in [
                &conv, &writer, &prev, &user, &assistant, &events, &reaches, &sources,
            ] {
                buf.extend_from_slice(&(f.len() as u64).to_le_bytes());
                buf.extend_from_slice(f.as_bytes());
            }
            buf.extend_from_slice(&seq.to_le_bytes());
            for opt in [tin, tout] {
                match opt {
                    Some(v) => {
                        buf.push(1);
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                    None => buf.push(0),
                }
            }
            buf.extend_from_slice(&ts.to_le_bytes());
            Ok(blake3::hash(&buf).to_hex().to_string())
        })
        .unwrap()
    };
    conn.execute(
        "UPDATE conversations SET tip_hash = ?2 WHERE id = ?1",
        rusqlite::params![&id, forged_tip],
    )
    .unwrap();

    let err = store
        .append_turn(&id, "three", "3")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("own recorded witness"),
        "the writer's own witness must catch what the repaired conversations \
         tip no longer can: {err}"
    );

    // Evidence preserved: writer_tips still holds the pre-tamper hash.
    let rec = store.load(&id).unwrap();
    assert_eq!(rec.turns.len(), 2, "the refused turn must not have landed");
}

/// #1786 §5: a STALE witness (tip_seq below the writer's final seq — the
/// rollback residue) is verified against the row it actually pins and is
/// NOT a violation; the writer's next append repairs it to the tip.
#[test]
fn stale_witness_is_accepted_and_repaired_by_the_next_append() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("rollback", None).unwrap();
    store.append_turn(&id, "one", "1").unwrap();
    let conn = raw(root.path());
    let (h1, s1): (String, i64) = conn
        .query_row(
            "SELECT tip_hash, tip_seq FROM writer_tips WHERE conversation_id = ?1",
            rusqlite::params![&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    store.append_turn(&id, "two", "2").unwrap();

    // Model the rolled-back binary: writer_tips frozen at turn one.
    conn.execute(
        "UPDATE writer_tips SET tip_hash = ?2, tip_seq = ?3 WHERE conversation_id = ?1",
        rusqlite::params![&id, h1, s1],
    )
    .unwrap();
    store
        .verify_chain(&id)
        .expect("a stale witness is the honest rollback residue, not a violation");

    // The next append repairs it.
    store.append_turn(&id, "three", "3").unwrap();
    let (_, seq_now): (String, i64) = conn
        .query_row(
            "SELECT tip_hash, tip_seq FROM writer_tips WHERE conversation_id = ?1",
            rusqlite::params![&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let max_seq: i64 = conn
        .query_row(
            "SELECT MAX(seq) FROM turns WHERE conversation_id = ?1",
            rusqlite::params![&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        seq_now, max_seq,
        "the append must repair the witness to the tip"
    );
    store.verify_chain(&id).unwrap();

    // But a STALE witness whose hash disagrees with the row it pins IS a
    // violation — stale means old, not wrong.
    conn.execute(
        "UPDATE writer_tips SET tip_hash = ?2, tip_seq = ?3 WHERE conversation_id = ?1",
        rusqlite::params![&id, "0".repeat(64), s1],
    )
    .unwrap();
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(err.contains("chain violation"), "{err}");
}

/// #1786 §5: a witness pinning a seq PAST the writer's last turn means rows
/// were deleted out from under it — violation from the PER-WRITER arm.
///
/// Mechanism-pinned (the #1792 lesson, re-learned here by a red-first drill:
/// the single-writer version of this test stayed green with the per-writer
/// loop gutted, because the conversations-row witness catches that shape too
/// and its message also says "deleted"). So: the deleted final turn belongs
/// to the NON-tip writer, whom the conversations-row witness does not cover
/// — only `writer_tips.tip_seq > final_seq` can see it — and the assertion
/// pins that arm's specific diagnosis.
#[test]
fn witness_past_the_final_turn_refuses() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store_w = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store_w.create("deletion", None).unwrap();
    store_w.append_turn(&id, "w one", "1").unwrap();
    store_w.append_turn(&id, "w two", "2").unwrap();
    let old_writer = store_w.writer_fingerprint().to_string();
    drop(store_w);
    let store_x = reopen_as_new_writer(root.path(), workspace.path());
    store_x.append_turn(&id, "x one", "1").unwrap();

    let conn = raw(root.path());
    conn.execute(
        "DELETE FROM turns WHERE conversation_id = ?1 AND writer_fingerprint = ?2
           AND seq = (SELECT MAX(seq) FROM turns
                       WHERE conversation_id = ?1 AND writer_fingerprint = ?2)",
        rusqlite::params![&id, &old_writer],
    )
    .unwrap();
    let err = store_x.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation") && err.contains("past its last recorded turn"),
        "the non-tip writer's trailing-row deletion must be caught BY ITS \
         WITNESS's seq arm: {err}"
    );
}

/// #1786 §5 step 3 — HANDOFF RELOCATION: the first append by a new writer
/// verifies the outgoing writer's conversations-row witness and, when that
/// writer has no per-writer row (the migration shape), copies the CHECKED
/// witness down before overwriting it. Without this, the handoff destroys
/// the only witness pinning the outgoing writer's final turn.
#[test]
fn handoff_relocates_the_outgoing_writers_witness() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store_w = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store_w.create("relocation", None).unwrap();
    store_w.append_turn(&id, "w asks", "w answer").unwrap();
    let old_writer = store_w.writer_fingerprint().to_string();
    drop(store_w);

    // Model the pre-B migration state: the outgoing writer has NO
    // per-writer row (only the conversations-row witness pins its final
    // turn).
    let conn = raw(root.path());
    conn.execute(
        "DELETE FROM writer_tips WHERE conversation_id = ?1",
        rusqlite::params![&id],
    )
    .unwrap();

    let store_x = reopen_as_new_writer(root.path(), workspace.path());
    store_x.append_turn(&id, "x asks", "x answer").unwrap();

    // The outgoing writer's witness must have been relocated, not destroyed.
    let relocated: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM writer_tips WHERE conversation_id = ?1
               AND writer_fingerprint = ?2",
            rusqlite::params![&id, &old_writer],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        relocated, 1,
        "the handoff must relocate the checked witness"
    );

    // And it still has teeth: tamper the old writer's final turn.
    conn.execute(
        "UPDATE turns SET assistant = 'forged' WHERE conversation_id = ?1
           AND writer_fingerprint = ?2",
        rusqlite::params![&id, &old_writer],
    )
    .unwrap();
    let err = store_x.verify_chain(&id).unwrap_err().to_string();
    assert!(err.contains("chain violation"), "{err}");
}

/// #1786 §5: the import writes witnesses, and a fingerprint change after
/// import does not reopen the final-turn hole (the r2-review scenario).
#[test]
fn import_writes_witnesses_that_survive_a_fingerprint_change() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let record = legacy_record(
        "1000-conv-import-w",
        "imported",
        workspace.path(),
        &[("old ask", "old answer")],
        100,
        500,
    );
    write_legacy_record(root.path(), &record);
    let store_w = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let importing_writer = store_w.writer_fingerprint().to_string();
    let conn = raw(root.path());
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM writer_tips WHERE conversation_id = '1000-conv-import-w'
               AND writer_fingerprint = ?1",
            rusqlite::params![&importing_writer],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "the import must witness what it writes");
    drop(store_w);

    // Fingerprint change, new writer extends, then the imported writer's
    // final turn is tampered — its own witness must still catch it.
    let store_x = reopen_as_new_writer(root.path(), workspace.path());
    store_x
        .append_turn("1000-conv-import-w", "new", "turn")
        .unwrap();
    store_x.verify_chain("1000-conv-import-w").unwrap();
    conn.execute(
        "UPDATE turns SET assistant = 'forged' WHERE conversation_id = '1000-conv-import-w'
           AND writer_fingerprint = ?1",
        rusqlite::params![&importing_writer],
    )
    .unwrap();
    let err = store_x
        .verify_chain("1000-conv-import-w")
        .unwrap_err()
        .to_string();
    assert!(err.contains("chain violation"), "{err}");
}

/// #1786 §5: a planted witness for a writer with no turns refuses — with
/// the genesis-witness exception for the zero-turn create/import shape.
#[test]
fn witness_for_a_writer_with_no_turns_refuses_unless_genesis() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("planted", None).unwrap();
    store.append_turn(&id, "one", "1").unwrap();
    let conn = raw(root.path());
    conn.execute(
        "INSERT INTO writer_tips (conversation_id, writer_fingerprint, tip_hash, tip_seq)
         VALUES (?1, 'ghost-writer', ?2, 7)",
        rusqlite::params![&id, "b".repeat(64)],
    )
    .unwrap();
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation") && err.contains("ghost-writer"),
        "a planted witness must refuse naming the writer: {err}"
    );
}

/// #1786 §5 steps 1+3 in COMPOSITION (r3 blocker): a stale-but-honest
/// witness meeting a writer handoff.
///
/// Each mechanism is sound alone and each has its own test, but they cover
/// disjoint states: `handoff_relocates_the_outgoing_writers_witness` deletes
/// `writer_tips` entirely (the migration shape, so the INSERT has no
/// conflict), and `stale_witness_is_accepted_and_repaired_by_the_next_append`
/// never hands off (so the SAME writer's next append repairs it). Compose
/// them and the outgoing writer never appends again, so "repaired by the
/// next append" never arrives:
///
/// ```text
/// A5 by current binary        writer_tips[A] = (hash(A5), 5)
/// A6 by rolled-back binary    conversations.tip = hash(A6); writer_tips[A] untouched
/// B  hands off                relocation INSERT hits ON CONFLICT -> DO NOTHING
///                             conversations.tip -> B, writer_tips[A] still seq 5
/// ```
///
/// A6 is A's final turn: no successor links to it, the conversations-row
/// witness has moved to B, and A's own witness pins seq 5. It is pinned by
/// nothing — precisely the #1794 residual 2 this phase exists to close.
///
/// The handoff already VERIFIED the conversation-level witness against A6,
/// so advancing A's witness here is relocation of checked evidence, not
/// backfill from the rows it is meant to protect.
#[test]
fn stale_writer_witness_is_advanced_during_handoff() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store_a.create("handoff-stale", None).unwrap();
    store_a.append_turn(&id, "a one", "1").unwrap();
    let writer_a = store_a.writer_fingerprint().to_string();

    let conn = raw(root.path());
    let (h1, s1): (String, i64) = conn
        .query_row(
            "SELECT tip_hash, tip_seq FROM writer_tips
              WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
            rusqlite::params![&id, &writer_a],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    // A's real final turn, then the rolled-back binary's residue: the turn
    // and the conversations-row tip advanced, `writer_tips` did not.
    store_a.append_turn(&id, "a two", "2").unwrap();
    conn.execute(
        "UPDATE writer_tips SET tip_hash = ?3, tip_seq = ?4
          WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
        rusqlite::params![&id, &writer_a, &h1, s1],
    )
    .unwrap();
    drop(store_a);

    // Handoff under a current binary.
    let store_b = reopen_as_new_writer(root.path(), workspace.path());
    store_b.append_turn(&id, "b one", "1").unwrap();

    let a_final_seq: i64 = conn
        .query_row(
            "SELECT MAX(seq) FROM turns WHERE conversation_id = ?1
               AND writer_fingerprint = ?2",
            rusqlite::params![&id, &writer_a],
            |row| row.get(0),
        )
        .unwrap();
    let (_, witness_seq): (String, i64) = conn
        .query_row(
            "SELECT tip_hash, tip_seq FROM writer_tips
              WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
            rusqlite::params![&id, &writer_a],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        witness_seq, a_final_seq,
        "the handoff verified the conversation witness against A's final turn, \
         so it must advance A's stale witness to that already-checked tip — \
         otherwise A's final turn is pinned by nothing after the tip moves to B"
    );

    // The advanced witness must have teeth: mutate A's final turn.
    conn.execute(
        "UPDATE turns SET assistant = 'forged' WHERE conversation_id = ?1
           AND writer_fingerprint = ?2 AND seq = ?3",
        rusqlite::params![&id, &writer_a, a_final_seq],
    )
    .unwrap();
    let err = store_b.load_verified(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation"),
        "a mutation of A's final turn must be caught after handoff: {err}"
    );
}

/// #1786 §5 step 3 (r3 blocker, second half): the handoff must CHECK the
/// outgoing writer's existing witness before it advances it, and a bad one
/// must fail the append closed with the evidence left exactly as found.
///
/// The advance added for `stale_writer_witness_is_advanced_during_handoff`
/// is the hazard this pins: an unconditional "advance the stale witness to
/// the verified tip" would overwrite a TAMPERED witness with a good-looking
/// one, destroying the only record that anything was wrong — laundering, and
/// the exact failure the own-witness check exists to prevent on the
/// appending side. Relocation is for CHECKED evidence only.
#[test]
fn corrupt_stale_writer_witness_is_not_laundered_during_handoff() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store_a = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store_a.create("handoff-corrupt", None).unwrap();
    store_a.append_turn(&id, "a one", "1").unwrap();
    let writer_a = store_a.writer_fingerprint().to_string();

    let conn = raw(root.path());
    let stale_seq: i64 = conn
        .query_row(
            "SELECT tip_seq FROM writer_tips
              WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
            rusqlite::params![&id, &writer_a],
            |row| row.get(0),
        )
        .unwrap();

    // A's real final turn, then a CORRUPT witness frozen at the stale seq:
    // the shape a rolled-back binary leaves, with the hash tampered.
    store_a.append_turn(&id, "a two", "2").unwrap();
    let forged = "0".repeat(64);
    conn.execute(
        "UPDATE writer_tips SET tip_hash = ?3, tip_seq = ?4
          WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
        rusqlite::params![&id, &writer_a, &forged, stale_seq],
    )
    .unwrap();
    drop(store_a);

    let store_b = reopen_as_new_writer(root.path(), workspace.path());
    let err = store_b
        .append_turn(&id, "b one", "1")
        .expect_err("a handoff must not chain past an unconfirmable outgoing witness")
        .to_string();
    assert!(
        err.contains("outgoing writer's own recorded witness could not be confirmed"),
        "the refusal must name the outgoing witness as the reason: {err}"
    );

    // The bad evidence is left exactly as found — not advanced, not repaired.
    let (hash_now, seq_now): (String, i64) = conn
        .query_row(
            "SELECT tip_hash, tip_seq FROM writer_tips
              WHERE conversation_id = ?1 AND writer_fingerprint = ?2",
            rusqlite::params![&id, &writer_a],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        hash_now, forged,
        "the tampered witness must not be laundered"
    );
    assert_eq!(
        seq_now, stale_seq,
        "the tampered witness must not be advanced"
    );
}
