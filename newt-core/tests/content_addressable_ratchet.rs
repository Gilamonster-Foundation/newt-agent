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
//! frozen core this workspace pins (`Cargo.toml` takes the default features
//! only, and merkle/store are `unstable-*`-gated), so a conversion to one of
//! those is a dependency decision as well as a code change — raise it rather
//! than assuming it is available.

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
        // Tests comparing digests; convert with their subject.
        ("newt-core/src/agentic/tools.rs", 5),
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
    let root = workspace_root();
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    let mut stack = vec![root.clone()];
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
            let n = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| predicate(l))
                .count();
            if n > 0 {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                found.insert(rel, n);
            }
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
