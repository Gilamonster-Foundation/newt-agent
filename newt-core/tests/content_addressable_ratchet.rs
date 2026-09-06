//! **The content-addressable ratchet.**
//!
//! The rule (`CLAUDE.md`, `AGENTS.md`, the workspace `AGENTS.md`, the
//! Authority Register, and the `provenance-audit` skill — five places):
//!
//! > Every data structure that is persisted, transmitted, chained, or
//! > identified takes its identity from `content-addressable` (v0.1.2).
//! > Hand-rolling a hash, digest, id, or canonical encoding is a defect.
//!
//! Since 0.1.2 the rule has a second half, because decoding is where a
//! canonical identity is most easily lost:
//!
//! > Decode canonical bytes only through `from_canonical_dagcbor_checked`
//! > (or `ContentAddressable::from_canonical_form`). The bare
//! > `from_canonical_dagcbor` is deprecated: it verifies neither canonical
//! > form nor the typed round trip, so the value it hands back can carry a
//! > different `ContentId` than the bytes it came from.
//!
//! # Why a test and not a sixth paragraph
//!
//! The rule already existed in all five of those places on 2026-08-22, and
//! three design rounds (~70 confirmed review findings) still went into
//! rebuilding a content-addressed span store, a dag-cbor identity scheme, and
//! a Merkle DAG — all of which already shipped, in this crate's own
//! dependency tree. Prose that is not read does not bind. A failing test does.
//!
//! # What this file does
//!
//! It counts the surviving hand-rolled `blake3::hash(...)` sites and holds
//! that count against a constant that may only go DOWN. It is the
//! `KNOWN_VIOLATIONS` pattern from `first_principle.rs`, pointed at
//! encodings instead of integrity.
//!
//! Both of those count a **bad** identity. The third section — "The ABSENCE
//! ratchet", at the bottom of this file — counts **no** identity: durable
//! journals whose rows are addressed by nothing at all. It is a separate
//! section because it is a separate hazard: a presence check fails closed and
//! an absence check fails OPEN, so it carries its own anti-vacuous proofs.
//! What counts as a journal, and the two-tier split, are defined there.
//!
//! **It does not attempt to prove the conversions are correct** — that is
//! each conversion's own regression tests. This proves only that the debt is
//! counted, cannot grow silently, and is being paid down. A ratchet is a
//! direction, not a proof.
//!
//! # Working it down
//!
//! Pick a site, decide what it IS, convert, lower the constant:
//!
//! | The thing being identified | Type |
//! |---|---|
//! | a canonical structured value (record, event, manifest) | `ContentId` |
//! | an opaque byte string (file, payload, tool result, cache key) | `RawContentId` |
//! | a node with causal parents (chain / DAG link) | `MerkleNode<T>` |
//!
//! `ContentId` and `RawContentId` are DIFFERENT identities even when their
//! digest bytes match — the profile is semantic, so pick by what the thing is,
//! not by which is convenient.
//!
//! `RawContentId`, `MerkleNode<T>`, and `NodeStore` reach further than the
//! frozen core this workspace pins, so a conversion to one of those is a
//! dependency decision as well as a code change — raise it rather than
//! assuming it is available.
//!
//! **`unstable-merkle` is now ON** (#2085), and that was raised rather than
//! assumed. The event journal needs a node addressed over its payload AND its
//! parents, because that is the only thing that makes a deleted or reordered
//! row detectable; nothing in the default feature set provides it, and the
//! alternative was hand-rolling the chain — the exact defect this file counts.
//! `unstable-store` remains OFF: `NodeStore` has no consumer yet, and turning
//! on a feature for a type nobody calls is how an unstable surface becomes
//! load-bearing by accident.

use std::collections::BTreeMap;

/// Hand-rolled `blake3::hash(...)` sites still in production source.
///
/// MAY ONLY GO DOWN. Lower it in the same change that removes a site; never
/// raise it — a new hand-rolled digest is the defect this file exists to
/// prevent, and "I needed one" is what every one of the current sites also
/// believed.
const KNOWN_HAND_ROLLED_DIGESTS: usize = 20;

