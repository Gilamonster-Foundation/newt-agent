//! **Ratchet on the regression register (`docs/testing/regressions.toml`).**
//!
//! Each entry is a regression an operator saw in a live run, with the mocked
//! test that guards it on every PR and the real-resource test that grounds
//! that mock. This test keeps the register honest:
//!
//! - every named test exists in the named file, carries a test attribute, and
//!   is not `#[ignore]`d, so a guard cannot be deleted, renamed away, or
//!   switched off while its entry still claims it;
//! - `guard` and `ground` are different tests, so one test cannot stand in for
//!   both tiers;
//! - the entry count may only go UP ([`REGISTERED`]), so an entry cannot be
//!   dropped to make a failing guard go away.
//!
//! It reads source text and does not run the named tests: whether a guard
//! PASSES is the test suite's job, and whether a `ground` test skipped (no
//! kernel fence, no tmux) is visible only in that test's own output.

use std::collections::BTreeSet;

/// The workspace root: this crate's parent directory.
fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("newt-core has a parent workspace directory")
        .to_path_buf()
}

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
fn not_a_live_test(source: &str, test: &str) -> Option<&'static str> {
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
    if attributes.iter().any(|line| line.starts_with("#[ignore")) {
        return Some("it is #[ignore]d");
    }
    let is_test = |line: &&str| *line == "#[test]" || line.starts_with("#[tokio::test");
    (!attributes.iter().any(is_test)).then_some("it has no test attribute")
}

#[test]
fn every_registered_regression_names_a_live_guard_and_a_live_ground() {
    let root = workspace_root();
    let path = root.join("docs/testing/regressions.toml");
    let text = std::fs::read_to_string(&path).expect("the regression register is readable");
    let register: Register = toml::from_str(&text).expect("the regression register parses");

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
            let found = match std::fs::read_to_string(root.join(&test.file)) {
                Ok(source) => not_a_live_test(&source, &test.test),
                Err(_) => Some("the file is missing"),
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
    assert_eq!(not_a_live_test(source, "live"), None);
    assert_eq!(not_a_live_test(source, "live_async"), None);
    assert_eq!(not_a_live_test(source, "absent"), Some("no such fn"));
    assert_eq!(
        not_a_live_test(source, "switched_off"),
        Some("it is #[ignore]d")
    );
    assert_eq!(
        not_a_live_test(source, "helper"),
        Some("it has no test attribute")
    );
}
