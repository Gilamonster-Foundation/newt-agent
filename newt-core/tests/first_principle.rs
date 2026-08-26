//! **First-principle conformance suite: content-addressable data structures.**
//!
//! The stated principle this suite exists to enforce:
//!
//! > Content-addressable data structures are a first-principle guiding design
//! > philosophy. Any drift from tamper-resistant and provenance-traceable data
//! > is a drift away from first principles.
//!
//! (See `hartsock/content-addressable` for the principle in the abstract; this
//! file is its executable form for newt's durable conversation record.)
//!
//! # Why this suite exists separately from `tests/store.rs`
//!
//! `tests/store.rs` tests the store's *behaviour* — that `append_turn` appends,
//! that `verify_chain` catches a tampered row. Those tests pass today. They can
//! all pass while the principle is still violated, because a chain that is
//! written correctly, verified correctly *in tests*, and never verified in
//! production is behaviourally indistinguishable from one that is verified. A
//! behavioural suite structurally cannot see that difference. This suite can,
//! because some of its laws are about the code rather than the data.
//!
//! # The laws
//!
//! Each test below is one law, named for what it protects. Three shapes appear:
//!
//! * **Data laws** — exercise the store and assert a property of the record.
//! * **Wiring laws** — assert that a guarantee has a production caller. A
//!   guarantee nobody invokes is not a guarantee; see `evidence_unread` below.
//! * **Shape laws** — assert a type can express what the principle requires.
//!
//! # Known violations are recorded, not hidden
//!
//! A law newt does not yet satisfy is written as a real, failing assertion and
//! marked `#[ignore = "FIRST-PRINCIPLE VIOLATION — ..."]` so CI stays green
//! while the debt stays visible and executable. `violation_ledger_is_current`
//! is a ratchet: it counts those markers and fails if the count changes without
//! the constant being updated. You cannot quietly add a violation, and you
//! cannot quietly delete the marker for one instead of fixing it. Un-ignoring a
//! law is how a fix is proved: the test must go red first with the old code.

use newt_core::{ConversationStore, PhantomReach, PhantomResolution};

mod common;
use common::{cfg_is_test_only, for_each_production_line, production_roots};

/// Number of laws in this file currently marked as violations. This may only
/// go DOWN, and only in a change that also removes the corresponding
/// `#[ignore]` and shows the test passing. Raising it requires filing the
/// violation as an issue and naming it in the marker.
///
/// Trajectory, reconciled with the staged plan (#1785 → #1786 → #1787):
/// the suite landed at 3 (issue #1785's original acceptance text predates the
/// third law and said "lower to 1" — superseded by this note). #1785 took
/// 3 → 2. #1786 Phase A (the v2 encoding) hashed `phantom_reaches` and
/// landed `sources`, taking 2 → 1; Phase C (the producer plumbing) retires
/// the last law — `derived_records_name_their_sources`, strengthened per the
/// spec's §10.1 to drive the REAL compaction producer, never the Debug-grep
/// this file originally carried — taking 1 → 0. #1787 is downstream
/// diagnostics built on those fixes, not a law in this file.
const KNOWN_VIOLATIONS: usize = 1;

fn store(root: &std::path::Path, workspace: &std::path::Path) -> ConversationStore {
    ConversationStore::new(root, workspace, 100).unwrap()
}

// =========================================================================
// DATA LAWS — properties of the record itself
// =========================================================================

/// **Law: appending never alters what was already recorded.**
///
/// The no-editing-history rule. Every earlier turn must be byte-identical
/// after a later append; the past is a prefix of the future, never a thing
/// that gets revised in place.
///
/// A failure here means history is being rewritten, which invalidates every
/// downstream guarantee at once — the chain, the provenance, and any audit
/// built on either.
#[test]
fn append_preserves_prefix() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();

    let mut snapshots: Vec<Vec<(String, String)>> = Vec::new();
    for i in 0..5 {
        store
            .append_turn(&id, &format!("user {i}"), &format!("assistant {i}"))
            .unwrap();
        let rec = store.load(&id).unwrap();
        snapshots.push(
            rec.turns
                .iter()
                .map(|t| (t.user.clone(), t.assistant.clone()))
                .collect(),
        );
    }

    // Every snapshot must be a prefix of the one after it — no entry may
    // change value, and none may disappear.
    for w in snapshots.windows(2) {
        let (earlier, later) = (&w[0], &w[1]);
        assert!(
            later.len() > earlier.len(),
            "the log must grow by append: {} -> {}",
            earlier.len(),
            later.len()
        );
        assert_eq!(
            &later[..earlier.len()],
            &earlier[..],
            "appending rewrote an already-recorded turn — history is not append-only"
        );
    }
}