/// Per-file expected counts, so a conversion in one file cannot be masked by
/// a new site appearing in another — the aggregate alone would net to zero
/// and report success while the rule was being broken.
fn expected_by_file() -> BTreeMap<&'static str, usize> {
    BTreeMap::from([
        // Tests comparing digests; convert with their subject. REPOINTED by
        // #1899, not lowered, when `tools.rs`'s inline tests became siblings;
        // repointed again when the branch tests were grouped by behavior.
        // All five sites remain intact in the file-artifact family, including
        // two Unix-gated sites. This relocation pays no ratchet debt.
        (
            "newt-core/src/agentic/tools_tests/execute_file_artifacts.rs",
            5,
        ),
        // Artifact digests over opaque bytes → RawContentId.
        ("newt-core/src/agentic/artifact_hooks.rs", 4),
        // The §6 turn chain: canonical_encoding_v1/v2 + the content id.
        // The highest-value conversion — these ARE the record identities,
        // and per the migration posture they get SMASHED (one importer,
        // then one encoding), not carried as a third dispatch arm.
        ("newt-core/src/store/turn_chain.rs", 3),
        ("newt-core/src/store.rs", 1),
        ("newt-core/src/prune.rs", 1),
        ("newt-core/src/navigator/ledger.rs", 1),
        ("newt-core/src/drift_cache.rs", 1),
        ("newt-core/src/conversation.rs", 1),
        ("newt-core/src/agentic/prompt_intake.rs", 1),
        ("newt-core/src/agentic/mod_tests/artifact_provenance.rs", 1),
        ("newt-core/src/agentic/artifact_read.rs", 1),
    ])
}

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("newt-core has a parent workspace directory")
        .to_path_buf()
}

/// Count `blake3::hash(` occurrences per production source file.
///
/// Deliberately textual: the property is "no hand-rolled digest appears in
/// the source", which is a fact about the program text. A type-level rule
/// would be stronger and is the eventual goal — this is what can be enforced
/// today without restructuring every call site first.
fn hand_rolled_sites() -> BTreeMap<String, usize> {
    sites_matching(|line| line.contains("blake3::hash("))
}

/// Production source lines matching `predicate`, per file.
///
/// One scanner for every row this file arms: a second copy would drift from
/// the first, which is the sprawl the ratchet exists to count.
fn sites_matching(predicate: impl Fn(&str) -> bool) -> BTreeMap<String, usize> {
    production_sources()
        .into_iter()
        .filter_map(|(rel, text)| {
            let n = code_lines(&text).filter(|l| predicate(l)).count();
            (n > 0).then_some((rel, n))
        })
        .collect()
}

/// A source's lines with whole-line comments dropped.
///
/// Both rows need this and for the same reason: a rule quoted in a doc
/// comment is documentation, not code. Counting it would flag every file
/// that *explains* the rule — including this one's neighbours — and, worse,
/// would let a violation hide behind a comment that merely names the fix.
fn code_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|l| !l.trim_start().starts_with("//"))
}

/// Every production `.rs` source in the workspace, as `(relative path, text)`.
///
/// **The single walker.** `sites_matching` (presence of a bad construct) and
/// [`journals_under`] (absence of a required one) read the same file list, so
/// a directory exclusion cannot silently apply to one row and not the other.
fn production_sources() -> Vec<(String, String)> {
    production_sources_under(&workspace_root())
}

fn production_sources_under(root: &std::path::Path) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                // Hidden directories included: the main checkout carries
                // gitignored worktrees under `.worktrees/` with full `src`
                // trees, and counting a nested copy would inflate every row
                // on a developer's machine while CI stayed green.
                if name.starts_with('.') || matches!(name.as_ref(), "target" | "docs") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // Production sources only. `tests/` are allowed to compute a
            // digest independently — that is how a test checks the crate
            // without agreeing with it by construction.
            if !path.components().any(|c| c.as_os_str() == "src") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            found.push((rel, text));
        }
    }
    found
}

