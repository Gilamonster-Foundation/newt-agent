//! Negative-existence guard: `agent-bridle` must be a **published crates.io**
//! release — never a `git`/`path` pin — and `Cargo.lock` must carry exactly one
//! bridle facade version, sourced from crates.io.
//!
//! Why this exists (#1256): a *temporary* git pin (the "API not yet published"
//! bridge #1235 used to reach agent-bridle's output-observer branch before it
//! shipped) is non-reproducible, and — the real footgun — a rebase can leave the
//! `Cargo.lock` carrying BOTH the git-pinned version AND a published one at once,
//! so the build links two incompatible bridle *type-universes*. That is exactly
//! what happened rebasing this branch over #1235's pin. This guard fails the
//! suite the moment a git/path pin, a git-sourced lock entry, or a duplicate
//! bridle version reappears — before it can ship.
//!
//! Fix when it fires: replace the git/path pin with the published release
//! (`agent-bridle = { version = "X.Y.Z", features = [...] }`) and regenerate the
//! lock (`cargo update -p agent-bridle`).

use std::fs;
use std::path::{Path, PathBuf};

/// The workspace root: `<root>/newt-acp-worker` is this test's manifest dir.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root is the manifest dir's parent")
        .to_path_buf()
}

/// Every `Cargo.toml` belonging to the **main** workspace (the one that shares
/// the root `Cargo.lock`). Skips `target/`, `.git/`, `docs/` (the results/bench
/// archive — standalone throwaway crates, not shipped), and any non-root
/// directory that has its OWN `Cargo.lock` (a separate workspace: it can't
/// pollute the main lock, so it is out of this guard's scope).
fn cargo_tomls(dir: &Path, out: &mut Vec<PathBuf>, is_root: bool) {
    if !is_root && dir.join("Cargo.lock").exists() {
        return; // a sibling workspace with its own lock
    }
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(name, "target" | ".git" | "docs") {
                continue;
            }
            cargo_tomls(&path, out, false);
        } else if path.file_name().and_then(|s| s.to_str()) == Some("Cargo.toml") {
            out.push(path);
        }
    }
}

/// Does this manifest line pin the `agent-bridle` **facade** to a `git`/`path`
/// source? Matches the facade dependency key only (`agent-bridle = …`), not the
/// sub-crates (`agent-bridle-core = …`), and never a comment.
fn is_bridle_source_pin(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with('#') {
        return false;
    }
    let names_facade = t.starts_with("agent-bridle ") || t.starts_with("agent-bridle=");
    names_facade
        && (t.contains("git =")
            || t.contains("git=")
            || t.contains("path =")
            || t.contains("path="))
}

#[test]
fn agent_bridle_is_published_not_git_or_path_pinned() {
    let root = workspace_root();
    let mut tomls = Vec::new();
    cargo_tomls(&root, &mut tomls, true);
    assert!(
        !tomls.is_empty(),
        "found no Cargo.toml to scan under {root:?}"
    );

    let mut offenders = Vec::new();
    for toml in &tomls {
        let Ok(body) = fs::read_to_string(toml) else {
            continue;
        };
        for (n, line) in body.lines().enumerate() {
            if is_bridle_source_pin(line) {
                offenders.push(format!(
                    "{}:{}: {}",
                    toml.strip_prefix(&root).unwrap_or(toml).display(),
                    n + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "agent-bridle must be a PUBLISHED crates.io version \
         (`agent-bridle = {{ version = \"X.Y.Z\", ... }}`), never a git/path pin — a temporary \
         git pin is a rebase footgun that can leave TWO bridle type-universes in the lock.\n\n\
         Offending manifest lines:\n{}\n\n\
         Fix: swap to the published release and `cargo update -p agent-bridle`.",
        offenders.join("\n")
    );
}

#[test]
fn cargo_lock_bridle_is_crates_io_and_unique() {
    let root = workspace_root();
    let lock = fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");

    let mut facade_count = 0usize;
    let mut git_sourced = Vec::new();
    for block in lock.split("[[package]]") {
        let (mut name, mut source, mut version) = (None, None, None);
        for line in block.lines() {
            let l = line.trim();
            if let Some(v) = l
                .strip_prefix("name = \"")
                .and_then(|s| s.strip_suffix('"'))
            {
                name = Some(v);
            } else if let Some(v) = l
                .strip_prefix("source = \"")
                .and_then(|s| s.strip_suffix('"'))
            {
                source = Some(v);
            } else if let Some(v) = l
                .strip_prefix("version = \"")
                .and_then(|s| s.strip_suffix('"'))
            {
                version = Some(v);
            }
        }
        let Some(name) = name else {
            continue;
        };
        if !name.starts_with("agent-bridle") {
            continue;
        }
        if name == "agent-bridle" {
            facade_count += 1;
        }
        if let Some(src) = source {
            if src.starts_with("git+") {
                git_sourced.push(format!("{name} {} ({src})", version.unwrap_or("?")));
            }
        }
    }

    assert!(
        git_sourced.is_empty(),
        "Cargo.lock has git-sourced agent-bridle package(s) — every bridle crate must come from \
         crates.io:\n{}",
        git_sourced.join("\n")
    );
    assert_eq!(
        facade_count, 1,
        "Cargo.lock must contain exactly ONE agent-bridle (facade) version (found {facade_count}) — \
         two versions link two incompatible bridle type-universes into the same build."
    );
}

// ── scanner self-tests: lock the heuristics so a future refactor can't silently
//    weaken the guard ─────────────────────────────────────────────────────────

#[test]
fn detector_flags_git_and_path_pins() {
    assert!(is_bridle_source_pin(
        r#"agent-bridle = { git = "https://x", rev = "abc", version = "0.7.5" }"#
    ));
    assert!(is_bridle_source_pin(
        r#"agent-bridle = { path = "../agent-bridle" }"#
    ));
}

#[test]
fn detector_allows_published_and_ignores_noise() {
    // A plain published version is fine.
    assert!(!is_bridle_source_pin(
        r#"agent-bridle = { version = "0.7.7", features = ["shell"] }"#
    ));
    // A comment is not a dependency line.
    assert!(!is_bridle_source_pin(
        r#"# agent-bridle = { git = "https://x" } — retired pin"#
    ));
    // Sub-crate keys are not the facade (the lock test covers their sources).
    assert!(!is_bridle_source_pin(
        r#"agent-bridle-core = { path = "agent-bridle-core" }"#
    ));
}
