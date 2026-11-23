//! A test that reads outside the crate cannot run from the published package, so it
//! must not ship (#3164). A test reads outside the crate in one of three ways:
//!
//! - it declares `mod common;`: `tests/common` is the workspace's shared source scanner,
//!   it reads sibling crates, and it is excluded from the package;
//! - it embeds a file above the crate, `include_str!("../../..")` or `include_bytes!`, so
//!   it cannot compile in an unpacked package;
//! - it walks up from `CARGO_MANIFEST_DIR`, so it reads whatever sits beside the unpacked
//!   package. The rule reads the whole file: one that names `CARGO_MANIFEST_DIR` and
//!   goes up from any path (`.parent()`, `.ancestors()`, `.pop()`, or a `..` segment in a
//!   string, as in `join("../newt-tui/src/chat.rs")`) is taken to walk up, so the rule
//!   errs toward excluding a test that stays inside.
//!
//! This guard checks that rule on the top-level `tests/*.rs` files, and checks that no
//! file under a test directory reads outside the crate except `tests/common`, which is
//! excluded whole. It is excluded from the package itself: its twin below looks for
//! tests the package does not ship, so it can only pass in the workspace.

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

/// Does `source` embed a file above the crate? From `tests/`, `../../` leaves it.
fn embeds_outside_the_crate(source: &str) -> bool {
    ["include_str!", "include_bytes!"].iter().any(|embed| {
        source.match_indices(embed).any(|(at, _)| {
            source[at + embed.len()..]
                .trim_start()
                .strip_prefix('(')
                .is_some_and(|rest| rest.trim_start().starts_with("\"../../"))
        })
    })
}

/// Does `source` walk up from the crate's own directory? See the module doc: the whole
/// file is read, not one statement, because the `..` can arrive through a helper.
fn walks_up_from_the_crate(source: &str) -> bool {
    source.contains("CARGO_MANIFEST_DIR")
        && [
            ".parent()",
            ".ancestors()",
            ".pop()",
            "\"../",
            "\"/..",
            "\"..\"",
        ]
        .iter()
        .any(|up| source.contains(up))
}

/// Does `source` read outside the crate, in any of the three ways the module doc names?
fn reads_outside_the_crate(source: &str) -> bool {
    declares_mod_common(source)
        || embeds_outside_the_crate(source)
        || walks_up_from_the_crate(source)
}

/// The `.rs` files under `tests`'s subdirectories that read outside the crate, as paths
/// relative to the crate.
fn nested_reading_outside_the_crate(dir: &Path, root: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("a test directory is readable") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            nested_reading_outside_the_crate(&path, root, found);
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && reads_outside_the_crate(&std::fs::read_to_string(&path).unwrap_or_default())
        {
            let relative = path.strip_prefix(root).expect("under the crate");
            found.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// The `tests/*.rs` files that read outside the crate, as `tests/<name>.rs`.
fn reading_outside_the_crate(tests: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(tests).expect("tests/ is readable") {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_some_and(|ext| ext == "rs")
            && reads_outside_the_crate(
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
fn a_test_that_reads_outside_the_crate_is_excluded_from_the_package() {
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
    let unexcluded: Vec<_> = reading_outside_the_crate(&root.join("tests"))
        .into_iter()
        .filter(|test| !excluded.contains(test))
        .collect();
    assert!(
        unexcluded.is_empty(),
        "these tests read outside the crate, so they cannot run from the package; add them \
         to `exclude` in newt-core/Cargo.toml: {unexcluded:?}"
    );
    let mut nested = Vec::new();
    for entry in std::fs::read_dir(root.join("tests")).expect("tests/ is readable") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            nested_reading_outside_the_crate(&path, root, &mut nested);
        }
    }
    nested.sort();
    assert_eq!(
        nested,
        ["tests/common/mod.rs"],
        "only `tests/common`, excluded whole, may read outside the crate from a test \
         directory; this guard checks top-level files only, so extend it to cover the others"
    );
}

/// Anti-vacuous twin: the scan finds a test for each way out of the crate, so a pass is
/// not a scan that sees nothing. It reads files the package excludes, so it runs in the
/// workspace only.
#[test]
fn the_scan_sees_each_way_out_of_the_crate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let found = reading_outside_the_crate(&root.join("tests"));
    for test in [
        "tests/first_principle.rs",
        "tests/regression_register.rs",
        "tests/content_addressable_ratchet.rs",
        "tests/prompt_window_is_sealed.rs",
        "tests/tenacity_exact_family_ratchet.rs",
    ] {
        assert!(
            found.iter().any(|found| found == test),
            "the scan must see `{test}`; found {found:?}"
        );
    }
}

#[test]
fn the_predicates_read_every_way_out_of_the_crate() {
    for embeds in [
        "const R: &str = include_str!(\"../../docs/r.toml\");",
        "include_bytes!( \"../../x.bin\")",
        "include_str!(\n    \"../../newt-tui/src/lib.rs\"\n)",
    ] {
        assert!(embeds_outside_the_crate(embeds), "{embeds:?}");
        assert!(reads_outside_the_crate(embeds), "{embeds:?}");
    }
    for inside in [
        "include_str!(\"../src/lib.rs\")",
        "include_str!(\"fixtures/a.txt\")",
        "// ../../docs",
    ] {
        assert!(!embeds_outside_the_crate(inside), "{inside:?}");
    }
    for walks in [
        "Path::new(env!(\"CARGO_MANIFEST_DIR\"))\n    .parent()\n    .unwrap();",
        "Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"..\");",
        "concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../docs\");",
        "Path::new(env!(\"CARGO_MANIFEST_DIR\")).ancestors().nth(1);",
        "let mut dir = PathBuf::from(env!(\"CARGO_MANIFEST_DIR\"));\ndir.pop();",
        "fn source(rel: &str) -> String { read(Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(rel)) }\n\
         source(\"../newt-tui/src/chat.rs\");",
    ] {
        assert!(walks_up_from_the_crate(walks), "{walks:?}");
        assert!(reads_outside_the_crate(walks), "{walks:?}");
    }
    for stays in [
        "Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"tests/ui/x.rs\");",
        "Path::new(env!(\"CARGO_MANIFEST_DIR\")).join(\"tests\");\npanic!(\"...\");",
        "let dir = file.parent();",
    ] {
        assert!(!walks_up_from_the_crate(stays), "{stays:?}");
    }
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