/// **The ratchet.** The total may only fall.
#[test]
fn hand_rolled_digests_only_decrease() {
    let found = hand_rolled_sites();
    let total: usize = found.values().sum();
    assert!(
        total <= KNOWN_HAND_ROLLED_DIGESTS,
        "hand-rolled digest count went UP: {total} > {KNOWN_HAND_ROLLED_DIGESTS}.\n\
         Every persisted, transmitted, chained, or identified structure takes its \
         identity from `content-addressable` (v0.1.1) — ContentId for a canonical \
         structured value, RawContentId for an opaque byte string, MerkleNode for a \
         node with causal parents.\n\
         Sites now: {found:#?}"
    );
    assert_eq!(
        total, KNOWN_HAND_ROLLED_DIGESTS,
        "hand-rolled digest count went DOWN to {total} — good. Lower \
         KNOWN_HAND_ROLLED_DIGESTS to {total} and update expected_by_file() in the \
         same change, so the next one cannot hide behind the slack."
    );
}

/// Per-file, so paying down one file cannot mask a new site in another.
#[test]
fn no_file_gains_a_hand_rolled_digest() {
    let found = hand_rolled_sites();
    let expected = expected_by_file();
    for (file, count) in &found {
        let allowed = expected.get(file.as_str()).copied().unwrap_or(0);
        assert!(
            *count <= allowed,
            "{file} has {count} hand-rolled digest(s), expected at most {allowed}.\n\
             A NEW hand-rolled digest is the defect this ratchet exists to prevent. \
             Use `content_addressable`: ContentId (structured value), RawContentId \
             (opaque bytes), or MerkleNode (causal link)."
        );
    }
    for (file, allowed) in &expected {
        let now = found.get(*file).copied().unwrap_or(0);
        assert!(
            now <= *allowed,
            "{file}: {now} > {allowed} (bookkeeping drift)"
        );
    }
}

/// The ratchet must be able to SEE a site — otherwise it would report a clean
/// tree while the rule was being broken, which is the vacuous-green pattern
/// this codebase has caught four times already (a latched `#[cfg(test)]`
/// scanner, a name-collision that satisfied a wiring law, an unreachable
/// agreement check, and an invariant satisfiable by construction).
#[test]
fn the_ratchet_can_see_the_sites_it_counts() {
    let found = hand_rolled_sites();
    assert!(
        !found.is_empty(),
        "the scanner found NO hand-rolled digests anywhere — either the debt is \
         genuinely paid (then delete this file and the constant) or the scanner is \
         broken and every green it reports is uninformative"
    );
    assert!(
        found.contains_key("newt-core/src/store/turn_chain.rs"),
        "the scanner must see the §6 turn-chain sites, the ones this rule most \
         wants converted; found: {found:#?}"
    );
}

/// The crate is a real dependency, not an aspiration — so a conversion has
/// somewhere to land.
/// Bare (unchecked) decodes of canonical bytes still in production source.
///
/// The bare door returns a value without verifying that re-encoding it
/// reproduces the input, so what comes back can carry a different
/// `ContentId` than the bytes it was read from — a silently different
/// identity, which is the failure this whole file exists to prevent.
///
/// EXACTLY ONE is expected, and it is not a debt to pay down: the schema-tag
/// probe in `newt-interaction/src/downgrade.rs` reads a SUBSET of a record
/// on purpose (the tag, to decide which decoder to open), so the checked
/// door's guarantee is not merely unmet there but meaningless — a tag-only
/// struct cannot re-encode to a whole record. The crate names this caller
/// explicitly. It must carry a local `#[allow(deprecated)]`, which is what
/// makes it visible in review rather than accidental.
const EXPECTED_BARE_DECODES: usize = 1;