/// **Law: tampering with any recorded entry is detectable.**
///
/// The chain must actually work: alter one byte of one row and verification
/// must refuse. This is the guarantee the whole principle rests on, and it is
/// the one newt already implements correctly — recorded here so the suite
/// states the complete law rather than only the parts that are broken.
#[test]
fn tampering_is_detectable() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "user one", "assistant one").unwrap();
    store.append_turn(&id, "user two", "assistant two").unwrap();

    store
        .verify_chain(&id)
        .expect("an untampered chain must verify");

    // Reach past the API and alter a row in place, exactly as an attacker or
    // a careless migration would.
    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    let changed = conn
        .execute(
            "UPDATE turns SET user = 'tampered'
             WHERE conversation_id = ?1
               AND seq = (SELECT MIN(seq) FROM turns WHERE conversation_id = ?1)",
            rusqlite::params![&id],
        )
        .unwrap();
    assert_eq!(changed, 1, "the tamper must have actually landed");

    assert!(
        store.verify_chain(&id).is_err(),
        "a tampered row must break verification — otherwise the chain is decoration"
    );
}

/// **Law: everything in the record is covered by the chain.**
///
/// `tampering_is_detectable` above proves the chain works on the fields it
/// covers. This law asks the harder question: does it cover the whole row?
///
/// A field stored inside a hashed record but left outside the hash is a hole
/// with the shape of protection. The row looks tamper-evident, the chain
/// verifies, and that one column can be rewritten by anyone with the database
/// open. Worse, nothing about reading the record tells you which fields are
/// covered — so a consumer that trusts "the chain verified" trusts the
/// uncovered fields exactly as much as the covered ones.
///
/// This law is written to catch the NEXT field added outside the hash, not
/// only today's. It tampers with a non-covered column and requires the chain
/// to notice.
///
/// FIXED (#1786 Phase A): the v2 canonical encoding hashes
/// `phantom_reaches` (and `sources`), so erasing the record of newt
/// substituting a tool for the one the model named now breaks the chain.
/// The original violation: the column was stored per row and excluded from
/// the v1 encoding as "telemetry, not provenance" — but
/// `PhantomResolution::Rewrite` is the derivation edge between what was
/// emitted and what was executed, and `Unknown` is the fabrication ledger;
/// both are provenance someone will rely on.
///
/// HONEST RESIDUE (spec §3.2), so this green is never read as retroactive:
/// pre-bump v1 rows keep their reaches outside the hash FOREVER — their
/// encoding arm cannot change without rewriting history. This law covers
/// rows the current write path produces (v2); the v1 residue is permanent
/// and documented, not fixed.
#[test]
fn the_whole_record_is_covered_by_the_chain() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();

    // A turn in which newt silently rewrote the tool the model asked for.
    let reach = PhantomReach {
        name_as_called: "grep_files".to_string(),
        resolution: PhantomResolution::Rewrite("shell".to_string()),
        active_context_features: vec!["scheduled".to_string()],
    };
    store
        .append_turn_full(
            &id,
            "find the thing",
            "done",
            &[],
            &[reach],
            &[],
            None,
            None,
        )
        .unwrap();
    store.verify_chain(&id).expect("untampered chain verifies");

    // Rewrite ONLY the uncovered column: make it look as though newt never
    // substituted anything, and that the model called the right tool all along.
    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    let changed = conn
        .execute(
            "UPDATE turns SET phantom_reaches = '[]' WHERE conversation_id = ?1",
            rusqlite::params![&id],
        )
        .unwrap();
    assert_eq!(changed, 1, "the tamper must have actually landed");

    // Confirm the tamper is real and observable through the public API, so a
    // failure here is never mistaken for the edit not having happened.
    let rec = store.load(&id).unwrap();
    assert!(
        rec.turns[0].phantom_reaches.is_empty(),
        "the tampered value must be what the store now returns"
    );

    assert!(
        store.verify_chain(&id).is_err(),
        "the record of newt rewriting the model's tool call was erased and the \
         chain still verified.\n\
         Every field stored in a chained row must be inside the canonical \
         encoding, or 'the chain verified' means less than a reader will assume \
         it does.\n\
         Fix: include phantom_reaches in the v2 canonical encoding."
    );
}

