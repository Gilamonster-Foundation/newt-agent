//! **One owner for skill directories (#2331).**
//!
//! The prompt index resolved skill directories one way and the `use_skill`
//! loader another, so a bundled-only skill was listed to the model and then
//! `unknown skill` when it asked for it. `Config::skill_search_dirs` and
//! `Config::skill_install_dir` are now the only resolvers. This test pins
//! every production consumer to them: each call into a `newt_skills`
//! directory function is counted per file, next to that file's calls into
//! the owner.
//!
//! A new consumer changes a count and fails here. The fix is to take its
//! directories from the owner and add its row, with the consumer named, in
//! the same PR. A count that DROPS fails too, so a consumer cannot quietly
//! stop using the owner and still read green.
//!
//! Counting runs over whitespace-squeezed production code (the shared
//! scanner skips comments and `#[cfg(test)]` regions), so a rustfmt line
//! split cannot hide a call. A name needle cannot see a helper renamed to
//! something new; review catches that, with this table as the checklist.

use std::collections::BTreeMap;
use std::path::Path;

mod common;
use common::{for_each_production_line, production_roots, workspace_root};

/// `newt_skills` functions that take a skill directory list or destination.
const DIR_CALLS: [&str; 4] = [
    "discover_paths(",
    "discover_paths_with_shadows(",
    "load_body_from(",
    "install_skill(",
];

/// The owner's resolvers.
const OWNER_CALLS: [&str; 2] = [".skill_search_dirs()", ".skill_install_dir()"];

/// Retired second sources. Each one produced a directory list the loader did
/// not share; none may come back.
const RETIRED: [&str; 2] = ["with_bundled_default(", "default_skills_dir("];

/// Workspace-relative file -> (directory calls, owner calls, consumer).
const CONSUMERS: &[(&str, usize, usize, &str)] = &[
    (
        "newt-cli/src/skills.rs",
        3,
        2,
        "`newt skills` list, install and share; the install default",
    ),
    (
        "newt-core/src/agentic/tools.rs",
        1,
        1,
        "the `use_skill` loader",
    ),
    (
        "newt-core/src/config/skills.rs",
        0,
        1,
        "the owner (search path falls back to the install dir)",
    ),
    (
        "newt-tui/src/chat.rs",
        0,
        2,
        "persona skill-binding warnings at start and on /persona",
    ),
    (
        "newt-tui/src/lib.rs",
        3,
        3,
        "prompt index, persona binding check, /posture skills, default-skill seeding",
    ),
    ("newt-tui/src/settings_form.rs", 1, 1, "/settings posture"),
];

fn count(code: &str, needles: &[&str]) -> usize {
    needles.iter().map(|n| code.matches(n).count()).sum()
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every production file's squeezed code, keyed by workspace-relative path.
fn squeezed_production_code() -> BTreeMap<String, String> {
    let root = workspace_root();
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for_each_production_line(
        &production_roots(&root),
        &|_| false,
        &mut |path, code, _| {
            files
                .entry(relative(&root, path))
                .or_default()
                .extend(code.chars().filter(|c| !c.is_whitespace()));
        },
    );
    files
}

#[test]
fn every_skill_directory_consumer_takes_its_dirs_from_the_owner() {
    let files = squeezed_production_code();
    assert!(
        files.len() > 100,
        "the scanner must read the workspace, read {} files",
        files.len()
    );

    let mut actual: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (path, code) in &files {
        // newt-skills implements the directory functions; it is the layer
        // below the owner, not a consumer of it.
        if path.starts_with("newt-skills/") {
            continue;
        }
        let dir_calls = count(code, &DIR_CALLS);
        let owner_calls = count(code, &OWNER_CALLS);
        if dir_calls + owner_calls > 0 {
            actual.insert(path.clone(), (dir_calls, owner_calls));
        }
    }
    let expected: BTreeMap<String, (usize, usize)> = CONSUMERS
        .iter()
        .map(|(path, dirs, owner, _)| ((*path).to_string(), (*dirs, *owner)))
        .collect();
    assert_eq!(
        actual, expected,
        "skill directory consumers changed: take the dirs from \
         Config::skill_search_dirs / skill_install_dir and update CONSUMERS \
         with the consumer named (path -> (newt_skills dir calls, owner calls))"
    );

    for (path, dirs, owner, consumer) in CONSUMERS {
        assert!(
            *dirs == 0 || *owner > 0,
            "{path} ({consumer}) calls newt_skills directory functions \
             without resolving through the owner"
        );
    }
}

#[test]
fn retired_skill_directory_sources_stay_deleted() {
    let files = squeezed_production_code();
    let revived: Vec<&String> = files
        .iter()
        .filter(|(_, code)| count(code, &RETIRED) > 0)
        .map(|(path, _)| path)
        .collect();
    assert!(
        revived.is_empty(),
        "a retired skill directory source is back in {revived:?}"
    );
}

/// The needles must survive the squeeze: rustfmt splits a long chain before
/// `.skill_search_dirs()`, and the count has to see it either way. Without
/// this, a squeeze bug would zero every count and fail the table loudly, but
/// for the wrong reason.
#[test]
fn needles_match_split_and_joined_call_shapes() {
    let squeeze = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let split = squeeze(
        "let dirs = newt_core::Config::resolve()\n    .map(|c| c\n        .skill_search_dirs())",
    );
    assert_eq!(count(&split, &OWNER_CALLS), 1);
    let calls =
        squeeze("newt_skills::load_body_from(&dirs, name); discover_paths_with_shadows(search)");
    assert_eq!(count(&calls, &DIR_CALLS), 2);
    assert_eq!(
        count(&squeeze("load_body_from_in(&fs, &dirs, n)"), &DIR_CALLS),
        0
    );
}