#[test]
fn no_production_code_decodes_through_the_bare_door() {
    let sites = sites_matching(|line| {
        line.contains("from_canonical_dagcbor(") && !line.contains("_checked(")
    });
    let total: usize = sites.values().sum();
    assert_eq!(
        total, EXPECTED_BARE_DECODES,
        "bare canonical decodes in production changed: {sites:?}\n\
         The bare door does not verify the typed round trip, so the value it \
         returns can carry a different ContentId than its bytes. Use \
         `from_canonical_dagcbor_checked`. The one sanctioned exception is \
         the schema-tag probe, which reads a subset by design."
    );

    // ...and it is the site we think it is, carrying its justification.
    let expected = "newt-interaction/src/downgrade.rs";
    assert_eq!(
        sites.keys().collect::<Vec<_>>(),
        vec![expected],
        "the bare decode moved: {sites:?}"
    );
    let text = std::fs::read_to_string(workspace_root().join(expected)).expect("the probe file");
    assert!(
        text.contains("#[allow(deprecated)]"),
        "{expected} decodes through the bare door without the local \
         `#[allow(deprecated)]` that makes the exception visible"
    );
}

/// **Anti-vacuous twin.** A scanner that finds nothing reports success
/// forever, and this row's expected count is small enough that a broken
/// scanner would look exactly like a clean tree.
#[test]
fn the_bare_decode_scanner_sees_a_seeded_call() {
    let seeded = "    let v: T = canonical::from_canonical_dagcbor(bytes)?;";
    let predicate =
        |line: &str| line.contains("from_canonical_dagcbor(") && !line.contains("_checked(");
    assert!(predicate(seeded), "the scanner missed a seeded bare decode");
    // ...and does not fire on the checked door, or the row would count
    // every correct call site as a violation.
    assert!(!predicate(
        "    let v: T = canonical::from_canonical_dagcbor_checked(bytes)?;"
    ));
    // ...nor on a comment mentioning it, which the line filter drops.
    assert!(
        sites_matching(|l| l.contains("from_canonical_dagcbor("))
            .values()
            .sum::<usize>()
            >= EXPECTED_BARE_DECODES
    );
}

#[test]
fn the_content_addressable_crate_is_available() {
    use content_addressable::ContentAddressable;
    #[derive(serde::Serialize)]
    struct Probe<'a> {
        note: &'a str,
    }
    impl ContentAddressable for Probe<'_> {
        fn canonical_form(&self) -> Result<Vec<u8>, content_addressable::ContentError> {
            content_addressable::canonical::to_canonical_dagcbor(self)
        }
    }
    let a = Probe { note: "same" }
        .content_id()
        .expect("probe must address");
    let b = Probe { note: "same" }
        .content_id()
        .expect("probe must address");
    let c = Probe { note: "other" }
        .content_id()
        .expect("probe must address");
    assert_eq!(a, b, "equal values must produce equal content ids");
    assert_ne!(a, c, "different values must produce different content ids");
}