// =========================================================================
// WIRING LAWS — a guarantee with no caller is not a guarantee
// =========================================================================

/// **Law: tamper detection has a production caller.**
///
/// This is the law that cannot be written as a behavioural test, and the
/// reason this file exists.
///
/// Writing tamper evidence is not the same as having tamper detection. If
/// nothing verifies the chain on the read path, then a tampered conversation
/// and an intact one are *observationally identical* to every consumer of the
/// store: same turns returned, same order, same everything. The evidence is
/// present in the database and buys exactly nothing, while looking from the
/// outside like diligence. No test that only reads data back can distinguish
/// those two worlds — so the law has to be about the wiring.
///
/// The same codebase already gets this right elsewhere:
/// `newt-core/src/agentic/artifact_read.rs` verifies its ledger's chain before
/// *every* read (three call sites, all on production paths). The durable
/// conversation store — the record that survives restarts and is the one an
/// audit would actually care about — does not. That asymmetry is the drift.
///
/// FIXED (#1785): the chain now has two production readers — the write path
/// checks the recorded tip witness before every append (`append_turn_full`),
/// and the restore path materializes the record through `load_verified`,
/// which verifies and loads from ONE SQLite read snapshot
/// (`prepare_conversation_restore`). This law holding is what keeps it that
/// way: if the restore-path verification is ever refactored away, this goes
/// red again.
///
/// Two deliberate narrowings, both load-bearing:
///
/// * The scan names `load_verified` and nothing else. A caller of
///   `verify_prompt_artifact_chain` is a DIFFERENT guarantee (the prompt
///   artifact ledger) and must never satisfy the conversation-chain law — the
///   first version of this law scanned both names, so wiring one chain could
///   silently excuse the other. And a return to the verify-then-load shape
///   (`verify_chain(id)?; load(id)?`) fails this law on purpose: two calls
///   verify one database state and hand back another, which is the TOCTOU
///   this fix removed.
///
/// * This scan is a STRUCTURAL RATCHET, not the proof of behaviour. It shows
///   the call is reachable; it cannot show the call refuses what it should.
///   The behavioural gate is the tamper-and-restore regressions below
///   (`restore_read_refuses_*`) and the seam test in newt-tui — a change that
///   keeps this scan green but breaks those has removed the protection while
///   preserving its appearance.
#[test]
fn evidence_unread_is_evidence_absent() {
    let callers = production_callers_of(&["load_verified"]);
    assert!(
        callers.iter().any(|c| c.contains("newt-tui")),
        "the restore path no longer materializes conversations through \
         load_verified, so nothing guarantees the record handed to the model \
         is the snapshot that was verified.\n\
         Found callers: {callers:?}\n\
         Fix: restore must go through ConversationStore::load_verified — \
         verify-then-load as two calls is the TOCTOU #1792 removed."
    );
}

/// **The scanner itself must work.**
///
/// `evidence_unread_is_evidence_absent` fails when it finds no production
/// caller — which is also what it would do if `production_callers_of` were
/// simply broken and returned nothing for everything. That would be a test
/// that reports a violation whether or not one exists, which is worth no more
/// than a test that reports success either way.
///
/// So: point the scanner at a guarantee that IS wired up. `verify_integrity`
/// is called on the production read path in `agentic/artifact_read.rs`. If
/// this test ever fails, the wiring law above has stopped being evidence and
/// its result must not be trusted until the scanner is fixed.
#[test]
fn the_scanner_finds_callers_that_do_exist() {
    let callers = production_callers_of(&["verify_integrity"]);
    assert!(
        !callers.is_empty(),
        "the source scanner found no caller of verify_integrity, which IS \
         called in production — the scanner is broken, so every result it \
         produces (including the violation above) is uninformative"
    );

    // The specific blindness that actually shipped in this scanner's first
    // version: a caller AFTER a `#[cfg(test)]` item in the same file. The
    // restore-path load_verified call sits thousands of lines past a test
    // seam in newt-tui/src/lib.rs; a scanner that latches on the first test
    // marker reports it absent — a violation verdict whether or not one
    // exists. Pin that the fix stays fixed.
    let callers = production_callers_of(&["load_verified"]);
    assert!(
        callers.iter().any(|c| c.contains("newt-tui")),
        "the scanner must see the restore-path caller that sits AFTER a \
         #[cfg(test)] item in the same file; found only: {callers:?}"
    );
}

