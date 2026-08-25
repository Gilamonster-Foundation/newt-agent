//! **Tenacity attributes family from typed card evidence — never from the
//! model name.**
//!
//! The card-capability pivot (#1818/#1819) made model names labels, never
//! evidence: family reaches tenacity only through the typed seam
//! (`ResolvedCapabilities::family_for_route` → `set_active_model_family`).
//! The legacy channel — `family_for` matching a configured family KEY as a
//! case-insensitive substring of the model NAME, and `attribute_active_family`
//! looking a card up by model name — was left caller-less by the pivot. This
//! ratchet pins its removal and guards the seam against reintroduction: a
//! name-shaped inference path in tenacity silently resurrects exactly the
//! attribution-by-coincidence class the pivot closed (a model *named*
//! `my-nemotron-alias` must get NO family without a card saying so).

use std::path::Path;

fn source(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Name-keyed inference machinery that must never return to tenacity. Each
/// needle is a distinct resurrection vector: the inference fn, the installer
/// that called it, the substring match itself, the doc language that licensed
/// it, and the name-keyed card lookup that fed it.
const BANNED_IN_TENACITY: &[&str] = &[
    "fn family_for(",
    "attribute_active_family",
    "lname.contains",
    "substring",
    "builtin_card",
];

/// The surviving exact-family surface: the typed installer and the
/// case-insensitive EQUALITY match on the family LABEL (equality on a label an
/// operator wrote is identity; containment in a model name is inference).
const REQUIRED_IN_TENACITY: &[&str] = &[
    "pub fn set_active_model_family(",
    "fn resolve(&self, family: Option<&str>)",
    "eq_ignore_ascii_case",
];

/// The needle bans above catch the verbatim-revert vector but are evadable
/// by respelling (a renamed local, a reworded doc). This is the structural
/// backstop: outside its tests, tenacity has NO input that carries a model
/// name at all — its public surface takes a *family* (`resolve`,
/// `set_active_model_family`) — so any inference would first have to add a
/// name-bearing parameter, and that shape is banned here. The tests module is
/// exempt (fixture literals like `"nemotron-super"` are labels under test,
/// not inputs to tenacity).
const BANNED_OUTSIDE_TESTS: &[&str] = &["model: &str", "model_name"];

#[test]
fn tenacity_takes_no_model_name_input_outside_its_tests() {
    let src = source("src/tenacity.rs");
    let non_test = src
        .split("#[cfg(test)]")
        .next()
        .expect("split always yields a first segment");
    // The exemption must exempt something real, or this scans the whole file
    // by accident forever.
    assert!(
        src.contains("#[cfg(test)]"),
        "src/tenacity.rs lost its #[cfg(test)] marker — the structural scan \
         below would silently cover nothing it thinks it covers"
    );
    for needle in BANNED_OUTSIDE_TESTS {
        assert!(
            !non_test.contains(needle),
            "newt-core/src/tenacity.rs (outside tests) contains `{needle}` — \
             tenacity's surface takes a FAMILY, never a model name; a \
             name-bearing input is the first step of every \
             family-by-coincidence reintroduction (#1820)"
        );
    }
}

#[test]
fn tenacity_has_no_model_name_inference_channel() {
    let src = source("src/tenacity.rs");
    for needle in BANNED_IN_TENACITY {
        assert!(
            !src.contains(needle),
            "newt-core/src/tenacity.rs contains `{needle}` — a model-name \
             inference channel in tenacity reintroduces family-by-coincidence; \
             family must arrive through set_active_model_family from typed \
             resolved-card metadata"
        );
    }
    for needle in REQUIRED_IN_TENACITY {
        assert!(
            src.contains(needle),
            "newt-core/src/tenacity.rs no longer contains `{needle}` — the \
             typed exact-family seam is the ONLY sanctioned attribution path; \
             removing it without replacing this ratchet orphans per-family \
             tenacity entirely"
        );
    }
}

/// The seam is only meaningful if the lanes actually feed it: both chat and
/// headless solve must derive the family from the route-gated typed decision
/// and install it. An absence-only ratchet would stay green if the whole
/// feature were deleted — this half keeps it honest.
#[test]
fn both_lanes_feed_the_typed_family_seam() {
    for (rel, lane) in [
        ("../newt-tui/src/chat.rs", "chat"),
        ("../newt-cli/src/solve.rs", "solve"),
    ] {
        let src = source(rel);
        for needle in [".family_for_route(", "set_active_model_family("] {
            assert!(
                src.contains(needle),
                "{lane} lane ({rel}) no longer contains `{needle}` — family \
                 attribution must flow route-gated typed metadata into \
                 tenacity in BOTH lanes, or per-family defaults silently \
                 diverge between chat and headless (the #1139 class)"
            );
        }
    }
}

/// The guards above read real files — a scanner that reads nothing reports
/// success forever.
#[test]
fn the_ratchet_reads_the_real_sources() {
    assert!(source("src/tenacity.rs").contains("TenacityRuntimeSnapshot"));
    assert!(source("../newt-tui/src/chat.rs").contains("fn run_chat"));
    assert!(source("../newt-cli/src/solve.rs").contains("TurnDriverConfig::new"));
}
