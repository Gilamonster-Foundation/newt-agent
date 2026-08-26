//! Shared production-source scanner for structural ratchet tests.
//!
//! A source scan is an unusual shape for a test and is used deliberately: the
//! property under test is a fact about the program text (reachability,
//! duplicate-implementation counts), not about any value the program
//! computes. Extracted from `first_principle.rs` so the markup sprawl ratchet
//! (#1823) does not stand up a second scanner beside it — the brace-depth
//! `#[cfg(test)]` skipper below is precisely the kind of subtle logic the
//! reuse discipline forbids duplicating.
//!
//! Scope and honesty notes shared by every consumer:
//! - The walk is scoped to PRODUCTION workspace members: `<member>/src` for
//!   every `[workspace] members` entry, minus the `tests/*` support crates,
//!   plus `newt-web/src` (workspace-`exclude`d but a real production surface
//!   this repo's ratchets baseline). Nothing else is visited — not
//!   workspace-`exclude`d crates such as `newt-mesh`, not a nested worktree,
//!   not a stray checkout. Scoping by MANIFEST rather than by "everything
//!   under the repo root, minus a skip list" is what keeps a developer's
//!   machine agreeing with CI.
//! - Within a root, build output and hidden directories are still skipped —
//!   `target/` for the former, and hidden-by-name for `.git/`, `.claude/`,
//!   and this repo's gitignored `.worktrees/` scratch checkouts
//!   (`.gitignore:107`), which really do carry full `src` trees.
//! - Lines inside `#[cfg(test)]`-gated items are skipped by brace depth — a
//!   simple "saw the attribute, skip the rest of the file" latch would blind
//!   the scanner to production code after an early inline test seam.
//! - String-literal contents are blanked before matching, so a pattern inside
//!   an error message cannot satisfy (or trip) a law.
//! - Multi-line string literals and raw strings would still confuse a line
//!   scanner; none of the scanned guarantees sit near one, and this is a
//!   structural ratchet, not a parser.

use std::path::{Path, PathBuf};

/// The workspace root, derived from the running test crate's manifest dir.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("newt-core has a parent workspace directory")
        .to_path_buf()
}

/// The production source roots of the workspace rooted at `workspace_root`:
/// `<member>/src` for each `[workspace] members` entry, minus the `tests/*`
/// support crates (whose `src/` is test scaffolding, not product code), plus
/// `newt-web/src` — workspace-`exclude`d because it carries its own
/// lockfile, but a production surface this repo's ratchets baseline.
///
/// A member that does not exist on disk is silently dropped; the consumer's
/// own self-check (every baselined path must fall under some root) is what
/// turns a scoping mistake into a loud failure rather than a silent gap.
pub fn production_roots(workspace_root: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = workspace_members(workspace_root)
        .into_iter()
        // `tests/common` and `tests/pty` are workspace MEMBERS, so a filter
        // like "is any path component `src`?" happily calls their scaffolding
        // production code. Membership is not production-ness.
        .filter(|m| !m.starts_with("tests/"))
        .map(|m| workspace_root.join(m).join("src"))
        .filter(|p| p.is_dir())
        .collect();
    let web = workspace_root.join("newt-web").join("src");
    if web.is_dir() {
        roots.push(web);
    }
    roots.sort();
    roots
}