// ---------------------------------------------------------------------------
// The ABSENCE ratchet: journals that mint no identity at all.
// ---------------------------------------------------------------------------
//
// Everything above counts a BAD identity — a hand-rolled digest, a decode that
// loses the round trip. It was built on the assumption that the danger is
// minting the wrong id, and so it is structurally blind to the opposite and
// more common failure: minting NONE.
//
// That blindness was not hypothetical. `newt-core/src/denial_journal.rs`
// persisted `DenialRecord` as JSON lines with zero occurrences of `ContentId`,
// `MerkleNode`, `content_addressable`, `blake3`, or even the word `hash`, in a
// repo whose CLAUDE.md says every persisted structure takes its identity from
// the crate. Every gate in this file was green on it. A human found it by
// reading the file.
//
// That row has since been PAID: #2088 moved `denial_journal` onto
// `event_journal`'s chain while this ratchet was in flight. The past tense is
// deliberate and the history is kept rather than tidied away — the file that
// motivated the gate is no longer the file the gate catches, and a reader who
// finds `denial_journal` clean must be able to see why it is still named here.
// The anchor in the real tree moved to `flight_recorder`, and a standing
// assertion below requires `denial_journal` to stay out of every tier.
//
// # What counts as a violation — the definition IS the policy
//
// A **journal** is a production source that appends a serde-serialized record
// to a file it opened with `.append(true)`. That exact pair — a
// `serde_json::to_string` beside an `OpenOptions::new().append(true)` — is
// this repo's durable-evidence idiom, and the scan below is nothing more than
// looking for both in one file.
//
// A journal is a **violation** when its code (comments excluded) names none of
// `ContentId`, `RawContentId`, `ContentAddressable`, or `MerkleNode`: the rows
// it writes take their identity from nothing, so an edited row is undetectable
// by any means the program possesses.
//
// # What the shape deliberately excludes, and why that is not a loophole
//
// The exclusions fall out of the shape rather than being curated, which is the
// point — a hand-maintained skip list is where the next violation hides:
//
// * **Rustyline history** (`rich_input.rs`, `lean_input.rs`) appends raw
//   strings, not a serialized record. There is no record type to address, and
//   the line format belongs to rustyline.
// * **`newt solve --events`** appends already-built `serde_json::Value`s to an
//   operator-named debug path. Untyped by construction; nothing to implement
//   `ContentAddressable` on.
// * **Config** (`settings.rs`, `tuning.rs`, …) is written whole-document with
//   `fs::write`, never appended. Config is a statement of present intent that
//   is replaced wholesale — it has no history, so it has nothing to tamper.
// * **In-memory types** never reach a writer at all.
//
// Test-support files that live under `src/` (`mod_tests/`, `tools_tests/`) are
// in scope by construction, and today none of them match. Left that way on
// purpose: a fixture that grows a real durable journal should be seen.
//
// # Two tiers, argued
//
// `settings_receipt` addresses each row with a `ContentId` but does not chain;
// `event_journal` chains (#2085). These are **not** the same defect as
// addressing nothing, and conflating them into one count would do two concrete
// harms. It would mark `settings_receipt` — the file this repo holds up as the
// good example, cited by `event_journal`'s own module doc — a violation, which
// is how a gate teaches people to read it as noise. And it would make the
// single most valuable step, `denial_journal` gaining a real identity, register
// as zero movement. A ratchet that cannot see progress is not a ratchet — and
// that step has now actually happened (#2088), which is the case the tier
// split was designed for rather than a hypothetical one.
//
// So: two named tiers, and the monotone bound that actually matters is on
// their SUM. That ordering is deliberate — promoting a journal from tier 1 to
// tier 2 must be allowed to raise tier 2, or the ratchet would forbid the
// improvement it exists to demand.

/// Journals that mint no identity at all. **MAY ONLY GO DOWN.**
///
/// Decrements, itemized — a ratchet move without an argument is the thing the
/// ratchet exists to prevent:
///
/// - **4 → 3: `denial_journal` pays (#2088).** It no longer appends its own
///   JSON lines; it delegates to `event_journal`, so its records are chained
///   and it correctly falls out of the scan entirely rather than moving to
///   tier 2. This ratchet was written to catch that file, and the debt was
///   paid while this change was in flight — so the real-tree anchor below
///   moved to `flight_recorder` rather than being deleted.
const UNADDRESSED_JOURNALS: usize = 3;

/// Journals that address each row but do not chain them, so an edited row is
/// caught and a deleted or reordered one is not.
const ADDRESSED_BUT_UNCHAINED_JOURNALS: usize = 1;

/// The destination invariant: every journal ends chained. **MAY ONLY GO
/// DOWN** — and unlike the two tiers it is stable under a tier-1 → tier-2
/// promotion, which is what lets the tiers move independently.
const UNCHAINED_JOURNALS: usize = UNADDRESSED_JOURNALS + ADDRESSED_BUT_UNCHAINED_JOURNALS;

/// How much identity a journal's records carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// No id at all. An edited row is undetectable.
    Unaddressed,
    /// Per-row `ContentId`. An edited row is caught; a deleted or reordered
    /// one is not, because every row stays independently valid.
    AddressedUnchained,
    /// `MerkleNode` over payload AND parent. Deletion and reorder are caught.
    Chained,
}

