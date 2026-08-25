//! **Headless `solve` consumes the capability sidecar, like every lane.**
//!
//! Independent review found (twice) that `solve` wired capabilities
//! differently from the TUI: first a missing `emits_leading_reasoning`
//! hand-off, then raw inline-only `BackendConfig` accessors after the
//! sidecar landed. Divergence between lanes is silent precisely where nobody
//! watches a stream, so the shape is asserted: `solve.rs` builds
//! `ResolvedCapabilities`, decides `for_principal`, and takes every
//! capability from the DECISION — never from raw accessors.

const REQUIRED: &[&str] = &[
    "ResolvedCapabilities::resolve(",
    ".for_principal(",
    "decision.chat_completions()",
    "decision.reasoning_replay_scope()",
    "decision.emits_leading_reasoning()",
];

/// Raw accessor reads that must NOT appear in solve (the decision owns them).
const BANNED: &[&str] = &[
    "backend.chat_completions_capability()",
    "backend.reasoning_replay_scope()",
    "backend.emits_leading_reasoning()",
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
