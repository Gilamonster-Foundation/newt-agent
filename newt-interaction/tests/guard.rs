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
const FORBIDDEN_STD: &[&str] = &["fs", "net", "process", "env"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("newt-interaction has a parent workspace directory")
        .to_path_buf()
}

/// The transitive SHIPPED-dependency closure of `package`, by name: normal
/// and BUILD dependencies, excluding only `dev`.
///
/// Build dependencies count. A build script runs with the full authority of
/// the building user — reading the filesystem, spawning processes, reaching
/// the network — before a line of the crate is compiled, so a forbidden
/// crate arriving through `[build-dependencies]` is not a technicality; it
/// is the guard's whole subject coming in another door. Only `dev` is
/// excluded, because dev-dependencies impose nothing on a consumer, which
/// is why this crate's own `serde_json`/`toml` are not violations.
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
            // `kind: null` is a normal dependency; "build" ships its
            // authority into the build; only "dev" is excluded.
            if kinds
                .iter()
                .any(|k| k["kind"].is_null() || k["kind"] == "build")
            {
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
/// A0's hardened production-source scanner, reused rather than re-written.
///
/// A `#[path]` include is a SOURCE include: it creates no cargo dependency
/// edge, so the closure guard above stays clean while this crate gets the
/// brace-depth `#[cfg(test)]` tracking, the trailing-comment truncation,
/// and the char-literal-safe string blanking that #1823's review rounds
/// paid for. Re-deriving that logic here would be a second implementation
/// of exactly what the reuse discipline forbids duplicating — and the first
/// cut of this file did re-derive it, complete with the latch bug the
/// shared scanner had already fixed.
#[path = "../../newt-core/tests/common/mod.rs"]
#[allow(dead_code)]
mod common;

/// Every production line of the crate rooted at `dir` that takes ambient
/// authority, as `path: line [capability]`.
///
/// Matches the module path (`std::fs::read_to_string`), the plain import
/// (`use std::fs;`), the aliased import (`use std::fs as x;`), and brace
/// groups (`use std::{env, fs};`, `use std::{process::Command, …}`).
///
/// **Accepted residual, stated rather than implied:** this is a tripwire,
/// not a proof. A determined author can still reach the filesystem through
/// a re-export, a macro, or a dependency that does it for them. What it
/// buys is that the ordinary ways in are loud, and that the epic's
/// "filesystem/application dependency" clause — which names capabilities,
/// not crates, so no closure walk can ever see it — has an armed check.
fn ambient_authority(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    common::for_each_production_line(&[dir.join("src")], &|_| false, &mut |path, code, raw| {
        if let Some(hit) = ambient_hit(code) {
            found.push(format!("{}: {} [{hit}]", path.display(), raw.trim()));
        }
    });
    found
}

/// The forbidden capability a line reaches, if any.
fn ambient_hit(code: &str) -> Option<&'static str> {
    for module in FORBIDDEN_STD {
        if code.contains(&format!("std::{module}")) {
            return Some(module);
        }
    }
    // `use std::…` in any spelling: the tail may be one module, an alias, or
    // a brace group with nested paths.
    let tail = code.split("use std::").nth(1)?;
    for module in FORBIDDEN_STD {
        let mut rest = tail;
        while let Some(at) = rest.find(module) {
            let before_ok = at == 0
                || !rest[..at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after_ok = rest[at + module.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            if before_ok && after_ok {
                return Some(module);
            }
            rest = &rest[at + module.len()..];
        }
    }
    None
}

/// Write a throwaway crate under the temp dir and return its root.
fn probe_crate(name: &str, source: &str) -> PathBuf {
    let mut probe = std::env::temp_dir();
    probe.push(format!(
        "newt-interaction-guard-{name}-{}",
        std::process::id()
    ));
    let src = probe.join("src");
    std::fs::create_dir_all(&src).expect("probe dir");
    std::fs::write(src.join("lib.rs"), source).expect("probe file");
    probe
}
#[test]
fn the_protocol_crate_touches_no_ambient_authority() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut scanned = 0usize;
    common::for_each_production_line(&[root.join("src")], &|_| false, &mut |_, _, _| {
        scanned += 1;
    });
    assert!(
        scanned > 50,
        "the source scan saw only {scanned} lines — it is not reading this crate"
    );
    let found = ambient_authority(root);
    assert!(
        found.is_empty(),
        "newt-interaction takes ambient authority: {found:#?}\nThe protocol \
         layer describes records; reading a file, opening a socket, spawning \
         a process, or consulting the environment all belong outward."
    );
}

/// **Anti-vacuous twin (b): a mid-file `#[cfg(test)]` must not blind the
/// rest of the file.** A latch that flips on the first test attribute and
/// never clears skips every later line — the exact blindness A0's shared
/// scanner fixed by tracking brace depth
/// (`newt-core/tests/common/mod.rs`). A scanner with that latch reports a
/// crate clean the moment it contains one test module.
#[test]
fn the_source_scanner_sees_past_a_test_module() {
    let probe = probe_crate(
        "mid-file-cfg",
        "#[cfg(test)]\nmod tests {\n    fn t() { let _ = 1; }\n}\n\n\
         pub fn real() -> String { std::fs::read_to_string(\"/etc/hostname\").unwrap() }\n",
    );
    let found = ambient_authority(&probe);
    std::fs::remove_dir_all(&probe).ok();
    assert_eq!(
        found.len(),
        1,
        "production code after a test module went unscanned: {found:?}"
    );
}

/// **Anti-vacuous twin (c): brace groups and aliases are the same import.**
/// `use std::{env, fs};` and `use std::fs as filesystem;` reach exactly
/// what `use std::fs;` reaches, and a needle that only knows the one
/// spelling is a tripwire with a documented way around it.
#[test]
fn the_source_scanner_sees_grouped_and_aliased_imports() {
    for (name, source) in [
        (
            "group",
            "use std::{env, fs};\npub fn f() -> bool { true }\n",
        ),
        (
            "alias",
            "use std::fs as filesystem;\npub fn f() -> bool { true }\n",
        ),
        (
            "nested-group",
            "use std::{collections::BTreeMap, process::Command};\npub fn f() -> bool { true }\n",
        ),
    ] {
        let probe = probe_crate(name, source);
        let found = ambient_authority(&probe);
        std::fs::remove_dir_all(&probe).ok();
        assert!(
            !found.is_empty(),
            "the `{name}` spelling of an ambient-authority import was missed"
        );
    }
}
/// **Anti-vacuous twin.** The same scanner, pointed at a seeded `std::fs`
/// call, must find it. A scanner that reports clean on code it cannot read
/// reports clean on everything.
#[test]
fn the_source_scanner_would_notice_ambient_authority() {
    let probe = probe_crate(
        "plain",
        "pub fn read() -> String { std::fs::read_to_string(\"/etc/hostname\").unwrap() }\n",
    );
    let found = ambient_authority(&probe);
    std::fs::remove_dir_all(&probe).ok();
    assert_eq!(
        found.len(),
        1,
        "the source scanner missed a std::fs call it was pointed straight at"
    );
}