/// The production-caller scanner must not let a test-only hit satisfy a
/// wiring law. This fixture covers the two places source layout can hide test
/// status from a line scanner: an out-of-line child reached through a
/// parent-side `#[cfg(test)] #[path = ...]`, and an inline cfg attribute whose
/// item opens its brace on the attribute line. It also pins that
/// `#[cfg(not(test))]` remains visible as production code.
#[test]
fn scanner_does_not_count_test_only_callers() {
    let root = tempfile::tempdir().unwrap();
    let src = root.path().join("src");
    std::fs::create_dir_all(src.join("lib_tests")).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
#[cfg(test)]
#[path = "lib_tests/core.rs"]
mod tests;

#[cfg(test)] fn inline_test_only() {
    if true { helper(); }
    load_verified();
}

#[cfg(not(test))]
fn production_only() { load_verified(); }
"#,
    )
    .unwrap();
    std::fs::write(
        src.join("lib_tests/core.rs"),
        "fn parent_gated_test_only() { load_verified(); }\n",
    )
    .unwrap();

    let callers = production_callers_of_in(root.path(), &["load_verified"]);
    assert_eq!(
        callers.len(),
        1,
        "only the production-only caller may be counted: {callers:?}"
    );
    assert!(
        callers[0].contains("production_only"),
        "the production-only caller must remain visible: {callers:?}"
    );
}

#[test]
fn cfg_test_only_matcher_is_conservative() {
    assert!(cfg_is_test_only("#[cfg(test)]"));
    assert!(cfg_is_test_only("#[cfg(all(test, unix))]"));
    assert!(!cfg_is_test_only("#[cfg(not(test))]"));
    assert!(!cfg_is_test_only("#[cfg(any(test, unix))]"));
}

/// Scan every crate's `src/` for a call to any of `names`, ignoring the
/// definition sites, doc comments, and source items that cannot compile in a
/// production build.
///
/// A source scan is an unusual shape for a test and is used deliberately: the
/// property under test is "this code is reachable in production", which is a
/// fact about the program text, not about any value the program computes.
fn production_callers_of(names: &[&str]) -> Vec<String> {
    production_callers_of_in(&common::workspace_root(), names)
}

/// The roots a law scans. Scoped to production workspace members (plus
/// newt-web) — see `common::production_roots`. A tempdir fixture with no
/// workspace manifest falls back to `<root>/src`, which is the shape the
/// scanner self-test builds.
fn roots_for(workspace_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let roots = production_roots(workspace_root);
    if roots.is_empty() {
        vec![workspace_root.join("src")]
    } else {
        roots
    }
}

fn production_callers_of_in(workspace_root: &std::path::Path, names: &[&str]) -> Vec<String> {
    let mut found = Vec::new();
    for_each_production_line(
        &roots_for(workspace_root),
        // agentic/artifact_read.rs defines a PRIVATE fn of the same name —
        // the session-local artifact ledger's verifier (the one that was
        // already wired correctly). Counting its `self.verify_chain(..)`
        // calls as callers of the STORE's chain verifier would make this
        // law pass with the store still unverified — a name collision a
        // textual scanner cannot resolve by types, so it is excluded by
        // file. Discovered red-first: reverting the #1785 fix left the law
        // green until this exclusion was added.
        &|path| path.ends_with("agentic/artifact_read.rs"),
        &mut |path, code, raw| {
            let ctrim = code.trim_start();
            for n in names {
                // A call, not the definition — matched on the stripped
                // line, so the name inside a string literal (an error
                // message, a doc example) cannot satisfy the law.
                if ctrim.contains(&format!("{n}(")) && !ctrim.contains(&format!("fn {n}(")) {
                    found.push(format!("{}: {}", path.display(), raw.trim_start()));
                }
            }
        },
    );
    found
}

