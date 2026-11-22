//! A test that reads a sibling crate cannot run from the published package, so it must
//! not ship (#3164). `tests/common` is the workspace's shared source scanner: a test
//! that declares `mod common;` reads sibling crates, `tests/common` is excluded from
//! the package, and so the test must be excluded with it. Without this, such a test
//! ships and cannot compile in an unpacked package.
//!
//! This guard checks that rule only, on the top-level `tests/*.rs` files (no test
//! directory declares `mod common;` today). It is excluded from the package itself: its
//! twin below looks for tests the package does not ship, so it can only pass in the
//! workspace.

use std::collections::BTreeSet;
use std::path::Path;

/// The package's `exclude` list, as written in `Cargo.toml`.
fn excluded(manifest: &str) -> BTreeSet<String> {
    let manifest: toml::Value = toml::from_str(manifest).expect("Cargo.toml parses");
    manifest["package"]["exclude"]
        .as_array()
        .expect("the package has an `exclude` list")
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .expect("exclude entries are strings")
                .to_string()
        })
        .collect()
}

/// Does `source` declare the shared scanner as a module? Indentation, a visibility and a
/// trailing comment do not hide it.
fn declares_mod_common(source: &str) -> bool {
    source.lines().any(|line| {
        let line = line.trim();
        let line = line
            .strip_prefix("pub(crate) ")
            .or_else(|| line.strip_prefix("pub "))
            .unwrap_or(line);
        line.starts_with("mod common;")
    })
}

/// The `tests/*.rs` files that declare `mod common;`, as `tests/<name>.rs`.
fn declaring_mod_common(tests: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(tests).expect("tests/ is readable") {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_some_and(|ext| ext == "rs")
            && declares_mod_common(
                &std::fs::read_to_string(&path).expect("a test file is readable"),
            )
        {
            found.push(format!(
                "tests/{}",
                path.file_name().unwrap().to_string_lossy()
            ));
        }
    }
    found.sort();
    found
}

#[test]
fn a_test_that_declares_mod_common_is_excluded_from_the_package() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let excluded = excluded(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap());
    assert!(
        excluded.contains("tests/common"),
        "`tests/common` reads sibling crates and must stay excluded: {excluded:?}"
    );
    assert!(
        excluded.contains("tests/package_excludes.rs"),
        "this guard's twin reads tests the package excludes, so the guard must not ship: \
         {excluded:?}"
    );
    let unexcluded: Vec<_> = declaring_mod_common(&root.join("tests"))
        .into_iter()
        .filter(|test| !excluded.contains(test))
        .collect();
    assert!(
        unexcluded.is_empty(),
        "these tests declare `mod common;`, which is excluded from the package, so they \
         cannot compile there; add them to `exclude` in newt-core/Cargo.toml: {unexcluded:?}"
    );
}

/// Anti-vacuous twin: the scan finds the tests that declare `mod common;`, so a pass is
/// not a scan that sees nothing. It reads files the package excludes, so it runs in the
/// workspace only.
#[test]
fn the_scan_sees_the_tests_that_declare_mod_common() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let found = declaring_mod_common(&root.join("tests"));
    assert!(
        found.iter().any(|test| test == "tests/first_principle.rs"),
        "the scan must see `tests/first_principle.rs`; found {found:?}"
    );
}

#[test]
fn the_predicate_reads_every_form_of_the_declaration() {
    for declared in [
        "mod common;",
        "pub mod common;",
        "pub(crate) mod common;",
        "    mod common;",
        "mod common; // the shared scanner",
        "#![allow(dead_code)]\nmod common;\nfn main() {}",
    ] {
        assert!(declares_mod_common(declared), "{declared:?}");
    }
    for other in [
        "",
        "mod common_extra;",
        "mod commonly;",
        "// mod common;",
        "mod common {}",
        "use common::workspace_root;",
    ] {
        assert!(!declares_mod_common(other), "{other:?}");
    }
}