/// Classify one production source's text. `None` when it is not a journal.
///
/// Pure and text-only on purpose: it is the piece the fixture test below can
/// feed a known positive, which is the only way to show the detector detects.
fn journal_tier(text: &str) -> Option<Tier> {
    let code: Vec<&str> = code_lines(text).collect();
    let names = |needle: &str| code.iter().any(|l| l.contains(needle));
    if !(names("append(true)") && names("serde_json::to_string")) {
        return None;
    }
    if names("MerkleNode") {
        return Some(Tier::Chained);
    }
    // `RawContentId` contains `ContentId`, so one probe covers both.
    if names("ContentId") || names("ContentAddressable") {
        return Some(Tier::AddressedUnchained);
    }
    Some(Tier::Unaddressed)
}

/// Every journal under `root`, classified.
///
/// **The anti-vacuous guard lives here, in the shared path**, because an
/// absence check fails OPEN: a scan for "files lacking X" passes trivially
/// when it scans nothing, and anything that narrows the input makes a pass
/// MORE likely. So the scan asserts it read something before it is allowed to
/// report a clean tree. Point this at an empty directory and it fails rather
/// than congratulating you.
fn journals_under(root: &std::path::Path) -> BTreeMap<String, Tier> {
    let sources = production_sources_under(root);
    assert!(
        !sources.is_empty(),
        "the journal scan read NO production sources under {}: every absence \
         check below would pass vacuously. The walker is broken, or the root \
         is wrong.",
        root.display()
    );
    assert!(
        sources.iter().any(|(_, text)| !text.trim().is_empty()),
        "the journal scan read {} sources and every one was empty",
        sources.len()
    );
    sources
        .into_iter()
        .filter_map(|(rel, text)| journal_tier(&text).map(|tier| (rel, tier)))
        .collect()
}

fn journals() -> BTreeMap<String, Tier> {
    journals_under(&workspace_root())
}

/// The enumerated debt: `(file, record type, why it must move)`.
///
/// The type name is asserted present in the file, so this table cannot rot
/// into naming a type that was renamed or moved out from under it — the
/// identity half of count-plus-identity-plus-reason.
fn unaddressed_debt() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "newt-core/src/agentic/permissions.rs",
            "PersistentDenial",
            "The persistent denylist is READ BACK by `load_denials` and \
             consulted as authority. Unaddressed evidence that re-enters an \
             authority decision is the highest-severity row here: editing a \
             line edits what the agent may do. NOTE: this file carries a \
             SECOND unaddressed type, `PermissionRecord` (the \
             `permission-log.jsonl` audit trail). Addressing only one drops \
             the file from the scan, so the exact-set assertion goes red and \
             the row must be justified again by hand rather than lapsing.",
        ),
        (
            "newt-core/src/flight_recorder.rs",
            "ShadowCaveat",
            "`unconfined.jsonl` is folded back by `newt ocap propose` into a \
             proposed caveat set. Same shape as the denylist: unaddressed \
             evidence feeding an authority proposal.",
        ),
        (
            "newt-core/src/metrics.rs",
            "TurnMetrics",
            "Durable telemetry, and the one row whose payment needs a design \
             decision first: `rotate_log` truncates the file to the last N \
             lines, so a naive chain would be broken by the program itself \
             every rotation. It needs the Authority Register's \
             chain-plus-one-ref shape, with the head reference surviving \
             rotation. Counted anyway — the rule says every persisted \
             structure, and carving out the awkward one is how a ratchet \
             starts lying.",
        ),
    ]
}

/// The addressed-but-unchained tier: real ids, no link between rows.
fn unchained_debt() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![(
        "newt-core/src/settings_receipt.rs",
        "SettingChange",
        "Each row carries the id its own bytes compute to and `is_intact` \
         re-derives it, so an edited row is caught. Nothing links row N to \
         row N-1, so deleting the receipt for a raised cap leaves a file that \
         is still perfectly valid. `event_journal` is the shape it migrates \
         onto.",
    )]
}