/// Regression for #1785, write path: an append onto a conversation whose
/// recorded tip witness disagrees with the recorded final turn must refuse.
/// This is deliberately an O(1) witness check, not a full-chain walk.
///
/// Also pins the failure contract: refusing must not change what is stored.
#[test]
fn append_refuses_to_extend_a_tampered_tip_witness() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "user one", "assistant one").unwrap();
    store.append_turn(&id, "user two", "assistant two").unwrap();

    // Alter a recorded turn behind the store's back.
    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    conn.execute(
        "UPDATE turns SET assistant = 'rewritten' WHERE conversation_id = ?1
           AND seq = (SELECT MAX(seq) FROM turns WHERE conversation_id = ?1)",
        rusqlite::params![&id],
    )
    .unwrap();

    let err = store
        .append_turn(&id, "user three", "assistant three")
        .expect_err("appending onto a tampered chain must refuse");
    assert!(
        err.to_string().contains("chain violation"),
        "the refusal must say why: {err}"
    );

    // The failure contract: terminal for the write, harmless to the record.
    // Nothing repaired, nothing deleted, nothing appended.
    let rec = store.load(&id).unwrap();
    assert_eq!(rec.turns.len(), 2, "the refused turn must not have landed");
    assert_eq!(
        rec.turns[1].assistant, "rewritten",
        "the tampered row must be left exactly as found — evidence, not debris"
    );
}

/// The append guard validates the recorded tip witness only. An interior
/// mutation that leaves the recorded final row and witness intact is detected
/// by the full restore verification, not by the O(1) append path.
#[test]
fn append_tip_witness_only_checks_the_tip() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "u1", "a1").unwrap();
    store.append_turn(&id, "u2", "a2").unwrap();
    store.append_turn(&id, "u3", "a3").unwrap();

    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    let changed = conn
        .execute(
            "UPDATE turns SET assistant = 'rewritten history' WHERE conversation_id = ?1
               AND seq = (SELECT MIN(seq) + 1 FROM turns WHERE conversation_id = ?1)",
            rusqlite::params![&id],
        )
        .unwrap();
    assert_eq!(changed, 1, "the middle-turn tamper must have landed");

    store
        .append_turn(&id, "u4", "a4")
        .expect("the O(1) tip witness checks only the recorded final turn");
    assert_eq!(store.load(&id).unwrap().turns.len(), 4);

    let err = store
        .load_verified(&id)
        .expect_err("restore verification must catch the broken interior link");
    let msg = format!("{err}");
    assert!(
        msg.contains("chain violation") && msg.contains("does not link"),
        "the full verifier must name the broken link: {msg}"
    );
}

/// Regression for #1785, migration path: a conversation whose tip witness is
/// the schema-diff backfill (`''` — the column did not exist when the rows
/// were written) must still accept appends. An empty tip is absence of
/// evidence, not evidence of tampering, and the first post-migration append
/// is what establishes the witness. Refusing would lock writes out of exactly
/// the oldest histories.
#[test]
fn append_accepts_a_conversation_with_no_tip_witness() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "user one", "assistant one").unwrap();

    // Model the pre-column state the schema-diff backfill produces:
    // `tip_hash` blank while `writer_fingerprint` keeps its earlier-epoch
    // value — the exact drifted-schema fixture tests/store.rs hand-writes.
    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    conn.execute(
        "UPDATE conversations SET tip_hash = '' WHERE id = ?1",
        rusqlite::params![&id],
    )
    .unwrap();

    store
        .append_turn(&id, "post-migration", "ok")
        .expect("an absent witness must not refuse the append that repairs it");
    store
        .verify_chain(&id)
        .expect("the repairing append must leave a verifiable chain");
}

/// Regression for #1785/#1792, read path, MIDDLE-turn tamper: altering a turn
/// that has a successor breaks the successor's `prev_hash` link, so the
/// per-turn walk inside `load_verified` must refuse and name the broken link.
///
/// Also pins the failure contract: refusal returns no record, modifies
/// nothing, and leaves the tampered row exactly as the tamperer left it —
/// evidence, not debris.
#[test]
fn restore_read_refuses_a_tampered_middle_turn() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "u1", "a1").unwrap();
    store.append_turn(&id, "u2", "a2").unwrap();
    store.append_turn(&id, "u3", "a3").unwrap();

    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    let changed = conn
        .execute(
            "UPDATE turns SET assistant = 'rewritten history' WHERE conversation_id = ?1
               AND seq = (SELECT MIN(seq) + 1 FROM turns WHERE conversation_id = ?1)",
            rusqlite::params![&id],
        )
        .unwrap();
    assert_eq!(changed, 1, "the tamper must have actually landed");

    let err = store
        .load_verified(&id)
        .expect_err("a tampered middle turn must refuse the verified load");
    // Display (`{err}`), not the alternate chain format: the TUI surfaces
    // these with `{e}`, so the diagnosis must survive THAT rendering.
    let msg = format!("{err}");
    assert!(
        msg.contains("chain violation") && msg.contains("does not link"),
        "the refusal must name the broken per-turn link: {msg}"
    );
    // Seqs are PER-WRITER Lamport ticks — without the writer they do not
    // even identify a row in a multi-writer history. The diagnosis must
    // carry the writer fingerprint it already holds.
    assert!(
        msg.contains("writer"),
        "the per-turn diagnosis must name the writer whose chain broke: {msg}"
    );

    // Evidence preserved: nothing repaired, nothing deleted, tamper intact.
    let rec = store.load(&id).unwrap();
    assert_eq!(rec.turns.len(), 3, "refusal must not drop rows");
    assert_eq!(
        rec.turns[1].assistant, "rewritten history",
        "refusal must not modify the tampered row"
    );
}

