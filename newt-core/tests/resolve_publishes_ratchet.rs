//! **Countdown ratchet on publishing `Config::resolve*` call sites (#2488).**
//!
//! `Config::resolve` and `Config::resolve_runtime` load config AND republish
//! process-global runtime settings. A caller that only wants a value must use
//! `Config::resolve_unpublished` (#2478). This pins the number of production
//! call sites that still publish, PER FILE, so a site removed in one file cannot
//! pay for a new one in another. A count that rises fails; a count that drops
//! fails too, so the pin is lowered with the change that earned it.
//!
//! Counting runs over whitespace-squeezed production code (the shared scanner
//! skips comments, string literals and `#[cfg(test)]` regions), so a rustfmt
//! line split cannot hide a call.

use std::collections::BTreeMap;
use std::path::Path;

mod common;
use common::{for_each_production_line, production_roots, workspace_root};

/// The publishing forms. `Config::resolve_unpublished(` does not contain either
/// needle, and a needle preceded by an identifier character is a different
/// type (`SummarizerConfig::resolve(`), not a call to be migrated.
const PUBLISHING: [&str; 2] = ["Config::resolve(", "Config::resolve_runtime("];

/// Production call sites that still publish, per file (non-zero files only).
/// Ratchet: an entry may only be lowered or removed, never raised or added.
const KNOWN_PUBLISHING_RESOLVES: &[(&str, usize)] = &[
    ("newt-acp-worker/src/server.rs", 2),
    ("newt-cli/src/compaction_cmd.rs", 1),
    ("newt-cli/src/config_cmd.rs", 1),
    ("newt-cli/src/crew.rs", 3),
    ("newt-cli/src/dgx.rs", 2),
    ("newt-cli/src/doctor.rs", 1),
    ("newt-cli/src/lib.rs", 3),
    ("newt-cli/src/mcp_cmd.rs", 2),
    ("newt-cli/src/mcp_probe_cmd.rs", 1),
    ("newt-cli/src/skills.rs", 1),
    ("newt-core/src/pyo3_module.rs", 1),
    ("newt-mcp-server/src/lib.rs", 2),
    ("newt-tui/src/auth_command.rs", 1),
    ("newt-tui/src/commands/meta.rs", 1),
    ("newt-tui/src/crew_form/mod.rs", 1),
    ("newt-tui/src/lib.rs", 3),
    ("newt-tui/src/settings_form.rs", 2),
    ("newt-tui/src/wizard.rs", 1),
];

/// Drop whitespace, except a single space where it separates two identifier
/// characters, so `return\nConfig::resolve(` keeps its word boundary while a
/// rustfmt split like `Config\n    ::resolve(` still matches.
fn squeeze(s: &str) -> String {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::new();
    let mut pending = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pending = true;
            continue;
        }
        if pending && out.chars().next_back().is_some_and(is_ident) && is_ident(c) {
            out.push(' ');
        }
        pending = false;
        out.push(c);
    }
    out
}

fn count(code: &str) -> usize {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    PUBLISHING
        .iter()
        .flat_map(|needle| code.match_indices(needle).map(|(at, _)| at))
        .filter(|&at| !code[..at].chars().next_back().is_some_and(is_ident))
        .count()
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Workspace-relative file -> publishing call count, non-zero files only.
fn per_file_counts() -> BTreeMap<String, usize> {
    let root = workspace_root();
    let mut code: BTreeMap<String, String> = BTreeMap::new();
    for_each_production_line(&production_roots(&root), &|_| false, &mut |path, c, _| {
        let text = code.entry(relative(&root, path)).or_default();
        text.push_str(c);
        text.push('\n');
    });
    assert!(
        code.len() > 100,
        "the scanner must read the workspace, read {} files",
        code.len()
    );
    code.into_iter()
        .map(|(p, c)| (p, count(&squeeze(&c))))
        .filter(|(_, n)| *n > 0)
        .collect()
}

#[test]
fn publishing_resolve_call_sites_only_go_down_per_file() {
    let found = per_file_counts();
    let pinned: BTreeMap<String, usize> = KNOWN_PUBLISHING_RESOLVES
        .iter()
        .map(|(p, n)| ((*p).to_string(), *n))
        .collect();
    let mut rose = Vec::new();
    let mut fell = Vec::new();
    for path in found
        .keys()
        .chain(pinned.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let (now, was) = (
            found.get(path).copied().unwrap_or(0),
            pinned.get(path).copied().unwrap_or(0),
        );
        match now.cmp(&was) {
            std::cmp::Ordering::Greater => {
                rose.push(format!("  {path}: {now} found, {was} pinned"));
            }
            std::cmp::Ordering::Less => fell.push(format!("  {path}: {now} found, {was} pinned")),
            std::cmp::Ordering::Equal => {}
        }
    }
    assert!(
        rose.is_empty(),
        "more publishing Config::resolve/resolve_runtime call sites than pinned. Use \
         Config::resolve_unpublished for a value read (see #2488):\n{}",
        rose.join("\n")
    );
    assert!(
        fell.is_empty(),
        "fewer publishing call sites than pinned. Lower the pin — the ratchet only goes \
         down:\n{}",
        fell.join("\n")
    );
}

/// The needles must survive the squeeze, keep their word boundary, and match
/// neither the unpublished form nor a different type that shares the suffix.
#[test]
fn needles_match_split_calls_and_only_the_config_type() {
    let n = |s: &str| count(&squeeze(s));
    assert_eq!(n("newt_core::Config\n    ::resolve(&mut r)"), 1);
    assert_eq!(n("Config::resolve_runtime(&mut r)"), 1);
    assert_eq!(n("Config::resolve_unpublished(&mut r)"), 0);
    assert_eq!(n("newt_core::SummarizerConfig::resolve()"), 0);
    assert_eq!(
        n("return\nConfig::resolve(&mut r)"),
        1,
        "a keyword before it is not an identifier"
    );
    assert_eq!(n("let c = newt_core::Config::resolve(&mut r);"), 1);
}
