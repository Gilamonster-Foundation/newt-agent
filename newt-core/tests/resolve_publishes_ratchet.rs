//! **Countdown ratchet on publishing `Config::resolve*` call sites (#2488).**
//!
//! `Config::resolve` and `Config::resolve_runtime` load config AND republish
//! process-global runtime settings. A caller that only wants a value must use
//! `Config::resolve_unpublished` (#2478). This pins the number of production
//! call sites that still publish: it may only go DOWN. A count that rises
//! fails; a count that drops fails too, so the constant is lowered with the
//! change that earned it.
//!
//! Counting runs over whitespace-squeezed production code (the shared scanner
//! skips comments, string literals and `#[cfg(test)]` regions), so a rustfmt
//! line split cannot hide a call.

use std::collections::BTreeMap;
use std::path::Path;

mod common;
use common::{for_each_production_line, production_roots, workspace_root};

/// The publishing forms. `Config::resolve_unpublished(` does not contain
/// either needle.
const PUBLISHING: [&str; 2] = ["Config::resolve(", "Config::resolve_runtime("];

/// Production call sites that still publish. Ratchet: only ever lowered.
const KNOWN_PUBLISHING_RESOLVES: usize = 30;

fn squeeze(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn count(code: &str) -> usize {
    PUBLISHING.iter().map(|n| code.matches(n).count()).sum()
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
        code.entry(relative(&root, path))
            .or_default()
            .push_str(&squeeze(c));
    });
    assert!(
        code.len() > 100,
        "the scanner must read the workspace, read {} files",
        code.len()
    );
    code.into_iter()
        .map(|(p, c)| (p, count(&c)))
        .filter(|(_, n)| *n > 0)
        .collect()
}

#[test]
fn publishing_resolve_call_sites_only_go_down() {
    let per_file = per_file_counts();
    let total: usize = per_file.values().sum();
    let table = per_file
        .iter()
        .map(|(p, n)| format!("  {n:>3}  {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        total <= KNOWN_PUBLISHING_RESOLVES,
        "{total} publishing Config::resolve/resolve_runtime call sites, \
         KNOWN_PUBLISHING_RESOLVES says {KNOWN_PUBLISHING_RESOLVES}. Use \
         Config::resolve_unpublished for a value read (see #2488).\n{table}"
    );
    assert!(
        total >= KNOWN_PUBLISHING_RESOLVES,
        "{total} publishing call sites, but KNOWN_PUBLISHING_RESOLVES says \
         {KNOWN_PUBLISHING_RESOLVES}. Lower the constant — the ratchet only \
         goes down.\n{table}"
    );
}

/// The needles must survive the squeeze and must not match the unpublished
/// form.
#[test]
fn needles_match_split_calls_and_not_the_unpublished_form() {
    assert_eq!(
        count(&squeeze("newt_core::Config\n    ::resolve(&mut r)")),
        1
    );
    assert_eq!(count(&squeeze("Config::resolve_runtime(&mut r)")), 1);
    assert_eq!(count(&squeeze("Config::resolve_unpublished(&mut r)")), 0);
}