/// Regression for #1785/#1792, read path, FINAL-turn tamper: the last turn has
/// no successor linking to it, so the per-turn walk cannot see the alteration
/// — only the tip witness can. `load_verified` must refuse via the witness.
///
/// The diagnostic must be honest about what a tip-only mismatch proves: the
/// witness disagrees with the named writer at its final seq. It must NOT
/// claim the final row is proven to be the corrupted datum — the witness
/// itself could be the altered side — and it must not promise a "first bad
/// turn" that a tip-only mismatch cannot locate.
#[test]
fn restore_read_refuses_a_tampered_final_turn() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "u1", "a1").unwrap();
    store.append_turn(&id, "u2", "a2").unwrap();

    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    let final_seq: i64 = conn
        .query_row(
            "SELECT MAX(seq) FROM turns WHERE conversation_id = ?1",
            rusqlite::params![&id],
            |row| row.get(0),
        )
        .unwrap();
    let changed = conn
        .execute(
            "UPDATE turns SET assistant = 'rewritten tail' WHERE conversation_id = ?1 AND seq = ?2",
            rusqlite::params![&id, final_seq],
        )
        .unwrap();
    assert_eq!(changed, 1, "the tamper must have actually landed");

    let err = store
        .load_verified(&id)
        .expect_err("a tampered final turn must refuse the verified load");
    // Display rendering again — see the middle-turn test.
    let msg = format!("{err}");
    assert!(
        msg.contains("chain violation") && msg.contains(&id),
        "the refusal must identify the conversation: {msg}"
    );
    assert!(
        msg.contains("tip witness") && msg.contains(&format!("seq {final_seq}")),
        "a tip-only mismatch must report the witness disagreeing at the \
         writer's final seq: {msg}"
    );
    assert!(
        !msg.contains("first bad turn"),
        "a tip-only mismatch cannot locate a first bad turn and must not \
         claim to: {msg}"
    );

    // Evidence preserved.
    let rec = store.load(&id).unwrap();
    assert_eq!(rec.turns.len(), 2);
    assert_eq!(rec.turns[1].assistant, "rewritten tail");
}

/// Regression for #1785/#1792, migration path: a conversation whose tip
/// witness is the schema-diff backfill (`''` — the column did not exist when
/// the rows were written) must still restore. An empty witness is absence of
/// evidence, not evidence of tampering — the same policy the append path
/// applies — and refusing would lock restores out of exactly the oldest
/// histories while asserting a conclusion nothing recorded supports. The
/// per-turn links are still fully verified.
#[test]
fn restore_read_accepts_an_absent_tip_witness() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "u1", "a1").unwrap();
    store.append_turn(&id, "u2", "a2").unwrap();

    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    conn.execute(
        "UPDATE conversations SET tip_hash = '' WHERE id = ?1",
        rusqlite::params![&id],
    )
    .unwrap();

    let rec = store
        .load_verified(&id)
        .expect("an absent witness must not refuse the restore that precedes its repair");
    assert_eq!(rec.turns.len(), 2, "the full record must materialize");
}

