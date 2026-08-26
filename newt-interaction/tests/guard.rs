//! **The dependency direction is armed, not asserted in a comment** (A2.0,
//! #1828).
//!
//! Epic #1803 requires this layer to carry *"no Ratatui, crossterm, Axum,
//! HTMX, ammonia, browser, mobile, filesystem, or application dependency"*.
//! Half of that list is crates and half is capabilities, so the guard has
//! two halves:
//!
//! 1. [`the_protocol_crate_has_no_forbidden_dependency`] walks the resolved
//!    dependency closure from this package and refuses a forbidden crate.
//! 2. [`the_protocol_crate_touches_no_ambient_authority`] scans this crate's
//!    own source for `std::fs`, `std::net`, `std::process`, and `std::env` —
//!    "filesystem" is not a crate name, so no closure walk can see it.
//!
//! Each half carries an **anti-vacuous twin** that points the same machinery
//! at a target known to violate it. A guard that cannot fail is decoration,
//! and this repo has the receipts: `newt-mcp-data/Cargo.toml:38-41` documents
//! the same intent in prose and arms nothing.
//!
//! These are ordinary `cargo test`s, so `cargo test --workspace` runs them on
//! every PR — unlike `check-publish-order` (`release.yml:307`), which uses
//! `cargo metadata` but fires only on tags and `release/**`.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Crates this layer must never reach, by the epic's list. `newt-*` is
/// handled separately: ANY workspace crate is an inward-direction violation.
const FORBIDDEN: &[&str] = &[
    "ratatui",
    "crossterm",
    "axum",
    "ammonia",
    "tauri",
    "reqwest",
    "rusqlite",
    "tokio",
    "hyper",
    "wry",
    "webkit2gtk",
];

/// Ambient authority this layer must never take. "filesystem" and
/// "application" are capabilities, not crate names.
const FORBIDDEN_STD: &[&str] = &["std::fs", "std::net", "std::process", "std::env"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("newt-interaction has a parent workspace directory")
        .to_path_buf()
}

