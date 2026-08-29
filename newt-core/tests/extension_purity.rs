//! **E0a (#1863): extension handlers execute nothing and reach nothing.**
//!
//! The epic's security clause for extensions is that a handler "takes source
//! and returns a representation; it opens no file, spawns no process,
//! resolves no URL, and evaluates nothing the diagram author wrote."
//!
//! Half of that is held by the SIGNATURE — `fn render(&self, &str,
//! SupportLevel) -> Option<Enhancement>` receives no handle to anything and no
//! `&mut`, so it cannot be *given* authority. But a signature cannot stop a
//! body reaching for `std::fs` on its own, which is why the other half is this
//! scan. Structural, per the issue: enforced by the crate's dependency
//! surface, not by convention.
//!
//! Scoped to the extension module rather than the crate, because `newt-core`
//! at large legitimately touches the filesystem — this is a claim about where
//! handlers live, not about the crate.

// The shared production scanner. Each consumer uses a subset of it — this one
// needs the walker and the root, not the workspace-member machinery — and it
// is `mod`-included rather than a crate, so the unused half is dead in THIS
// binary. Allowed narrowly here rather than by widening the scanner's API or
// by copying the walk, which is what the reuse discipline is trying to avoid.
#[allow(dead_code)]
mod common;
use common::{for_each_production_line, workspace_root};
use std::path::Path;

/// Ambient authority a pure handler must never reach for.
///
/// `std::env` is included with the others: reading the environment is an
/// input a handler was not given, and it is the axis #1850 spent a slice
/// making single-threaded-safe. A renderer that varied with `NEWT_*` would be
/// impure in exactly the way that matters — the same source would not produce
/// the same picture.
const FORBIDDEN: &[&str] = &[
    "std::fs",
    "std::net",
    "std::process",
    "std::env",
    "File::",
    "Command::",
    "TcpStream",
    "include_str!",
    "include_bytes!",
    "unsafe ",
];

/// The module under the claim.
const EXTENSION_DIR: &str = "newt-core/src/markup/extension";

fn extension_sources() -> Vec<(String, String)> {
    let root = workspace_root();
    let dir = root.join(EXTENSION_DIR);
    let mut out = Vec::new();
    for_each_production_line(&[dir], &|_: &Path| false, &mut |path, code, _raw| {
        out.push((path.to_string_lossy().replace('\\', "/"), code.to_string()));
    });
    out
}

fn hits(lines: &[(String, String)]) -> Vec<String> {
    let mut found = Vec::new();
    for (path, code) in lines {
        for needle in FORBIDDEN {
            if code.contains(needle) {
                found.push(format!("{path}: {needle} in `{}`", code.trim()));
            }
        }
    }
    found
}

/// **The extension module reaches for no ambient authority.**
#[test]
fn extension_handlers_touch_no_ambient_authority() {
    let lines = extension_sources();
    // Anti-vacuous half (a): the scan must actually be reading the module. An
    // empty scan satisfies every `!contains` below perfectly, and a renamed
    // directory would produce exactly that.
    assert!(
        lines.len() > 50,
        "the scan saw only {} production line(s) under {EXTENSION_DIR} — it is \
         not reading the module, so the check below would pass vacuously",
        lines.len()
    );

    let found = hits(&lines);
    assert!(
        found.is_empty(),
        "an extension handler reached for ambient authority:\n{}",
        found.join("\n")
    );
}

/// **Anti-vacuous twin (b): the detector fires on source that really does it.**
///
/// The guard above is a `!contains` over a list; if the needles were wrong, or
/// the line filter blanked too much, it would pass over a handler that opened
/// a file. This points the same matcher at source that does.
#[test]
fn the_purity_scan_would_notice_a_handler_that_reached_out() {
    let offending = [
        ("probe.rs", "let s = std::fs::read_to_string(p).unwrap();"),
        ("probe.rs", "std::process::Command::new(\"sh\").spawn();"),
        ("probe.rs", "let host = std::env::var(\"NEWT_X\");"),
        ("probe.rs", "unsafe { std::env::set_var(\"A\", \"b\") };"),
    ];
    for (path, code) in offending {
        let found = hits(&[(path.to_string(), code.to_string())]);
        assert!(!found.is_empty(), "the purity scan did not notice: {code}");
    }
    // …and does NOT fire on ordinary handler code, or it would be a scan that
    // always fails, which is just as useless as one that never does.
    let benign = [
        "let shape = measure(source);",
        "out.push_str(&escape(line));",
        "for token in EDGES { if rest.starts_with(token) { edges += 1; } }",
    ];
    for code in benign {
        assert!(
            hits(&[("probe.rs".into(), code.to_string())]).is_empty(),
            "the purity scan fired on benign code: {code}"
        );
    }
}