/// Regression for the adversarial-review finding on #1792, corrected against
/// the migration fixtures: a blank `tip_hash` with a real writer is a
/// SANCTIONED historical state (the drifted-schema and pre-FTS fixtures in
/// tests/store.rs hand-write it), so absence-of-witness must key on the tip
/// alone. The REVERSE mix — a tip witness recorded with a blank writer — has
/// no producer at all (every write of the tip writes the writer in the same
/// statement; no migration blanks the writer while keeping the tip) and must
/// refuse: it is evidence the witness columns themselves were altered.
#[test]
fn restore_read_refuses_a_witness_with_no_writer() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "u1", "a1").unwrap();

    let conn = rusqlite::Connection::open(root.path().join("conversations.db")).unwrap();
    conn.execute(
        "UPDATE conversations SET writer_fingerprint = '' WHERE id = ?1",
        rusqlite::params![&id],
    )
    .unwrap();

    let err = store
        .load_verified(&id)
        .expect_err("a witness attributed to no writer must refuse the verified load");
    let msg = format!("{err}");
    assert!(
        msg.contains("chain violation") && msg.contains(&id),
        "the unattributed witness must refuse with an integrity diagnosis: {msg}"
    );

    // Evidence preserved.
    let rec = store.load(&id).unwrap();
    assert_eq!(rec.turns.len(), 1, "refusal must not drop rows");
}

// =========================================================================
// SHAPE LAWS — can the type express what the principle requires?
// =========================================================================

/// **Law: a derived record names what it derives from.**
///
/// Provenance-traceable means every value that is not a direct recording of
/// something witnessed can be followed back to the witnessed things it came
/// from. A compaction summary is the clearest case: it replaces N turns with
/// generated prose, and that prose is an assertion whose only justification is
/// the turns it replaced.
///
/// A record that names no source is unattributable by construction — you
/// cannot audit it, you cannot re-derive it, and you cannot tell a faithful
/// summary from a fabricated one, because both look the same on the wire.
///
/// VIOLATION: `ConversationTurn` has no field naming a source. A compaction
/// summary is written into the same `user`/`assistant` strings as a real turn,
/// with nothing distinguishing it and nothing pointing back. Once written, its
/// origin is gone.
#[test]
#[ignore = "FIRST-PRINCIPLE VIOLATION — ConversationTurn cannot express provenance; summaries name no source turns"]
fn derived_records_name_their_sources() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = store(root.path(), workspace.path());
    let id = store.create("first-principle", None).unwrap();
    store.append_turn(&id, "user one", "assistant one").unwrap();
    store.append_turn(&id, "user two", "assistant two").unwrap();

    // A summary of the two turns above is appended the only way the store
    // allows: as another turn, indistinguishable from a witnessed one.
    store
        .append_turn(
            &id,
            "",
            "[CONTEXT COMPACTION — REFERENCE ONLY] the user asked twice",
        )
        .unwrap();

    let rec = store.load(&id).unwrap();
    let summary = rec.turns.last().unwrap();

    // The law: this derived record must name the turns it derives from. There
    // is currently no field in which to say so, which is the violation.
    let names_sources = format!("{summary:?}").contains("source");
    assert!(
        names_sources,
        "a compacted summary was recorded with no reference to the turns it \
         replaced — it is an unattributable assertion.\n\
         Fix: give the turn record a source-reference field so a derived entry \
         names its inputs, making it re-derivable and auditable."
    );
}

// =========================================================================
// THE RATCHET
// =========================================================================

/// **The debt may only shrink.**
///
/// Counts the violation markers in this file and compares against
/// `KNOWN_VIOLATIONS`. Two failure modes it exists to catch:
///
/// * A new violation is added without being declared — count goes up, this
///   fails, and the author must acknowledge the debt explicitly.
/// * A violation is "fixed" by deleting its `#[ignore]` line along with the
///   test — count goes down without the constant being lowered, this fails.
///
/// A ratchet is used rather than a comment because a comment cannot fail.
#[test]
fn violation_ledger_is_current() {
    let this_file = include_str!("first_principle.rs");
    let marker = "FIRST-PRINCIPLE VIOLATION";
    // Subtract the occurrences in this test's own documentation and body.
    let in_ignores = this_file
        .lines()
        .filter(|l| l.trim_start().starts_with("#[ignore =") && l.contains(marker))
        .count();

    assert_eq!(
        in_ignores, KNOWN_VIOLATIONS,
        "the first-principle violation count changed.\n\
         Found {in_ignores} law(s) marked as violated, but KNOWN_VIOLATIONS says \
         {KNOWN_VIOLATIONS}.\n\
         If you fixed one: remove its #[ignore], confirm the test passes, and \
         lower KNOWN_VIOLATIONS.\n\
         If you added one: file it, name the issue in the marker, and raise \
         KNOWN_VIOLATIONS deliberately."
    );
}
