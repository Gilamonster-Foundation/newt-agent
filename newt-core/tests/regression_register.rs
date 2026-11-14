//! **Ratchet on the regression register (`docs/testing/regressions.toml`).**
//!
//! Each entry is a regression an operator saw in a live run, with the mocked
//! test that guards it on every PR and the real-resource test that grounds
//! that mock. This test keeps the register honest:
//!
//! - every named test exists and carries a test attribute; guards are enabled
//!   in the unit tier and grounds are `#[ignore]`d for the weekly/release tier;
//! - `guard` and `ground` are different tests, so one test cannot stand in for
//!   both tiers;
//! - the entry count may only go UP ([`REGISTERED`]), so an entry cannot be
//!   dropped to make a failing guard go away.
//!
//! It reads source text and does not run the named tests: whether a guard
//! PASSES is the test suite's job, and whether a `ground` test skipped (no
//! kernel fence, no tmux) is visible only in that test's own output.

use std::collections::BTreeSet;

// Embedded at compile time: checking the register performs no filesystem I/O.
const SOURCES: &[(&str, &str)] = &[
    (
        "newt-core/src/agentic/tools/native_git.rs",
        include_str!("../src/agentic/tools/native_git.rs"),
    ),
    (
        "newt-tui/src/cockpit/presenter.rs",
        include_str!("../../newt-tui/src/cockpit/presenter.rs"),
    ),
];

/// Entries in the register. Raise it in the PR that adds an entry; it never
/// goes down.
const REGISTERED: usize = 4;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    regression: Vec<Regression>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Regression {
    id: String,
    seen: String,
    symptom: String,
    guard: TestRef,
    ground: TestRef,
}

#[derive(serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct TestRef {
    file: String,
    test: String,
}

/// Why `test` in `source` is not a live test, or `None` when it is one. The
/// attributes are read from the lines directly above the `fn`, up to the
/// previous blank line or closing brace.
fn not_a_live_test(source: &str, test: &str, ground: bool) -> Option<&'static str> {
    let lines: Vec<&str> = source.lines().collect();
    let needles = [format!("fn {test}("), format!("async fn {test}(")];
    let Some(at) = lines.iter().position(|line| {
        let line = line.trim_start();
        needles.iter().any(|needle| line.starts_with(needle))
    }) else {
        return Some("no such fn");
    };
    let attributes: Vec<&str> = lines[..at]
        .iter()
        .rev()
        .map(|line| line.trim())
        .take_while(|line| line.starts_with("#[") || line.starts_with("///"))
        .collect();
    let ignored = attributes.iter().any(|line| line.starts_with("#[ignore"));
    if ignored != ground {
        return Some(if ground {
            "ground must be #[ignore]d for the real-resource tier"
        } else {
            "guard is #[ignore]d"
        });
    }
    let is_test = |line: &&str| *line == "#[test]" || line.starts_with("#[tokio::test");
    (!attributes.iter().any(is_test)).then_some("it has no test attribute")
}

#[test]
fn every_registered_regression_names_a_live_guard_and_a_live_ground() {
    let register: Register = toml::from_str(include_str!("../../docs/testing/regressions.toml"))
        .expect("the regression register parses");

    let mut problems = Vec::new();
    let mut ids = BTreeSet::new();
    for entry in &register.regression {
        if !ids.insert(entry.id.as_str()) {
            problems.push(format!("{}: duplicate id", entry.id));
        }
        if entry.symptom.trim().is_empty() || entry.seen.trim().is_empty() {
            problems.push(format!("{}: symptom and seen are required", entry.id));
        }
        if entry.guard == entry.ground {
            problems.push(format!("{}: guard and ground are the same test", entry.id));
        }
        for (tier, test) in [("guard", &entry.guard), ("ground", &entry.ground)] {
            let found = match SOURCES.iter().find(|(file, _)| *file == test.file) {
                Some((_, source)) => not_a_live_test(source, &test.test, tier == "ground"),
                None => Some("the source must be embedded in SOURCES"),
            };
            if let Some(why) = found {
                problems.push(format!(
                    "{}: {tier} `{}` in {}: {why}",
                    entry.id, test.test, test.file
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "regression register:\n{}",
        problems.join("\n")
    );
    assert_eq!(
        register.regression.len(),
        REGISTERED,
        "the register holds {} entries and REGISTERED says {REGISTERED}: raise REGISTERED when \
         adding an entry; an entry is never removed",
        register.regression.len()
    );
}

#[test]
fn the_checker_rejects_a_missing_an_ignored_and_an_unmarked_test() {
    let source = "\
#[test]
fn live() {}

/// Documented.
#[tokio::test]
async fn live_async() {}

#[test]
#[ignore]
fn switched_off() {}

fn helper() {}
";
    assert_eq!(not_a_live_test(source, "switched_off", true), None);
    assert_eq!(
        not_a_live_test(source, "live", true),
        Some("ground must be #[ignore]d for the real-resource tier")
    );
    assert_eq!(
        not_a_live_test(source, "helper", true),
        Some("ground must be #[ignore]d for the real-resource tier")
    );
    assert_eq!(not_a_live_test(source, "live", false), None);
    assert_eq!(not_a_live_test(source, "live_async", false), None);
    assert_eq!(not_a_live_test(source, "absent", false), Some("no such fn"));
    assert_eq!(
        not_a_live_test(source, "switched_off", false),
        Some("guard is #[ignore]d")
    );
    assert_eq!(
        not_a_live_test(source, "helper", false),
        Some("it has no test attribute")
    );
}
