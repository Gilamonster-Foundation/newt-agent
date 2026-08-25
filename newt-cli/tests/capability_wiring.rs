//! **Headless `solve` consumes the capability sidecar, like every lane.**
//!
//! Independent review found (twice) that `solve` wired capabilities
//! differently from the TUI: first a missing `emits_leading_reasoning`
//! hand-off, then raw inline-only `BackendConfig` accessors after the
//! sidecar landed. Divergence between lanes is silent precisely where nobody
//! watches a stream, so the shape is asserted: `solve.rs` pairs the selected
//! backend with ITS OWN provenance receipt, seeds `ResolvedCapabilities`
//! from the receipt's binding (never a re-derived one), decides
//! `for_route` against the typed route destination, renders prose through
//! the ONE display owner, and takes every capability from the DECISION —
//! never from raw accessors, never from the pre-pivot principal-only API.

const REQUIRED: &[&str] = &[
    // The receipt-seeded sidecar: binding evidence comes from the slot's
    // own receipt, paired by the shared index selector.
    "&picked.receipt.binding,",
    "ResolvedCapabilities::resolve(",
    // The destination-first typed decision.
    "BackendDestination::of(backend)",
    ".for_route(&destination, principal)",
    // The one prose owner (shared with the TUI's display seam).
    "newt_tui::applicability_prose(decision.applicability())",
    // Typed family attribution — the anti-substring seam.
    ".family_for_route(&destination, principal)",
    // Every capability read comes from the decision.
    "decision.chat_completions()",
    "decision.reasoning_replay_scope()",
    "decision.emits_leading_reasoning()",
];

/// Shapes that must NOT appear in solve: raw inline-only accessors (the
/// decision owns them) and the pre-pivot capability/attribution APIs (a
/// re-derived binding or a principal-only decision reintroduces the exact
/// silent-rebind / lane-divergence classes the pivot closed).
const BANNED: &[&str] = &[
    "backend.chat_completions_capability()",
    "backend.reasoning_replay_scope()",
    "backend.emits_leading_reasoning()",
    // Pre-pivot decision API (no destination gate).
    ".for_principal(",
    // Pre-pivot prose channel.
    "retarget_notice",
    // A binding re-derived from the (possibly overridden) backend instead
    // of the receipt.
    "CardBindingSeed::from_backend(",
    // (the `attribute_active_family(` ban retired with #1820: the symbol no
    // longer exists, and newt-core/tests/tenacity_exact_family_ratchet.rs now
    // owns the no-name-inference guarantee at the source.)
];

fn solve_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/solve.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn solve_takes_every_capability_from_the_decision() {
    let src = solve_source();
    for needle in REQUIRED {
        assert!(
            src.contains(needle),
            "newt-cli/src/solve.rs no longer contains `{needle}` — headless must \
             consume the same sidecar decision as the TUI, or the two lanes drift \
             silently"
        );
    }
    for needle in BANNED {
        assert!(
            !src.contains(needle),
            "newt-cli/src/solve.rs reads `{needle}` — a raw inline-only accessor \
             beside the sidecar decision reintroduces exactly the lane divergence \
             this guard exists to prevent"
        );
    }
}

/// The guard reads the real file — a scanner that reads nothing reports
/// success forever.
#[test]
fn the_guard_reads_the_real_solve_source() {
    assert!(solve_source().contains("TurnDriverConfig::new"));
}
