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

use newt_core::ConversationStore;

/// Number of laws in this file currently marked as violations. This may only
/// go DOWN, and only in a change that also removes the corresponding
/// `#[ignore]` and shows the test passing. Raising it requires filing the
/// violation as an issue and naming it in the marker.
const KNOWN_VIOLATIONS: usize = 2;

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
/// VIOLATION: `ConversationStore::verify_chain` is called only from
/// `newt-core/tests/`, `newt-core/tests/workspace_key.rs`, and a benchmark
/// script. It has no caller under any crate's `src/`.
#[test]
#[ignore = "FIRST-PRINCIPLE VIOLATION — conversation chain is written every turn and never verified in production"]
fn evidence_unread_is_evidence_absent() {
    let callers = production_callers_of(&["verify_chain", "verify_prompt_artifact_chain"]);
    assert!(
        !callers.is_empty(),
        "no production code verifies the conversation chain.\n\
         The chain is written on every append and read back by nobody, so a \
         tampered store behaves exactly like an intact one.\n\
         Fix: verify on the read path (see agentic/artifact_read.rs for the \
         pattern this crate already uses correctly), or on resume, or both."
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
}

/// Scan every crate's `src/` for a call to any of `names`, ignoring the
/// definition sites, doc comments, and `#[cfg(test)]` modules.
///
/// A source scan is an unusual shape for a test and is used deliberately: the
/// property under test is "this code is reachable in production", which is a
/// fact about the program text, not about any value the program computes.
fn production_callers_of(names: &[&str]) -> Vec<String> {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("newt-core has a parent workspace directory")
        .to_path_buf();

    let mut found = Vec::new();
    let mut stack = vec![workspace_root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                // Skip build output, VCS, and vendored/worktree copies — a hit
                // inside target/ or .claude/worktrees/ is not this build's code.
                if matches!(name.as_ref(), "target" | ".git" | ".claude" | "docs") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // Only production sources: a call from tests/ or benches/ is
            // exactly the situation this law is trying to detect.
            if !path.components().any(|c| c.as_os_str() == "src") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut in_test_mod = false;
            for line in text.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("#[cfg(test)]") {
                    in_test_mod = true;
                }
                if in_test_mod {
                    continue;
                }
                // Doc comments describe the guarantee; they do not invoke it.
                if trimmed.starts_with("//") {
                    continue;
                }
                for n in names {
                    // A call, not the definition.
                    if trimmed.contains(&format!("{n}(")) && !trimmed.contains(&format!("fn {n}("))
                    {
                        found.push(format!("{}: {}", path.display(), trimmed));
                    }
                }
            }
        }
    }
    found
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