/// **The absence ratchet.** Both tiers, and the sum that may only fall.
#[test]
fn unaddressed_journals_only_decrease() {
    let found = journals();
    let count = |t: Tier| found.values().filter(|v| **v == t).count();
    let unaddressed = count(Tier::Unaddressed);
    let unchained = unaddressed + count(Tier::AddressedUnchained);

    assert!(
        unaddressed <= UNADDRESSED_JOURNALS,
        "a journal that addresses NOTHING was added: {unaddressed} > \
         {UNADDRESSED_JOURNALS}.\n\
         Every persisted structure takes its identity from \
         `content-addressable` (CLAUDE.md, \"Content-addressable data \
         structures\"): ContentId for a canonical structured value, \
         RawContentId for opaque bytes, MerkleNode for a row with a causal \
         parent. A journal whose rows carry no id cannot detect its own \
         tampering.\n\
         Journals now: {found:#?}"
    );
    assert_eq!(
        unaddressed, UNADDRESSED_JOURNALS,
        "unaddressed journals fell to {unaddressed} — good. Lower \
         UNADDRESSED_JOURNALS to {unaddressed} and drop the paid row from \
         unaddressed_debt() in the same change, so the next one cannot hide \
         in the slack."
    );

    assert!(
        unchained <= UNCHAINED_JOURNALS,
        "unchained journals went UP: {unchained} > {UNCHAINED_JOURNALS}. A \
         per-row id detects an EDITED row and nothing else; only a chain \
         detects a deleted or reordered one.\n\
         Journals now: {found:#?}"
    );
    assert_eq!(
        unchained, UNCHAINED_JOURNALS,
        "unchained journals fell to {unchained} — lower the constants."
    );
}

/// Count is the weakest of the three claims. This is identity and reason: the
/// violations are the specific files the table names, each still exhibits the
/// journal shape, and each still contains the record type it is charged with.
#[test]
fn the_debt_is_the_files_and_types_the_table_names() {
    let found = journals();
    let root = workspace_root();

    for (tier, table, label) in [
        (Tier::Unaddressed, unaddressed_debt(), "unaddressed"),
        (
            Tier::AddressedUnchained,
            unchained_debt(),
            "addressed-but-unchained",
        ),
    ] {
        let actual: Vec<&str> = found
            .iter()
            .filter(|(_, t)| **t == tier)
            .map(|(f, _)| f.as_str())
            .collect();
        let expected: Vec<&str> = table.iter().map(|(f, _, _)| *f).collect();
        assert_eq!(
            actual, expected,
            "the {label} journals moved.\n\
             Update the table in the same change that moved them — a row that \
             silently disappears is indistinguishable from a violation the \
             scan stopped being able to see."
        );

        for &(file, ty, why) in &table {
            let text = std::fs::read_to_string(root.join(file))
                .unwrap_or_else(|e| panic!("{file}: {e} — enumerated but unreadable"));
            assert!(
                code_lines(&text).any(|l| l.contains(ty)),
                "{file} no longer defines `{ty}`. The table is charging a type \
                 that moved; re-point it or drop the row. Reason on file: {why}"
            );
            assert!(
                why.len() > 40,
                "{file} carries no real justification — a row without a reason \
                 is a number, and numbers do not get paid down"
            );
        }
    }
}

/// The tier split is the repo's own written position, not this file's
/// invention. `event_journal`'s module doc is where it is stated; if that
/// claim is deleted, the split it justifies must be re-argued, so this goes
/// red rather than quietly outliving its rationale.
#[test]
fn the_tier_split_carries_its_justification_in_the_source() {
    let doc = std::fs::read_to_string(workspace_root().join("newt-core/src/event_journal.rs"))
        .expect("the chained journal");
    // Strip comment markers and collapse whitespace first: the claims below
    // are wrapped prose, and re-wrapping a paragraph must not break the gate.
    // Deleting the claim must.
    let doc: String = doc
        .lines()
        .map(|l| {
            l.trim_start()
                .trim_start_matches("//!")
                .trim_start_matches("//")
        })
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for claim in [
        "addresses each row with a `ContentId`",
        "addresses nothing at all",
        "the chained shape both are meant to migrate onto",
    ] {
        assert!(
            doc.contains(claim),
            "event_journal.rs no longer says {claim:?} — the two-tier split in \
             content_addressable_ratchet.rs rests on that statement"
        );
    }
}

