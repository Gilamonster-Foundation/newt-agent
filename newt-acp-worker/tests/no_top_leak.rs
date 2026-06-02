//! Negative-existence guard: `Caveats::top()` must not appear in
//! non-test code under the headless dispatch path.
//!
//! Issue #94 replaced the headless `Caveats::top()` hard-code in
//! `newt-acp-worker/src/server.rs:451` with a per-dispatch caveat
//! derivation from a signed operator [`WorkerIdentity`]. This test
//! locks that property in: a future regression that re-introduces a
//! `Caveats::top()` literal anywhere in the dispatch chain
//! (`newt-acp-worker/src`, `newt-coder/src`, `newt-inference/src`,
//! `newt-cli/src`) fails the suite before anything else can ship.
//!
//! Test-only call sites (under `#[cfg(test)]` modules, doc comments,
//! string literals, or `tests/` directories) are explicitly allowed —
//! they're fixtures, not dispatch sites.

use std::fs;
use std::path::{Path, PathBuf};

/// Walk `dir`, return every `.rs` file under it.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

/// Build a byte-range list of `#[cfg(test)] mod ... { ... }` blocks in
/// `body`. Any `Caveats::top()` match whose byte offset lands inside
/// one of these ranges is treated as test code and ignored.
fn cfg_test_ranges(body: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while let Some(rel) = body[i..].find("#[cfg(test)]") {
        let start = i + rel;
        // Find the next `{` after the attribute. Anything between the
        // attribute and that brace is the `mod foo` declaration (or
        // similar — `fn`, `impl`, …; we accept them all).
        let mut j = start + "#[cfg(test)]".len();
        let open = loop {
            if j >= bytes.len() {
                break None;
            }
            if bytes[j] == b'{' {
                break Some(j);
            }
            j += 1;
        };
        let Some(open) = open else { break };

        // Match the brace to find the close. We do NOT try to handle
        // string-literal/char-literal braces; the repo's code style
        // doesn't put `{`/`}` inside strings inside test modules.
        let mut depth: i32 = 1;
        let mut k = open + 1;
        let mut close = None;
        while k < bytes.len() {
            match bytes[k] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(k);
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        let Some(close) = close else { break };
        ranges.push((start, close + 1));
        i = close + 1;
    }
    ranges
}

/// True if `line` (the literal source text of one line) is structurally
/// a comment / doc comment / string literal context for the
/// `Caveats::top()` token. Heuristic but conservative for this repo.
fn looks_like_non_call(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") {
        return true;
    }
    // String-literal context: the token sits inside a `"..."` on this
    // line. A real call site would have `Caveats::top()` as a Rust
    // expression, not embedded in a string. The cheap test: the token
    // appears AFTER the first `"` and BEFORE a matching `"` on the
    // same line.
    if let Some(tok_at) = line.find("Caveats::top()") {
        let head = &line[..tok_at];
        let quote_count = head.bytes().filter(|b| *b == b'"').count();
        if quote_count % 2 == 1 {
            return true;
        }
    }
    false
}

/// Scan one source file for non-comment, non-test `Caveats::top()` matches.
fn offending_lines(path: &Path) -> Vec<(usize, String)> {
    let body = fs::read_to_string(path).unwrap();
    let test_ranges = cfg_test_ranges(&body);
    let mut hits = Vec::new();

    let mut offset: usize = 0;
    for (n, line) in body.lines().enumerate() {
        let line_start = offset;
        offset += line.len() + 1; // +1 for the '\n' lines() consumed

        if !line.contains("Caveats::top()") {
            continue;
        }
        if looks_like_non_call(line) {
            continue;
        }
        // Inside any `#[cfg(test)]` block?
        let in_test = test_ranges
            .iter()
            .any(|(s, e)| line_start >= *s && line_start < *e);
        if in_test {
            continue;
        }
        hits.push((n + 1, line.to_string()));
    }
    hits
}

#[test]
fn no_caveats_top_in_headless_dispatch_paths() {
    // Resolve the workspace root from CARGO_MANIFEST_DIR
    // (`<root>/newt-acp-worker`). Walking absolute paths makes the
    // assertion output point at real files when it fails.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().to_path_buf();

    let scan_dirs = [
        workspace_root.join("newt-acp-worker").join("src"),
        workspace_root.join("newt-coder").join("src"),
        workspace_root.join("newt-inference").join("src"),
        workspace_root.join("newt-cli").join("src"),
    ];

    let mut findings: Vec<String> = Vec::new();

    for dir in &scan_dirs {
        for file in rust_sources(dir) {
            for (n, line) in offending_lines(&file) {
                findings.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(&workspace_root)
                        .unwrap_or(&file)
                        .display(),
                    n,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        findings.is_empty(),
        "Caveats::top() must not appear in non-test headless dispatch code (#94 regression).\n\n\
         Offending sites:\n{}\n\n\
         Fix: derive caveats from a `WorkerIdentity` (or its TUI sibling \
         `SessionCapability`); test fixtures stay inside `#[cfg(test)] mod ...` or `tests/`.",
        findings.join("\n")
    );
}

// ── meta-test the scanner itself, so a future refactor of the
//    heuristics doesn't silently weaken the regression check ─────────

#[test]
fn scanner_detects_a_real_top_call() {
    let body = "use newt_core::Caveats;\nfn main() { let c = Caveats::top(); }\n";
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("rogue.rs");
    fs::write(&path, body).unwrap();
    let hits = offending_lines(&path);
    assert_eq!(hits.len(), 1, "scanner missed a real call: {hits:?}");
}

#[test]
fn scanner_skips_doc_comments() {
    let body = "/// example: Caveats::top()\nfn main() {}\n";
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("doc.rs");
    fs::write(&path, body).unwrap();
    assert!(
        offending_lines(&path).is_empty(),
        "doc-comment top() must be ignored"
    );
}

#[test]
fn scanner_skips_string_literals() {
    let body = "fn main() { println!(\"saw Caveats::top() once\"); }\n";
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("strlit.rs");
    fs::write(&path, body).unwrap();
    assert!(
        offending_lines(&path).is_empty(),
        "string-literal top() must be ignored"
    );
}

#[test]
fn scanner_skips_cfg_test_blocks() {
    let body = r#"
fn live() {}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() { let _ = Caveats::top(); }
}
"#;
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("cfgtest.rs");
    fs::write(&path, body).unwrap();
    assert!(
        offending_lines(&path).is_empty(),
        "#[cfg(test)] top() must be ignored"
    );
}