/// `[workspace] members` entries from `workspace_root/Cargo.toml`, in
/// declaration order. A trailing `*` segment (`crates/*`) is expanded
/// against the filesystem so a globbed member cannot become an invisible
/// scanning gap.
fn workspace_members(workspace_root: &Path) -> Vec<String> {
    let Ok(manifest) = std::fs::read_to_string(workspace_root.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Some(start) = manifest.find("members = [") else {
        return Vec::new();
    };
    let rest = &manifest[start..];
    let Some(end) = rest.find(']') else {
        return Vec::new();
    };
    let mut members = Vec::new();
    for line in rest[..end].lines() {
        let line = line.trim().trim_end_matches(',').trim();
        let Some(member) = line.strip_prefix('"').and_then(|m| m.strip_suffix('"')) else {
            continue;
        };
        match member.strip_suffix("/*") {
            Some(parent) => {
                if let Ok(entries) = std::fs::read_dir(workspace_root.join(parent)) {
                    for entry in entries.flatten() {
                        if entry.path().is_dir() {
                            members
                                .push(format!("{parent}/{}", entry.file_name().to_string_lossy()));
                        }
                    }
                }
            }
            None => members.push(member.to_string()),
        }
    }
    members
}

/// Visit every production line of every Rust file under `roots` (see
/// [`production_roots`]), as `visit(path, code, raw)` where `code` is the
/// line with string-literal contents blanked and its trailing line comment
/// removed, and `raw` is the original line. Doc/line comments and
/// `#[cfg(test)]`-gated regions are not visited. `skip_file` lets a consumer
/// add its own file-level exclusions.
/// Every `.rs` file under `roots`, minus build output, hidden directories,
/// and whatever `skip_file` rejects.
fn rust_files(roots: &[PathBuf], skip_file: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                // Skip build output and every HIDDEN directory — a hit inside
                // `target/`, `.git/`, `.claude/worktrees/`, or this repo's own
                // `.worktrees/` scratch convention (`.gitignore:107`) is not
                // this build's code. Hidden-by-name rather than an enumerated
                // list: the main checkout really does carry full `src` trees
                // under `.worktrees/`, and an enumeration is always one
                // convention behind. (No `docs` skip: the roots are now
                // `<member>/src`, where a `docs` module would be real code.)
                if name.starts_with('.') || name == "target" {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            if skip_file(&path) {
                continue;
            }
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Files that some OTHER file declares as a `#[cfg(test)]`-gated out-of-line
/// module (`#[cfg(test)] mod x;`, with or without `#[path = "..."]`).
///
/// Such a child carries no cfg of its own, so a line scanner reading it
/// alone would call test scaffolding production code. Detecting them
/// STRUCTURALLY — from the declaration — rather than by filename convention
/// is what keeps this complete: this repo declares them as `lib_tests/*`,
/// `mod_tests/*`, `tools_tests/*`, `*_test.rs`, and a plain `tests.rs`, and
/// a name allowlist covers only the ones someone already noticed.
fn test_gated_children(files: &[PathBuf]) -> std::collections::BTreeSet<PathBuf> {
    let mut children = std::collections::BTreeSet::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let Some(dir) = file.parent() else { continue };
        let mut gated = false;
        let mut path_attr: Option<String> = None;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("#[cfg(") {
                gated |= cfg_is_test_only(trimmed);
                // An inline `#[cfg(test)] mod x;` declares on this same line.
                if let Some(rest) = trimmed.split_once(']').map(|(_, r)| r.trim()) {
                    if gated {
                        if let Some(child) = declared_child(dir, rest, path_attr.as_deref()) {
                            children.insert(child);
                            gated = false;
                            path_attr = None;
                        }
                    }
                }
                continue;
            }
            if trimmed.starts_with("#[path") {
                path_attr = trimmed
                    .split('"')
                    .nth(1)
                    .map(std::string::ToString::to_string);
                continue;
            }
            if trimmed.starts_with("#[") {
                continue;
            }
            if gated {
                if let Some(child) = declared_child(dir, trimmed, path_attr.as_deref()) {
                    children.insert(child);
                }
            }
            // Attributes bind to the next item only.
            gated = false;
            path_attr = None;
        }
    }
    children
}

/// Resolve `mod NAME;` (optionally redirected by `#[path = "..."]`) against
/// the declaring file's directory, which is where every out-of-line child in
/// this repo lives.
fn declared_child(dir: &Path, item: &str, path_attr: Option<&str>) -> Option<PathBuf> {
    let rest = item.strip_prefix("mod ")?;
    let name = rest.strip_suffix(';')?.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    if let Some(rel) = path_attr {
        return Some(dir.join(rel));
    }
    let flat = dir.join(format!("{name}.rs"));
    if flat.is_file() {
        return Some(flat);
    }
    let nested = dir.join(name).join("mod.rs");
    nested.is_file().then_some(nested)
}

/// Visit every production line of every Rust file under `roots` (see
/// [`production_roots`]), as `visit(path, code, raw)` where `code` is the
/// line with string-literal contents blanked and its trailing line comment
/// removed, and `raw` is the original line. Doc/line comments,
/// `#[cfg(test)]`-gated regions, and parent-gated out-of-line test children
/// are not visited. `skip_file` lets a consumer add its own exclusions.
pub fn for_each_production_line(
    roots: &[PathBuf],
    skip_file: &dyn Fn(&Path) -> bool,
    visit: &mut dyn FnMut(&Path, &str, &str),
) {
    let files = rust_files(roots, skip_file);
    let children = test_gated_children(&files);
    for path in &files {
        if children.contains(path) {
            continue;
        }
        let path = path.as_path();
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        // Skip cfg items that cannot compile in production by brace depth:
        // from the attribute, ignore lines until the braces opened after
        // it close again.
        let mut test_depth: i32 = 0;
        let mut pending_test_attr = false;
        for line in text.lines() {
            let trimmed = line.trim_start();
            // Doc comments describe the guarantee; they do not invoke it.
            if trimmed.starts_with("//") {
                continue;
            }
            // Brace counting and cfg matching both work on the line with
            // string literals blanked, so `"{"` in a message or `"test"`
            // in a feature name (`feature = "test-util"`) cannot skew
            // either.
            let mut code = strip_string_literals(line);
            // Truncate the trailing line comment AFTER blanking strings
            // (so a `//` inside a literal cannot trigger this). Without
            // it, a needle inside a trailing comment counts as a hit —
            // and under an exact-count baseline, a comment could silently
            // SUBSTITUTE for a deleted real site.
            if let Some(slashes) = code.find("//") {
                code.truncate(slashes);
            }
            let ctrim = code.trim_start();
            if test_depth == 0 && !pending_test_attr && cfg_is_test_only(ctrim) {
                // Do not continue yet: an inline attribute item such as
                // #[cfg(test)] fn f() { must contribute its opening brace
                // before the test-only body is skipped.
                pending_test_attr = true;
            }
            if test_depth > 0 || pending_test_attr {
                let opens = code.matches('{').count() as i32;
                let closes = code.matches('}').count() as i32;
                if pending_test_attr {
                    if opens > 0 {
                        pending_test_attr = false;
                    } else if ctrim.trim_end().ends_with(';') {
                        // A brace-less gated item (`#[cfg(test)] use …;`,
                        // `mod x;`) ends at its semicolon. Without this
                        // the pending flag latches forever and everything
                        // after it in the file goes invisible — the same
                        // blindness the brace tracker was built to fix.
                        // `trim_end` because truncating a trailing line
                        // comment leaves the space that preceded the
                        // `//`: `mod tests; // out of line` must still
                        // read as terminated.
                        pending_test_attr = false;
                        continue;
                    }
                }
                test_depth = (test_depth + opens - closes).max(0);
                continue;
            }
            visit(path, &code, line);
        }
    }
}

/// Return true only for cfg predicates that require the `test` atom. This is
/// deliberately conservative: `#[cfg(not(test))]` and
/// `#[cfg(any(test, unix))]` can compile in production and must stay visible.
pub fn cfg_is_test_only(code: &str) -> bool {
    let Some(rest) = code.strip_prefix("#[cfg(") else {
        return false;
    };
    let mut depth = 1_i32;
    for (index, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return cfg_all_requires_test(&rest[..index]);
        }
    }
    false
}