/// The transitive NORMAL-dependency closure of `package`, by name.
///
/// Dev-dependencies are excluded by KIND, not by name: they do not ship, so
/// `serde_json`/`toml` in `[dev-dependencies]` are not part of what this
/// crate imposes on a consumer. Build-dependencies are excluded for the same
/// reason.
fn normal_dependency_closure(package: &str) -> BTreeSet<String> {
    let out = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--all-features",
            "--manifest-path",
        ])
        .arg(workspace_root().join("Cargo.toml"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("cargo metadata emits json");

    // id -> name, taken from `packages[]` rather than parsed out of the id.
    // Cargo uses two id spellings — `path+file:///…/newt-core#0.8.0`, whose
    // `#` tail is only the version, and `registry+…#adler2@2.0.1`, whose tail
    // is `name@version`. Reading the declared name sidesteps both.
    let mut name_of = std::collections::BTreeMap::new();
    for pkg in meta["packages"].as_array().into_iter().flatten() {
        let (Some(id), Some(name)) = (pkg["id"].as_str(), pkg["name"].as_str()) else {
            continue;
        };
        name_of.insert(id.to_string(), name.to_string());
    }

    // id -> normal dependency ids
    let mut deps_of = std::collections::BTreeMap::new();
    let nodes = meta["resolve"]["nodes"]
        .as_array()
        .expect("resolve.nodes is an array");
    for node in nodes {
        let id = node["id"].as_str().expect("node id").to_string();
        let mut normal = Vec::new();
        for dep in node["deps"].as_array().into_iter().flatten() {
            let kinds = dep["dep_kinds"].as_array().cloned().unwrap_or_default();
            // `kind: null` is a normal dependency; "dev"/"build" are not.
            if kinds.iter().any(|k| k["kind"].is_null()) {
                normal.push(dep["pkg"].as_str().expect("dep pkg id").to_string());
            }
        }
        deps_of.insert(id, normal);
    }

    let start = name_of
        .iter()
        .find(|(_, name)| name.as_str() == package)
        .map(|(id, _)| id.clone())
        .unwrap_or_else(|| panic!("{package} is not in the resolve graph"));

    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([start]);
    let mut visited = BTreeSet::new();
    while let Some(id) = queue.pop_front() {
        if !visited.insert(id.clone()) {
            continue;
        }
        for dep in deps_of.get(&id).into_iter().flatten() {
            if let Some(name) = name_of.get(dep) {
                seen.insert(name.clone());
            }
            queue.push_back(dep.clone());
        }
    }
    seen
}

fn violations(closure: &BTreeSet<String>) -> Vec<String> {
    closure
        .iter()
        .filter(|name| FORBIDDEN.contains(&name.as_str()) || name.starts_with("newt-"))
        .cloned()
        .collect()
}

#[test]
fn the_protocol_crate_has_no_forbidden_dependency() {
    let closure = normal_dependency_closure("newt-interaction");
    assert!(
        !closure.is_empty(),
        "the closure came back empty — the walker found nothing to check"
    );
    let found = violations(&closure);
    assert!(
        found.is_empty(),
        "newt-interaction reaches forbidden crates {found:?} — the inward \
         layer must depend on none of them, and on no newt-* crate. Closure: \
         {closure:?}"
    );
}

/// **Anti-vacuous twin.** The same walker, pointed at a package that really
/// does depend on crossterm (`newt-core/Cargo.toml:76`, non-optional), must
/// report it. If this passes vacuously, so does the guard above.
#[test]
fn the_guard_would_notice_a_forbidden_dependency() {
    let closure = normal_dependency_closure("newt-core");
    let found = violations(&closure);
    assert!(
        found.iter().any(|name| name == "crossterm"),
        "the closure walker failed to see crossterm in newt-core — it cannot \
         be trusted to see one in newt-interaction. Found: {found:?}"
    );
}

/// Every `.rs` line under `dir`'s `src/`, outside `#[cfg(test)]`, as
/// (path, line).
fn production_lines(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.join("src")];
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                stack.push(entry.path());
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut in_test = false;
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("#[cfg(test)]") {
                in_test = true;
            }
            if trimmed.starts_with("//") || in_test {
                continue;
            }
            out.push((path.clone(), line.to_string()));
        }
    }
    out
}

#[test]
fn the_protocol_crate_touches_no_ambient_authority() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lines = production_lines(root);
    assert!(
        lines.len() > 50,
        "the source scan saw only {} lines — it is not reading this crate",
        lines.len()
    );
    let found: Vec<String> = lines
        .iter()
        .filter(|(_, line)| FORBIDDEN_STD.iter().any(|n| line.contains(n)))
        .map(|(path, line)| format!("{}: {}", path.display(), line.trim()))
        .collect();
    assert!(
        found.is_empty(),
        "newt-interaction takes ambient authority: {found:#?}\nThe protocol \
         layer describes records; reading a file, opening a socket, spawning \
         a process, or consulting the environment all belong outward."
    );
}

/// **Anti-vacuous twin.** The same scanner, pointed at this crate's own
/// tests directory — which legitimately uses `std::process` and `std::fs`
/// to run `cargo metadata` — must find them. A scanner that reports clean
/// on code it cannot read reports clean on everything.
#[test]
fn the_source_scanner_would_notice_ambient_authority() {
    let mut probe = std::env::temp_dir();
    probe.push(format!(
        "newt-interaction-guard-probe-{}",
        std::process::id()
    ));
    let src = probe.join("src");
    std::fs::create_dir_all(&src).expect("probe dir");
    std::fs::write(
        src.join("lib.rs"),
        "pub fn read() -> String { std::fs::read_to_string(\"/etc/hostname\").unwrap() }\n",
    )
    .expect("probe file");

    let lines = production_lines(&probe);
    let found = lines
        .iter()
        .filter(|(_, line)| FORBIDDEN_STD.iter().any(|n| line.contains(n)))
        .count();
    std::fs::remove_dir_all(&probe).ok();
    assert_eq!(
        found, 1,
        "the source scanner missed a std::fs call it was pointed straight at"
    );
}
