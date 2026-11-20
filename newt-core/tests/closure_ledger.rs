//! **Closure ledger.** Every workspace crate carries a declared ceiling on the
//! number of crates a consumer resolves for it, and a ceiling may only go down.
//!
//! `newt-core` is a hub: most crates here depend on it and inherit its whole
//! closure, so a crate that only wants a small part still pays for all of it
//! (#2233, epic #3140). Without a number on each crate, an extraction that
//! regresses, or a new dependency that arrives through the hub, is invisible.
//!
//! # What is counted
//!
//! The distinct `name version` packages in
//! `cargo tree -p <crate> -e normal,build --prefix none --target <TARGET>`.
//!
//! * Build dependencies count: a build script runs with the full authority of
//!   the building user before a line of the crate compiles. Dev dependencies
//!   do not: they impose nothing on a consumer.
//! * Repeats are collapsed. `cargo tree` marks a repeated subtree with a
//!   trailing ` (*)`, so `| sort -u | wc -l` counts such a package twice, once
//!   plain and once marked. Stripping the marker first is the count of
//!   packages.
//! * The target is pinned, so the same number comes out on every host.
//! * The default features of the named crate, as a consumer would get them. The
//!   walker in `newt-interaction/tests/guard.rs` reads a workspace-wide
//!   `--all-features` resolve instead, which answers a different question (is a
//!   forbidden crate reachable under any feature), so it is not reused here.
//!
//! # Re-measuring
//!
//! ```text
//! cargo tree -p <crate> -e normal,build --prefix none --target x86_64-unknown-linux-gnu \
//!     | sed 's/ (\*)$//' | sort -u | wc -l
//! ```
//!
//! A ceiling is that count rounded up to the next multiple of 10, with at least
//! 5 of headroom. When a crate shrinks, lower its row. When a crate grows past
//! its row, that is a finding to report, not a number to raise.

// The count is for one Linux triple, so one Linux host is enough to hold it.
// Off Linux this file is empty rather than fetching a foreign platform's
// manifests.
#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::sync::OnceLock;

/// The triple the counts are for. Not the host's: the same number everywhere.
const TARGET: &str = "x86_64-unknown-linux-gnu";

/// How far below its ceiling a crate may sit before the row must be lowered.
/// Loose enough to absorb a third-party graph moving under a `cargo update`,
/// tight enough that a real extraction forces the ratchet down.
const SLACK: usize = 15;

/// Declared ceiling on each crate's closure. MAY ONLY GO DOWN.
///
/// PACKAGE names, not directory names: `tests/common` is `tests-common`. Set
/// 2026-11-19 at `7987a6fa`.
const LEDGER: &[(&str, usize)] = &[
    ("newt-launch", 10),
    ("tests-pty", 10),
    ("newt-refactor-evidence", 10),
    ("newt-router", 20),
    ("newt-skills", 30),
    ("plugins-protocol", 40),
    ("agent-frame", 50),
    ("newt-interaction", 50),
    ("newt-tuner", 50),
    ("newt-data", 60),
    ("newt-harness", 60),
    ("agent-harness-py", 70),
    ("agent-toolchain", 90),
    ("newt-flock", 100),
    ("newt-provider-openai", 130),
    ("newt-mcp-data", 160),
    ("newt-carried-tools", 170),
    ("newt-core", 530),
    ("newt-git", 530),
    ("newt-identity", 530),
    ("newt-mcp-client", 530),
    ("newt-tools", 530),
    ("newt-inference", 540),
    ("newt-scheduler", 540),
    ("newt-coder", 540),
    ("newt-acp-worker", 540),
    ("tests-common", 540),
    ("newt-mcp-server", 540),
    ("newt-eval", 550),
    ("newt-tui", 550),
    ("newt-agent-py", 560),
    ("newt-agent", 650),
];

