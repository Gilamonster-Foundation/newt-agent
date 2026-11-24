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
//! * The default features of the named crate, unless FEATURE_LEDGER names a
//!   separate feature selection. The
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
    ("newt-fdguard", 10),
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
    ("newt-policy", 100),
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
    closure_with(package, &[])
}

fn closure_with(package: &str, args: &[&str]) -> BTreeSet<String> {
    let mut command = vec![
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
    ];
    command.extend_from_slice(args);
    parse_tree(package, &cargo(&command))
}

fn parse_tree(package: &str, output: &str) -> BTreeSet<String> {
    let tree: BTreeSet<_> = output
        .lines()
        .map(|line| line.trim_end_matches(" (*)").to_string())
        .collect();
    assert!(
        tree.iter()
            .any(|line| line.starts_with(&format!("{package} v"))),
        "cargo tree did not name its root {package}: {tree:?}"
    );
    tree
}

/// Feature-specific ceilings. MAY ONLY GO DOWN. Measured 2026-11-24 UTC at
/// bc6d45582d43 with the P15 manifest flip applied: 417 packages, ceiling 430.
/// Re-measure with the header command plus `--no-default-features --features markdown`.
const FEATURE_LEDGER: &[(&str, &[&str], usize)] = &[(
    "newt-core",
    &["--no-default-features", "--features", "markdown"],
    430,
)];

