use super::*;

// =========================================================================
// #1786 — the v2 encoding: provenance sources, hashed reaches, mixed epochs
// =========================================================================

/// #1786 §3: a derived row round-trips through the public API and its
/// citation is protected — tampering the stored sources breaks the chain.
#[test]
fn sources_round_trip_and_are_chain_protected() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("provenance", None).unwrap();
    store.append_turn(&id, "u1", "a1").unwrap();
    store.append_turn(&id, "u2", "a2").unwrap();

    // Cite both witnessed turns by their content ids. The ids are computed
    // the way any independent implementation would (KAT-pinned encoding):
    // this test derives them via a raw read of the stored bytes.
    let conn = raw(root.path());
    let mut stmt = conn
        .prepare(
            "SELECT user, assistant, events, phantom_reaches, sources FROM turns
              WHERE conversation_id = ?1 ORDER BY seq ASC",
        )
        .unwrap();
    let ids: Vec<String> = stmt
        .query_map([&id], |row| {
            let (u, a, e, p, s): (String, String, String, String, String) = (
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            );
            let mut buf = Vec::new();
            buf.extend_from_slice(b"newt-turn-content:v1");
            for f in [&u, &a, &e, &p, &s] {
                buf.extend_from_slice(&(f.len() as u64).to_le_bytes());
                buf.extend_from_slice(f.as_bytes());
            }
            Ok(blake3::hash(&buf).to_hex().to_string())
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(ids.len(), 2);

    store
        .append_turn_full(&id, "", "summary of both", &[], &[], &ids, None, None)
        .unwrap();
    store
        .verify_chain(&id)
        .expect("a derived row citing real turns must verify");

    // The citation is INSIDE the hash: rewriting it breaks the chain.
    conn.execute(
        "UPDATE turns SET sources = '[]' WHERE conversation_id = ?1
           AND seq = (SELECT MAX(seq) FROM turns WHERE conversation_id = ?1)",
        rusqlite::params![&id],
    )
    .unwrap();
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation"),
        "erasing a derived row's citation must break the chain: {err}"
    );
}

/// #1786 §3: an orphan citation — a source id matching no turn in the
/// conversation — refuses, naming the citing row and the missing id.
/// Constructed out-of-band (the write path validates shape but defers
/// existence to verify, where late-arriving content is legal — §3's
/// verify-time semantics).
#[test]
fn orphan_source_refuses() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("provenance", None).unwrap();
    store.append_turn(&id, "u1", "a1").unwrap();
    let ghost = "f".repeat(64);
    store
        .append_turn_full(
            &id,
            "",
            "summary",
            &[],
            &[],
            std::slice::from_ref(&ghost),
            None,
            None,
        )
        .unwrap();
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation") && err.contains(&ghost),
        "an unattributable citation must refuse and name the missing id: {err}"
    );
}

/// #1786 §3: non-canonical sources bytes refuse. The write path cannot
/// produce them (it canonicalizes), so reaching this state took out-of-band
/// SQL — well-formed evidence or none. Exercised for unsorted order,
/// uppercase hex, and non-array JSON.
///
/// Each conversation holds ONE turn, so the tampered row is also the last:
/// the per-turn walk has no successor to check its link against, and
/// `verify_chain` runs the sources checks BEFORE the tip witness. The
/// canonical-form refusal is therefore the check that actually fires, and
/// the assertion pins that specific diagnosis rather than the bare "chain
/// violation" prefix every failure class shares — otherwise reordering the
/// provenance block below the tip witness would leave this test green while
/// testing something else entirely.
#[test]
fn non_canonical_sources_refuse() {
    for bad in [
        r#"["ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff","aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]"#,
        r#"["FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"]"#,
        r#"{"not":"an array"}"#,
    ] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let id = store.create("provenance", None).unwrap();
        store.append_turn(&id, "u1", "a1").unwrap();
        raw(root.path())
            .execute(
                "UPDATE turns SET sources = ?2 WHERE conversation_id = ?1",
                rusqlite::params![&id, bad],
            )
            .unwrap();
        let err = store.verify_chain(&id).unwrap_err().to_string();
        assert!(
            err.contains("chain violation") && err.contains("not in canonical form"),
            "sources bytes {bad:?} must refuse AS a canonical-form violation: {err}"
        );
    }
}

