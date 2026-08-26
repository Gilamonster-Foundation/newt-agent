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
//! - Only crate `src/` trees are visited; `tests/`, `benches/`, build output,
//!   `docs/`, and every hidden directory (`.git/`, `.claude/`, and this
//!   repo's gitignored `.worktrees/` scratch checkouts) are skipped.
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

/// Visit every production line of every crate `src/` Rust file under
/// `workspace_root`, as `visit(path, code, raw)` where `code` is the line
/// with string-literal contents blanked and `raw` is the original line.
/// Doc/line comments and `#[cfg(test)]`-gated regions are not visited.
/// `skip_file` lets a consumer add its own file-level exclusions.
pub fn for_each_production_line(
    workspace_root: &Path,
    skip_file: &dyn Fn(&Path) -> bool,
    visit: &mut dyn FnMut(&Path, &str, &str),
) {
    let mut stack = vec![workspace_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                // Skip build output, docs, and every HIDDEN directory — a hit
                // inside `target/`, `.git/`, `.claude/worktrees/`, or this
                // repo's own `.worktrees/` scratch convention
                // (`.gitignore:107`) is not this build's code. Hidden-by-name
                // rather than an enumerated list: the main checkout really
                // does carry full `src` trees under `.worktrees/`, and an
                // enumeration is always one convention behind.
                if name.starts_with('.') || matches!(name.as_ref(), "target" | "docs") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // newt-tui's out-of-line lib_tests files are each reached by a
            // parent-side #[cfg(test)] #[path = ...] declaration. The child
            // itself has no local cfg for this line scanner to see.
            if is_test_only_source_path(&path) {
                continue;
            }
            if skip_file(&path) {
                continue;
            }
            // Only production sources: a hit in tests/ or benches/ is not a
            // production path.
            if !path.components().any(|c| c.as_os_str() == "src") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
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
                        } else if ctrim.ends_with(';') {
                            // A brace-less gated item (`#[cfg(test)] use …;`,
                            // `mod x;`) ends at its semicolon. Without this
                            // the pending flag latches forever and everything
                            // after it in the file goes invisible — the same
                            // blindness the brace tracker was built to fix.
                            pending_test_attr = false;
                            continue;
                        }
                    }
                    test_depth = (test_depth + opens - closes).max(0);
                    continue;
                }
                visit(&path, &code, line);
            }
        }
    }
}

/// Whether `path` is an out-of-line test module below a crate's `src/`.
/// These modules are parent-gated, so scanning the child file alone cannot
/// recover its cfg context.
pub fn is_test_only_source_path(path: &Path) -> bool {
    let mut below_src = false;
    for component in path.components() {
        if component.as_os_str() == "src" {
            below_src = true;
        } else if below_src && component.as_os_str() == "lib_tests" {
            return true;
        }
    }
    false
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
/// brace counting and word matching see only code. Handles `\"` escapes;
/// deliberately does not handle raw strings or multi-line literals — see the
/// module doc for why that is acceptable for a ratchet.
pub fn strip_string_literals(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_str = false;
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            escaped = false;
            if in_str {
                out.push('_');
            } else {
                out.push(c);
            }
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                if !in_str {
                    out.push(c);
                }
            }
            '"' => {
                in_str = !in_str;
                out.push('"');
            }
            _ if in_str => out.push('_'),
            _ => out.push(c),
        }
    }
    out
}