/// **The detector must be shown to detect.** An absence check that has never
/// caught anything is decoration: it reports success whether the property
/// holds or the scan is broken, and those look identical from the outside.
///
/// So: a seeded journal that is persisted and unaddressed, proven found —
/// and proven to discriminate in the three other directions, because a
/// detector that fires on everything is as useless as one that fires on
/// nothing.
#[test]
fn the_journal_scanner_finds_a_seeded_unaddressed_journal() {
    let seeded = r#"
        #[derive(Serialize)]
        pub struct FixtureRecord { pub note: String }
        pub fn append(path: &Path, r: &FixtureRecord) -> std::io::Result<()> {
            let line = serde_json::to_string(r).map_err(std::io::Error::other)?;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
            writeln!(f, "{line}")
        }
    "#;
    assert_eq!(
        journal_tier(seeded),
        Some(Tier::Unaddressed),
        "the scanner MISSED a seeded persisted-and-unaddressed record — every \
         green it has ever reported was uninformative"
    );

    // Upward: addressing and chaining are both recognized, or the ratchet
    // would count paid debt as unpaid forever and no one could lower it.
    assert_eq!(
        journal_tier(&format!(
            "{seeded}\nimpl ContentAddressable for FixtureRecord {{}}"
        )),
        Some(Tier::AddressedUnchained)
    );
    assert_eq!(
        journal_tier(&format!(
            "{seeded}\nuse content_addressable::MerkleNode;\nimpl ContentAddressable for FixtureRecord {{}}"
        )),
        Some(Tier::Chained)
    );

    // Downward: not everything is a journal, and a journal quoted in a
    // comment is prose. Both matter — a detector that fires on either would
    // flood the table with rows nobody can pay, which is how a gate gets an
    // `#[allow]`-shaped escape hatch bolted onto it.
    assert_eq!(
        journal_tier("pub struct InMemoryOnly { pub n: usize }"),
        None,
        "an in-memory type is not a journal"
    );
    assert_eq!(
        journal_tier("// let s = serde_json::to_string(r)?;\n// OpenOptions::new().append(true);"),
        None,
        "a journal described in a comment is documentation, not a violation"
    );
}

/// The seeded fixture proves the classifier; this proves it against the real
/// tree, so the two cannot both be satisfied by a scan that reads nothing.
#[test]
fn the_journal_scanner_sees_the_real_violation_it_was_built_for() {
    let found = journals();
    assert_eq!(
        found.get("newt-core/src/flight_recorder.rs"),
        Some(&Tier::Unaddressed),
        "`unconfined.jsonl` is a real unaddressed journal that feeds an \
         authority proposal, and it is this ratchet's anchor in the real tree \
         now that `denial_journal` has paid (#2088). If it is genuinely \
         addressed now, lower UNADDRESSED_JOURNALS and repoint this test at \
         another row; if it is not in the map at all, the scan stopped \
         seeing it.\n\
         Journals now: {found:#?}"
    );
    // The row this ratchet was BUILT for, kept as an assertion rather than a
    // memory: `denial_journal` paid by delegating to the chained sink, so it
    // must not reappear in any tier.
    assert_eq!(
        found.get("newt-core/src/denial_journal.rs"),
        None,
        "`denial_journal` was migrated onto `event_journal`'s chain (#2088) \
         and must no longer persist its own records. Its reappearance here \
         means that migration regressed.\n\
         Journals now: {found:#?}"
    );
    assert_eq!(
        found.get("newt-core/src/event_journal.rs"),
        Some(&Tier::Chained),
        "the one conformant journal must classify as conformant, or the \
         scanner is calling everything a violation"
    );
}

/// **Fails open, proven.** Narrow the scan to a directory with no sources and
/// it must FAIL, not report a clean tree. This is the gut-proof kept as a
/// test: the guard in `journals_under` is the only thing standing between an
/// absence check and a permanent vacuous green.
#[test]
#[should_panic(expected = "read NO production sources")]
fn a_scan_that_reads_nothing_fails_instead_of_passing() {
    let empty = workspace_root().join("newt-core/tests/no-such-directory");
    assert!(!empty.exists(), "the empty-scan probe needs a missing path");
    let _ = journals_under(&empty);
}