type Selection = (&'static str, &'static [&'static str]);

fn rows() -> impl Iterator<Item = (Selection, usize)> {
    LEDGER
        .iter()
        .map(|&(package, ceiling)| ((package, &[][..]), ceiling))
        .chain(
            FEATURE_LEDGER
                .iter()
                .map(|&(package, args, ceiling)| ((package, args), ceiling)),
        )
}

fn label((package, args): Selection) -> String {
    if args.is_empty() {
        package.to_string()
    } else {
        format!("{package} (confine off)")
    }
}

fn measure_rows(
    rows: impl Iterator<Item = (Selection, usize)>,
    measure: impl Fn(Selection) -> usize,
) -> BTreeMap<Selection, usize> {
    rows.map(|(selection, _)| (selection, measure(selection)))
        .collect()
}

/// Measure each package AND feature selection once; neither core row hides the other.
fn actuals() -> &'static BTreeMap<Selection, usize> {
    static ACTUALS: OnceLock<BTreeMap<Selection, usize>> = OnceLock::new();
    ACTUALS
        .get_or_init(|| measure_rows(rows(), |(package, args)| closure_with(package, args).len()))
}

#[test]
fn closure_is_within_the_declared_ceiling() {
    let over: Vec<String> = rows()
        .filter(|(selection, ceiling)| actuals()[selection] > *ceiling)
        .map(|(selection, ceiling)| {
            format!(
                "{}: {} > {ceiling}",
                label(selection),
                actuals()[&selection]
            )
        })
        .collect();
    assert!(
        over.is_empty(),
        "closure grew past its ceiling. A growing closure is a finding: find what came in. \
         Do not raise the ceiling.\n{over:#?}"
    );
}

#[test]
fn ledger_is_not_stale() {
    let stale: Vec<String> = rows()
        .filter(|(selection, ceiling)| ceiling.saturating_sub(actuals()[selection]) > SLACK)
        .map(|(selection, ceiling)| {
            format!(
                "{}: ceiling {ceiling}, measured {}; lower the ceiling",
                label(selection),
                actuals()[&selection]
            )
        })
        .collect();
    assert!(stale.is_empty(), "a ceiling is more than {SLACK} above its crate's closure, so it no longer binds:\n{stale:#?}");
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

const BRIDLE_CRATES: [&str; 5] = [
    "agent-bridle",
    "agent-bridle-core",
    "agent-bridle-fdguard",
    "agent-bridle-tool-shell",
    "agent-bridle-tool-web",
];
const DIRECT_BRIDLE: [&str; 3] = [
    "agent-bridle",
    "agent-bridle-tool-shell",
    "agent-bridle-tool-web",
];

fn is_bridle_crate(name: &str) -> bool {
    name == "agent-bridle" || name.starts_with("agent-bridle-")
}

fn bridle_names(tree: &BTreeSet<String>) -> BTreeSet<&str> {
    tree.iter()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| is_bridle_crate(name))
        .collect()
}

/// Inspect every declaration, including duplicate target rows and renamed local keys.
fn bridle_manifest(meta: &serde_json::Value) -> Result<(), String> {
    let packages = meta["packages"]
        .as_array()
        .ok_or("missing packages array")?;
    let core: Vec<_> = packages
        .iter()
        .filter(|p| p["name"] == "newt-core")
        .collect();
    if core.len() != 1 {
        return Err("metadata must name exactly one newt-core package".into());
    }
    let core = core[0];
    let deps = core["dependencies"]
        .as_array()
        .ok_or("missing dependency array")?;
    let features = core["features"]
        .as_object()
        .ok_or("missing features table")?;
    let features: BTreeMap<&str, Vec<&str>> = features
        .iter()
        .map(|(name, entries)| {
            let entries = entries.as_array().ok_or("feature must be an array")?;
            let entries = entries
                .iter()
                .map(|v| v.as_str().ok_or("feature entries must be strings"))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((name.as_str(), entries))
        })
        .collect::<Result<_, String>>()?;
    let confine = features.get("confine").ok_or("missing confine feature")?;
    for name in ["default", "windows-appcontainer", "macos-seatbelt"] {
        if !features.get(name).is_some_and(|f| f.contains(&"confine")) {
            return Err(format!("{name} must include confine"));
        }
    }
    let mut names = BTreeSet::new();
    let mut problems = Vec::new();
    for dep in deps {
        let name = dep["name"].as_str().ok_or("dependency has no name")?;
        if !is_bridle_crate(name) {
            continue;
        }
        names.insert(name);
        let local = match dep.get("rename") {
            Some(serde_json::Value::String(local)) => local.as_str(),
            Some(serde_json::Value::Null) => name,
            _ => return Err(format!("{name}: missing or invalid rename field")),
        };
        let activation = format!("dep:{local}");
        if dep.get("kind") != Some(&serde_json::Value::Null)
            || dep["optional"] != true
            || !confine.contains(&activation.as_str())
        {
            problems.push(format!("{name} ({local}, target {}): must be normal, optional, and in confine as {activation}", dep["target"]));
        }
        for (feature, entries) in &features {
            if ["confine", "windows-appcontainer", "macos-seatbelt"].contains(feature) {
                continue;
            }
            if entries.iter().any(|entry| {
                *entry == activation
                    || *entry == local
                    || entry.starts_with(&format!("{local}/"))
                    || entry.starts_with(&format!("{local}?/"))
            }) {
                problems.push(format!("{feature} activates {local} outside confine"));
            }
        }
    }
    if names != BTreeSet::from(DIRECT_BRIDLE) {
        problems.push(format!(
            "expected direct dependencies {DIRECT_BRIDLE:?}, found {names:?}"
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// Cargo-backed integration evidence, over both isolated feature selections.
#[test]
fn the_confine_off_closure_names_no_bridle_crate() {
    let mut leaks = Vec::new();
    for args in [
        &["--no-default-features"][..],
        &["--no-default-features", "--features", "markdown"],
    ] {
        let tree = closure_with("newt-core", args);
        let found = bridle_names(&tree);
        if !found.is_empty() {
            leaks.push(format!("{args:?}: {found:?}"));
        }
    }
    assert!(
        leaks.is_empty(),
        "confine-off closure contains bridle:\n{}",
        leaks.join("\n")
    );
}

/// Positive integration control: the same walker sees every exact default name.
#[test]
fn the_default_closure_names_every_bridle_crate() {
    let tree = closure("newt-core");
    assert_eq!(bridle_names(&tree), BTreeSet::from(BRIDLE_CRATES));
}

/// Cargo metadata integration evidence covers normal/dev/build and target rows.
#[test]
fn every_direct_bridle_dependency_is_optional_behind_confine() {
    let meta = serde_json::from_str(&cargo(&["metadata", "--no-deps", "--format-version", "1"]))
        .expect("cargo metadata prints JSON");
    assert_eq!(bridle_manifest(&meta), Ok(()));
}

/// Literal samples only: no Cargo, filesystem, network, host discovery or clock.
#[test]
fn the_bridle_checks_read_real_cargo_output() {
    let selections: [Selection; 2] = [
        ("newt-core", &[]),
        ("newt-core", &["--no-default-features"]),
    ];
    let measured = measure_rows(selections.into_iter().map(|s| (s, 0)), |(_, args)| {
        if args.is_empty() {
            100
        } else {
            20
        }
    });
    assert_eq!(measured.len(), 2);
    assert_eq!(measured[&selections[0]], 100);
    assert_eq!(measured[&selections[1]], 20);
    let tree = parse_tree("newt-core", "newt-core v0.8.6 (/w/newt-core)\nagent-bridle v0.8.0-rc.7\nagent-bridle-core v0.8.0-rc.7 (/w/vendor/agent-bridle-core)\nagent-bridle-fdguard v0.8.0-rc.7\nagent-bridle-tool-shell v0.8.0-rc.7\nagent-bridle-tool-web v0.8.0-rc.7\nagent-bridle v0.8.0-rc.7 (*)\nserde_derive v1.0.0 (proc-macro)\nagent-bridled v1.0.0\nunrelated v1.0.0\n");
    assert_eq!(tree.len(), 9);
    assert_eq!(bridle_names(&tree), BTreeSet::from(BRIDLE_CRATES));
    assert!(!is_bridle_crate("agent-bridled"));
    assert!(std::panic::catch_unwind(|| parse_tree("newt-core", "")).is_err());
    assert!(std::panic::catch_unwind(|| parse_tree("newt-core", "other v1.0.0")).is_err());
    let good = serde_json::json!({"packages": [{"name": "newt-core", "dependencies": [
        {"name": "agent-bridle", "rename": null, "kind": null, "optional": true, "target": null},
        {"name": "agent-bridle-tool-shell", "rename": null, "kind": null, "optional": true, "target": null},
        {"name": "agent-bridle-tool-web", "rename": null, "kind": null, "optional": true, "target": null}
    ], "features": {"confine": ["dep:agent-bridle", "dep:agent-bridle-tool-shell", "dep:agent-bridle-tool-web"],
        "default": ["markdown", "confine"], "markdown": [],
        "windows-appcontainer": ["confine", "agent-bridle/windows-appcontainer"],
        "macos-seatbelt": ["confine", "agent-bridle/macos-seatbelt"]}}]});
    assert_eq!(bridle_manifest(&good), Ok(()));
    for bad in [serde_json::json!({}), serde_json::json!({"packages": []})] {
        assert!(bridle_manifest(&bad).is_err());
    }
    for (pointer, value) in [
        ("/dependencies", serde_json::json!([])),
        ("/dependencies/0/optional", serde_json::json!(false)),
        ("/dependencies/0/kind", serde_json::json!("dev")),
        ("/dependencies/0/kind", serde_json::json!("build")),
        ("/features", serde_json::Value::Null),
        ("/features/confine", serde_json::json!([])),
        ("/features/default", serde_json::json!(["markdown"])),
        (
            "/features/windows-appcontainer",
            serde_json::json!(["agent-bridle/windows-appcontainer"]),
        ),
        ("/features/macos-seatbelt", serde_json::json!([])),
        (
            "/features/markdown",
            serde_json::json!(["dep:agent-bridle"]),
        ),
        (
            "/features/markdown",
            serde_json::json!(["agent-bridle/shell"]),
        ),
    ] {
        let mut bad = good.clone();
        *bad["packages"][0].pointer_mut(pointer).unwrap() = value;
        assert!(bridle_manifest(&bad).is_err(), "accepted {pointer}: {bad}");
    }
    let mut target = good.clone();
    target["packages"][0]["dependencies"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "name": "agent-bridle", "rename": "bridle_alias", "kind": null, "optional": true,
            "target": "cfg(windows)"
        }));
    assert!(bridle_manifest(&target)
        .unwrap_err()
        .contains("bridle_alias"));
    target["packages"][0]["features"]["confine"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("dep:bridle_alias"));
    assert_eq!(bridle_manifest(&target), Ok(()));
    for field in ["optional", "kind"] {
        let mut bad = target.clone();
        bad["packages"][0]["dependencies"][3][field] = if field == "optional" {
            serde_json::json!(false)
        } else {
            serde_json::json!("dev")
        };
        assert!(bridle_manifest(&bad).unwrap_err().contains("bridle_alias"));
    }
}
