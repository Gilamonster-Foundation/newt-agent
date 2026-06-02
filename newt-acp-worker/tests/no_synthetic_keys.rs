//! Negative-existence guard (issue #93): the headless dispatch path
//! must not contain any synthetic-key generators
//! (`UserKey::generate()` or `AgentKey::generate(...)`) in non-test
//! code. Subprocess plugins spawned from the worker have to inherit a
//! delegated child from the operator's `UserKey`, never one minted at
//! spawn time.
//!
//! Test-only call sites (under `#[cfg(test)]` modules, doc comments,
//! string literals, or `tests/` directories) are explicitly allowed —
//! they're fixtures, not dispatch sites. The legitimate
//! `UserKey::generate()` inside `newt-identity::load_or_generate` is
//! scoped *outside* the scanned directories: the headless dispatch
//! chain (`newt-acp-worker/src`, `newt-coder/src`, `newt-inference/src`,
//! `newt-cli/src`) is what we lock down here, mirroring the `#94`
//! `no_top_leak.rs` scanner.
//!
//! Companion to `no_top_leak.rs`: that test asserts the dispatch chain
//! never types `Caveats::top()`; this test asserts the dispatch chain
//! never types `*Key::generate(`. Together they pin the property the
//! PR establishes — every authority used by a subprocess plugin
//! traces back to the operator's `~/.newt/identity.pem`.

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
/// `body`. Any synthetic-key match whose byte offset lands inside one
/// of these ranges is treated as test code and ignored.
fn cfg_test_ranges(body: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while let Some(rel) = body[i..].find("#[cfg(test)]") {
        let start = i + rel;
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
/// a comment / doc comment / string literal context for `token`.
/// Conservative heuristic, mirrored from `no_top_leak.rs`.
fn looks_like_non_call(line: &str, token: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") {
        return true;
    }
    if let Some(tok_at) = line.find(token) {
        let head = &line[..tok_at];
        let quote_count = head.bytes().filter(|b| *b == b'"').count();
        if quote_count % 2 == 1 {
            return true;
        }
    }
    false
}

/// Scan one source file for non-comment, non-test synthetic-key
/// generator call sites.
///
/// The patterns we ban in the dispatch chain:
/// - `UserKey::generate(`  — minting a *fresh* user root anywhere in
///   the dispatch chain is a #93 violation; the operator's user key
///   comes from disk via `newt-identity::load_or_generate`, which
///   lives outside the scanned directories.
/// - `AgentKey::generate(` — there is no such API on the published
///   `agent_mesh_protocol::AgentKey` today, but a future drift that
///   adds one (e.g. a "test-only" generator that escapes
///   `#[cfg(test)]`) would silently let dispatch sites mint a
///   chainless key. Banning the symbol here makes that drift
///   surface as a CI failure rather than silently rooting plugins at
///   a phantom user.
fn offending_lines(path: &Path) -> Vec<(usize, String, &'static str)> {
    let body = fs::read_to_string(path).unwrap();
    let test_ranges = cfg_test_ranges(&body);
    let mut hits = Vec::new();

    let tokens = ["UserKey::generate(", "AgentKey::generate("];

    let mut offset: usize = 0;
    for (n, line) in body.lines().enumerate() {
        let line_start = offset;
        offset += line.len() + 1; // +1 for the '\n' lines() consumed

        for tok in &tokens {
            if !line.contains(tok) {
                continue;
            }
            if looks_like_non_call(line, tok) {
                continue;
            }
            let in_test = test_ranges
                .iter()
                .any(|(s, e)| line_start >= *s && line_start < *e);
            if in_test {
                continue;
            }
            hits.push((n + 1, line.to_string(), *tok));
        }
    }
    hits
}

#[test]
fn no_synthetic_key_generators_in_headless_dispatch_paths() {
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
            for (n, line, tok) in offending_lines(&file) {
                findings.push(format!(
                    "{}:{}: [{}] {}",
                    file.strip_prefix(&workspace_root)
                        .unwrap_or(&file)
                        .display(),
                    n,
                    tok,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        findings.is_empty(),
        "Synthetic key generators (UserKey::generate / AgentKey::generate) must not \
         appear in non-test headless dispatch code (#93 regression).\n\n\
         Offending sites:\n{}\n\n\
         Fix: derive child keys via `parent.delegate(...)` from the operator-rooted \
         `WorkerIdentity::Operator.root` (or its TUI sibling); test fixtures stay \
         inside `#[cfg(test)] mod ...` or `tests/`.",
        findings.join("\n")
    );
}

// ── meta-tests for the scanner itself ─────────────────────────────────

#[test]
fn scanner_detects_a_real_user_key_generate_call() {
    let body = "use agent_mesh_protocol::UserKey;\nfn main() { let u = UserKey::generate(); }\n";
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("rogue_user.rs");
    fs::write(&path, body).unwrap();
    let hits = offending_lines(&path);
    assert_eq!(
        hits.len(),
        1,
        "scanner missed a real UserKey::generate call: {hits:?}"
    );
}

#[test]
fn scanner_detects_a_real_agent_key_generate_call() {
    let body = "use agent_mesh_protocol::AgentKey;\nfn main() { let a = AgentKey::generate(); }\n";
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("rogue_agent.rs");
    fs::write(&path, body).unwrap();
    let hits = offending_lines(&path);
    assert_eq!(
        hits.len(),
        1,
        "scanner missed a real AgentKey::generate call: {hits:?}"
    );
}

#[test]
fn scanner_skips_doc_comments_and_strings() {
    let body = "/// example: UserKey::generate()\nfn main() { println!(\"AgentKey::generate is bad\"); }\n";
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("doc.rs");
    fs::write(&path, body).unwrap();
    assert!(
        offending_lines(&path).is_empty(),
        "doc-comment + string-literal hits must be ignored"
    );
}

#[test]
fn scanner_skips_cfg_test_blocks() {
    let body = r#"
fn live() {}
#[cfg(test)]
mod tests {
    use agent_mesh_protocol::UserKey;
    #[test]
    fn t() { let _ = UserKey::generate(); }
}
"#;
    let path_dir = tempfile::tempdir().unwrap();
    let path = path_dir.path().join("cfgtest.rs");
    fs::write(&path, body).unwrap();
    assert!(
        offending_lines(&path).is_empty(),
        "#[cfg(test)] hits must be ignored"
    );
}