/// #1786 §3: the derived-row shape invariant — non-empty sources plus tool
/// activity on one row refuses (a derived row is harness-minted).
#[test]
fn derived_row_with_tool_activity_refuses() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("provenance", None).unwrap();
    store
        .append_turn_full(&id, "act", "acted", &sample_events(), &[], &[], None, None)
        .unwrap();
    // Graft a citation onto the evented row out-of-band.
    raw(root.path())
        .execute(
            "UPDATE turns SET sources = ?2 WHERE conversation_id = ?1",
            rusqlite::params![&id, format!("[\"{}\"]", "a".repeat(64))],
        )
        .unwrap();
    let err = store.verify_chain(&id).unwrap_err().to_string();
    assert!(
        err.contains("chain violation") && err.contains("AND tool activity"),
        "a row claiming derivation AND tool activity must refuse AS a shape \
         violation, not merely as some chain failure: {err}"
    );
}

/// #1786 §7 + §9.1: the mixed-epoch acceptance in its honest construction —
/// the legacy import writes v1-PINNED rows (deliberately not
/// TURN_ENCODING_VERSION_CURRENT, so a post-import rollback still verifies
/// the imported history), live appends extend the same conversation as v2,
/// and the whole mixed chain verifies. Also pins that the write path marks
/// new rows v2.
#[test]
fn legacy_import_pins_v1_and_mixed_epoch_verifies() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let record = legacy_record(
        "1000-conv-mixed",
        "old work",
        workspace.path(),
        &[("old ask", "old answer"), ("more", "done")],
        100,
        500,
    );
    write_legacy_record(root.path(), &record);

    // Opening imports; appending extends the imported chain under v2.
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    store
        .append_turn("1000-conv-mixed", "new ask", "new answer")
        .unwrap();
    store
        .verify_chain("1000-conv-mixed")
        .expect("a v1-imported chain extended by v2 rows must verify");

    let conn = raw(root.path());
    let versions: Vec<i64> = conn
        .prepare(
            "SELECT encoding_version FROM turns WHERE conversation_id = '1000-conv-mixed'
              ORDER BY seq ASC",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        versions,
        vec![1, 1, 2],
        "imported rows must be PINNED v1 (rollback still verifies them); \
         live appends must be v2"
    );

    // The mixed record materializes through the verified read too.
    let rec = store.load_verified("1000-conv-mixed").unwrap();
    assert_eq!(rec.turns.len(), 3);
}

/// #1786 §3: the derived-row shape invariant must fail closed on the WRITE
/// path too, not only at verification.
///
/// `verify_chain` refuses a row carrying both derivation (non-empty sources)
/// and tool activity, and `append_turn_full` validates the *shape of the ids*
/// — but it did not enforce the invariant itself. A caller passing both
/// therefore wrote a row that chained, witnessed, and committed, after which
/// `verify_chain` refuses that conversation forever and the stated policy is
/// "rows are left exactly as found" — no repair path. Phase C introduces
/// exactly the producer that could pass both.
///
/// A write path that admits what verification rejects manufactures
/// permanently unverifiable history, so the refusal belongs at the append.
#[test]
fn append_refuses_a_derived_row_that_also_claims_tool_activity() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
    let id = store.create("shape", None).unwrap();
    store.append_turn(&id, "u1", "a1").unwrap();

    let source = "a".repeat(64);
    let err = store
        .append_turn_full(
            &id,
            "",
            "summary",
            &sample_events(),
            &[],
            std::slice::from_ref(&source),
            None,
            None,
        )
        .expect_err("a row claiming derivation AND tool activity must be refused at append")
        .to_string();
    assert!(
        err.contains("derivation") && err.contains("tool activity"),
        "the refusal must name the shape violation: {err}"
    );

    // And the refusal left nothing behind: the conversation still verifies.
    store
        .verify_chain(&id)
        .expect("a refused append must not have written a bricking row");
}
