//! Parity / characterization tests for the patch appliers.
//!
//! Documents where the default [`FuzzyApplier`] and the strict
//! `git apply --check` oracle AGREE and where they intentionally DIVERGE.
//!
//! The divergence on a header-lying diff is the empirical basis for #30:
//! the worker (fuzzy) is deliberately robust — it locates and applies a
//! hunk whose line numbers are off — but `git apply --check` rejects the
//! same diff. That is *why* the eval scorecard must judge the model's raw
//! emission with the strict oracle (newt-eval's `diff_applies`, #30B)
//! rather than the post-hoc captured diff: otherwise a model that emits a
//! lying diff the fuzzy worker rescues looks identical to one that emitted
//! a clean diff.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use newt_tools::{FuzzyApplier, PatchApplier};
use tempfile::TempDir;

/// The bundled `001-rename-function` workspace seed (13 lines).
const SEED_001: &str = r#"pub fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets() {
        assert_eq!(greet("a"), "Hello, a!");
    }
}
"#;

/// Join diff lines (blank context lines are passed as `" "`) and add a
/// trailing newline. Keeps trailing-space markers explicit in source.
fn diff_from(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

fn seed_tree(seed: &[(&str, &str)]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    for (rel, contents) in seed {
        let p = tmp.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, contents).unwrap();
    }
    tmp
}

/// Whether the fuzzy applier applies `diff` against a fresh `seed` tree.
fn fuzzy_accepts(seed: &[(&str, &str)], diff: &str) -> bool {
    let tmp = seed_tree(seed);
    FuzzyApplier.apply(tmp.path(), diff).is_ok()
}

/// Whether real `git apply --check` accepts `diff` against `seed`.
fn git_apply_accepts(seed: &[(&str, &str)], diff: &str) -> bool {
    let tmp = seed_tree(seed);
    run_git(tmp.path(), &["init", "-q"]);
    let mut child = Command::new("git")
        .args(["apply", "--check"])
        .current_dir(tmp.path())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .unwrap();
    child.wait().unwrap().success()
}

fn run_git(dir: &Path, args: &[&str]) {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        // Same scrub as `git_apply_check` above: under a git hook (the
        // pre-push gate runs `cargo test`), leaked GIT_DIR/GIT_WORK_TREE
        // would aim this at the real repo instead of the tempdir.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
}

/// A correct rename diff with an accurate `@@ -1,13 +1,13 @@` header.
fn clean_rename_diff() -> String {
    diff_from(&[
        "--- a/src/lib.rs",
        "+++ b/src/lib.rs",
        "@@ -1,13 +1,13 @@",
        "-pub fn greet(name: &str) -> String {",
        "+pub fn hello(name: &str) -> String {",
        "     format!(\"Hello, {name}!\")",
        " }",
        " ",
        " #[cfg(test)]",
        " mod tests {",
        "     use super::*;",
        " ",
        "     #[test]",
        "     fn greets() {",
        "-        assert_eq!(greet(\"a\"), \"Hello, a!\");",
        "+        assert_eq!(hello(\"a\"), \"Hello, a!\");",
        "     }",
        " }",
    ])
}

/// The devstral case-001 *lying* diff from issue #30: the header claims
/// 11 old lines but the body spans 13, and `pub fn greet` is emitted as a
/// context line so only the call site is renamed (producing code that
/// calls an undefined `hello`).
fn devstral_lying_diff() -> String {
    diff_from(&[
        "--- a/src/lib.rs",
        "+++ b/src/lib.rs",
        "@@ -1,11 +1,11 @@",
        " pub fn greet(name: &str) -> String {",
        "     format!(\"Hello, {name}!\")",
        " }",
        " ",
        " #[cfg(test)]",
        " mod tests {",
        "     use super::*;",
        " ",
        "     #[test]",
        "     fn greets() {",
        "-        assert_eq!(greet(\"a\"), \"Hello, a!\");",
        "+        assert_eq!(hello(\"a\"), \"Hello, a!\");",
        "     }",
        " }",
    ])
}

fn new_file_diff() -> String {
    diff_from(&[
        "--- /dev/null",
        "+++ b/src/util.rs",
        "@@ -0,0 +1,3 @@",
        "+pub fn helper() -> i32 {",
        "+    42",
        "+}",
    ])
}

fn nonmatching_diff() -> String {
    diff_from(&[
        "--- a/src/lib.rs",
        "+++ b/src/lib.rs",
        "@@ -1,1 +1,1 @@",
        " fn TOTALLY_WRONG() {",
        "-old",
        "+new",
    ])
}

// ── Agreement cases ──────────────────────────────────────────────────────────

#[test]
fn clean_diff_both_accept() {
    let seed = [("src/lib.rs", SEED_001)];
    let diff = clean_rename_diff();
    assert!(
        fuzzy_accepts(&seed, &diff),
        "fuzzy must accept a clean diff"
    );
    assert!(
        git_apply_accepts(&seed, &diff),
        "git apply --check must accept a clean diff"
    );
}

#[test]
fn new_file_both_accept() {
    // No seed: the diff creates src/util.rs from /dev/null.
    let seed: [(&str, &str); 0] = [];
    let diff = new_file_diff();
    assert!(
        fuzzy_accepts(&seed, &diff),
        "fuzzy must create a new file from a /dev/null diff"
    );
    assert!(
        git_apply_accepts(&seed, &diff),
        "git apply --check must accept a new-file diff"
    );
}

#[test]
fn genuinely_wrong_both_reject() {
    let seed = [("src/lib.rs", SEED_001)];
    let diff = nonmatching_diff();
    assert!(
        !fuzzy_accepts(&seed, &diff),
        "fuzzy must reject a hunk that matches nothing"
    );
    assert!(
        !git_apply_accepts(&seed, &diff),
        "git apply --check must reject a hunk that matches nothing"
    );
}

// ── Documented divergence (the #30 motivation) ───────────────────────────────

#[test]
fn lying_header_diff_diverges_fuzzy_accepts_git_rejects() {
    let seed = [("src/lib.rs", SEED_001)];
    let diff = devstral_lying_diff();
    // The fuzzy worker rescues it (robustness)…
    assert!(
        fuzzy_accepts(&seed, &diff),
        "fuzzy applier is expected to apply the lying diff (robust worker)"
    );
    // …but the strict oracle rejects it. The eval scorecard (diff_applies)
    // judges the raw emission with this oracle, so the model is correctly
    // scored as having emitted an unappliable diff.
    assert!(
        !git_apply_accepts(&seed, &diff),
        "git apply --check is expected to reject the lying diff (strict oracle)"
    );
}

// ── Strict diffy backend (opt-in) ────────────────────────────────────────────

#[cfg(feature = "applier-diffy")]
#[test]
fn diffy_backend_matches_strict_oracle() {
    use newt_tools::DiffyApplier;

    let seed = [("src/lib.rs", SEED_001)];
    let clean = clean_rename_diff();
    let lying = devstral_lying_diff();

    let diffy_accepts = |diff: &str| {
        let tmp = seed_tree(&seed);
        DiffyApplier.apply(tmp.path(), diff).is_ok()
    };

    assert!(diffy_accepts(&clean), "diffy must accept a clean diff");
    assert!(
        !diffy_accepts(&lying),
        "diffy (strict) must reject the lying diff, like git apply --check"
    );
}