/// Run `cargo <args>` from the package directory (cargo finds the workspace)
/// and return stdout.
fn cargo(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO"))
        .args(args)
        .output()
        .expect("cargo runs");
    assert!(
        out.status.success(),
        "cargo {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("cargo prints utf-8")
}

/// The distinct `name version` packages `package` resolves.
///
/// `--color never`: CI sets `CARGO_TERM_COLOR=always`, under which `cargo tree`
/// colours the ` (*)` marker, so it is not stripped and the count comes back as
/// the double count (`newt-core` 694, not 523).
fn closure(package: &str) -> BTreeSet<String> {
    cargo(&[
        "tree",
        "--color",
        "never",
        "-p",
        package,
        "-e",
        "normal,build",
        "--prefix",
        "none",
        "--target",
        TARGET,
    ])
    .lines()
    .map(|line| line.trim_end_matches(" (*)").to_string())
    .collect()
}

/// Every ledger crate's closure size, measured once for all the tests here.
fn actuals() -> &'static BTreeMap<&'static str, usize> {
    static ACTUALS: OnceLock<BTreeMap<&'static str, usize>> = OnceLock::new();
    ACTUALS.get_or_init(|| {
        LEDGER
            .iter()
            .map(|&(package, _)| (package, closure(package).len()))
            .collect()
    })
}

#[test]
fn closure_is_within_the_declared_ceiling() {
    let over: Vec<String> = LEDGER
        .iter()
        .filter(|&&(package, ceiling)| actuals()[package] > ceiling)
        .map(|&(package, ceiling)| format!("{package}: {} > {ceiling}", actuals()[package]))
        .collect();
    assert!(
        over.is_empty(),
        "closure grew past its ceiling. A growing closure is a finding: find what came in. \
         Do not raise the ceiling.\n{over:#?}"
    );
}

#[test]
fn ledger_is_not_stale() {
    let stale: Vec<String> = LEDGER
        .iter()
        .filter(|&&(package, ceiling)| ceiling.saturating_sub(actuals()[package]) > SLACK)
        .map(|&(package, ceiling)| {
            format!(
                "{package}: ceiling {ceiling}, measured {}; lower the ceiling",
                actuals()[package]
            )
        })
        .collect();
    assert!(
        stale.is_empty(),
        "a ceiling is more than {SLACK} above its crate's closure, so it no longer binds:\n{stale:#?}"
    );
}

/// `closure_is_within_the_declared_ceiling` asserts "a number is small", which a
/// walker that returns nothing satisfies. Point the same walker at a crate known
/// to be large and require it to come back naming crates.
#[test]
fn the_walker_sees_a_real_dependency_graph() {
    let tui = closure("newt-tui");
    assert!(
        tui.len() > 100,
        "newt-tui's closure came back with {} packages; the walker is reading nothing",
        tui.len()
    );
    for named in ["ratatui", "tokio"] {
        assert!(
            tui.iter().any(|p| p.starts_with(&format!("{named} v"))),
            "{named} is missing from newt-tui's closure; the walker cannot see the graph"
        );
    }
}

#[test]
fn every_workspace_member_is_in_the_ledger() {
    let meta: serde_json::Value =
        serde_json::from_str(&cargo(&["metadata", "--no-deps", "--format-version", "1"]))
            .expect("cargo metadata prints json");
    let members: BTreeSet<&str> = meta["packages"]
        .as_array()
        .expect("packages is an array")
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    // An absence check over an empty list passes having read nothing.
    assert!(!members.is_empty(), "read no workspace members");

    let ledger: BTreeSet<&str> = LEDGER.iter().map(|&(package, _)| package).collect();
    let missing: Vec<_> = members.difference(&ledger).collect();
    let gone: Vec<_> = ledger.difference(&members).collect();
    assert!(
        missing.is_empty() && gone.is_empty(),
        "the ledger and the workspace disagree. Not in the ledger: {missing:?}. \
         In the ledger but not a member: {gone:?}"
    );
}