/// An `all(...)` predicate requires tests when any of its factors does.
fn cfg_all_requires_test(predicate: &str) -> bool {
    let predicate = predicate.trim();
    if predicate == "test" {
        return true;
    }
    let Some(open) = predicate.find('(') else {
        return false;
    };
    if predicate[..open].trim() != "all" || !predicate.ends_with(')') {
        return false;
    }
    split_cfg_args(&predicate[open + 1..predicate.len() - 1])
        .into_iter()
        .any(cfg_all_requires_test)
}

/// Split cfg-function arguments while preserving nested cfg expressions.
fn split_cfg_args(args: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0_i32;
    for (index, ch) in args.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                let part = args[start..index].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    let part = args[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

/// Blank out string literal contents on one line (keeps the quotes), so
/// brace counting and word matching see only code. Handles `\"` escapes and
/// char literals — the `"` in `'"'` is a CHARACTER, not a string opener,
/// and treating it as one blanks the rest of the line (braces included),
/// which unbalances the `#[cfg(test)]` depth tracker and spills test-only
/// lines into the production scan. Rust lifetimes (`'a`) have no closing
/// quote and fall through untouched. Deliberately does not handle raw
/// strings or multi-line literals — see the module doc for why that is
/// acceptable for a ratchet.
pub fn strip_string_literals(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut in_str = false;
    while i < chars.len() {
        let c = chars[i];
        if !in_str && c == '\'' {
            // `'X'` — a plain char literal.
            if i + 2 < chars.len() && chars[i + 1] != '\\' && chars[i + 2] == '\'' {
                out.push_str("'_'");
                i += 3;
                continue;
            }
            // `'\X'` — an escaped char literal.
            if i + 3 < chars.len() && chars[i + 1] == '\\' && chars[i + 3] == '\'' {
                out.push_str("'__'");
                i += 4;
                continue;
            }
            // Otherwise a lifetime: keep it.
        }
        if in_str && c == '\\' {
            out.push('_');
            if i + 1 < chars.len() {
                out.push('_');
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = !in_str;
            out.push('"');
            i += 1;
            continue;
        }
        out.push(if in_str { '_' } else { c });
        i += 1;
    }
    out
}
