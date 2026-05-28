//! The five bundled evaluators.
//!
//! Every evaluator is a small post-condition check against an
//! [`EvalContext`]. The runner has already executed the worker by the
//! time these run; they look at the captured diff, the workspace
//! state, and (for Rust cases) shell out to `cargo`.
//!
//! Adding a new evaluator: implement [`Evaluator`], register it in
//! [`evaluator_by_name`] and [`default_evaluators`].

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use regex::Regex;

use crate::scorecard::{EvalContext, EvalResult};

/// Trait every check implements.
///
/// `evaluate` is intentionally sync — the heavy lifting (subprocess +
/// ACP I/O) happens in the runner, and evaluators are simple
/// post-condition checks against the resulting [`EvalContext`].
pub trait Evaluator: Send + Sync {
    /// Stable, kebab-case name (matches `case.toml` entries).
    fn name(&self) -> &str;
    /// Inspect the post-run context and return a verdict.
    fn evaluate(&self, ctx: &EvalContext) -> EvalResult;
}

// ── diff_nonempty ───────────────────────────────────────────────────

/// Verifies the worker actually produced edits.
///
/// Passes when `reply.diff` is non-empty AND `!reply.empty_diff`.
/// Empty-diff is the deterministic "the model said nothing useful"
/// signal — per `feedback_empty_diff_is_a_crash` it counts against the
/// model's scorecard.
pub struct DiffNonemptyEvaluator;

impl Evaluator for DiffNonemptyEvaluator {
    fn name(&self) -> &str {
        "diff_nonempty"
    }

    fn evaluate(&self, ctx: &EvalContext) -> EvalResult {
        if ctx.reply.empty_diff || ctx.reply.diff.trim().is_empty() {
            EvalResult::fail(
                self.name(),
                "captured diff is empty (worker produced no edits)",
            )
        } else {
            EvalResult::pass(
                self.name(),
                format!("captured {} bytes of diff", ctx.reply.diff.len()),
            )
        }
    }
}

// ── diff_applies ────────────────────────────────────────────────────

/// Verifies the worker's reply diff *would have applied* to the baseline.
///
/// Independent of whether the worker itself applied a patch — we copy
/// the baseline tree into a fresh tempdir, run `git apply --check`
/// against the diff, and pass on success.
///
/// If `reply.diff` is empty there's nothing to test, so we fail with a
/// clear reason — running this evaluator on a case where the worker
/// produced no diff is itself a meaningful negative signal.
pub struct DiffAppliesEvaluator;

impl Evaluator for DiffAppliesEvaluator {
    fn name(&self) -> &str {
        "diff_applies"
    }

    fn evaluate(&self, ctx: &EvalContext) -> EvalResult {
        if ctx.reply.diff.trim().is_empty() {
            return EvalResult::fail(self.name(), "no diff to apply");
        }

        let scratch = match tempfile::tempdir() {
            Ok(t) => t,
            Err(e) => return EvalResult::fail(self.name(), format!("tempdir: {e}")),
        };
        if let Err(e) = copy_tree(&ctx.baseline, scratch.path()) {
            return EvalResult::fail(self.name(), format!("copy baseline: {e}"));
        }
        if let Err(e) = git_init(scratch.path()) {
            return EvalResult::fail(self.name(), format!("git init: {e}"));
        }

        // Pipe the diff to `git apply --check`.
        let mut child = match Command::new("git")
            .args(["apply", "--check"])
            .current_dir(scratch.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return EvalResult::fail(self.name(), format!("spawn git apply: {e}")),
        };

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            if let Err(e) = stdin.write_all(ctx.reply.diff.as_bytes()) {
                return EvalResult::fail(self.name(), format!("write diff to git apply: {e}"));
            }
        }

        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => return EvalResult::fail(self.name(), format!("git apply wait: {e}")),
        };

        if output.status.success() {
            EvalResult::pass(self.name(), "git apply --check accepted the diff")
        } else {
            EvalResult::fail(
                self.name(),
                format!(
                    "git apply --check rejected the diff: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            )
        }
    }
}

// ── rust_compiles ───────────────────────────────────────────────────

/// `cargo check` on the post-worker workspace. Skipped (auto-pass with
/// a note) for non-Rust cases.
pub struct RustCompilesEvaluator;

impl Evaluator for RustCompilesEvaluator {
    fn name(&self) -> &str {
        "rust_compiles"
    }

    fn evaluate(&self, ctx: &EvalContext) -> EvalResult {
        if !ctx.case.is_rust() {
            return EvalResult::pass(self.name(), "non-Rust case — skipped");
        }
        let manifest = ctx.workspace.join("Cargo.toml");
        if !manifest.exists() {
            return EvalResult::fail(
                self.name(),
                format!("no Cargo.toml at {}", manifest.display()),
            );
        }

        let output = Command::new("cargo")
            .args(["check", "--quiet", "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", ctx.workspace.join("target"))
            .output();

        match output {
            Ok(o) if o.status.success() => EvalResult::pass(self.name(), "cargo check succeeded"),
            Ok(o) => EvalResult::fail(
                self.name(),
                format!(
                    "cargo check failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            ),
            Err(e) => EvalResult::fail(self.name(), format!("invoke cargo: {e}")),
        }
    }
}

// ── tests_pass ──────────────────────────────────────────────────────

/// `cargo test` on the post-worker workspace. Skipped for non-Rust
/// cases AND for Rust cases that have no `#[test]` anywhere under
/// `src/`.
pub struct TestsPassEvaluator;

impl Evaluator for TestsPassEvaluator {
    fn name(&self) -> &str {
        "tests_pass"
    }

    fn evaluate(&self, ctx: &EvalContext) -> EvalResult {
        if !ctx.case.is_rust() {
            return EvalResult::pass(self.name(), "non-Rust case — skipped");
        }
        let manifest = ctx.workspace.join("Cargo.toml");
        if !manifest.exists() {
            return EvalResult::fail(
                self.name(),
                format!("no Cargo.toml at {}", manifest.display()),
            );
        }
        if !workspace_has_tests(&ctx.workspace) {
            return EvalResult::pass(self.name(), "no #[test] found — skipped");
        }

        let output = Command::new("cargo")
            .args(["test", "--quiet", "--manifest-path"])
            .arg(&manifest)
            .env("CARGO_TARGET_DIR", ctx.workspace.join("target"))
            .output();

        match output {
            Ok(o) if o.status.success() => EvalResult::pass(self.name(), "cargo test succeeded"),
            Ok(o) => EvalResult::fail(
                self.name(),
                format!(
                    "cargo test failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            ),
            Err(e) => EvalResult::fail(self.name(), format!("invoke cargo: {e}")),
        }
    }
}

// ── pattern_match ───────────────────────────────────────────────────

/// At least one of `case.expected_patterns` must match the captured
/// diff. Each pattern is a `regex::Regex`.
///
/// If `expected_patterns` is empty the evaluator passes with a note —
/// the case author opted out of regex checks.
pub struct PatternMatchEvaluator;

impl Evaluator for PatternMatchEvaluator {
    fn name(&self) -> &str {
        "pattern_match"
    }

    fn evaluate(&self, ctx: &EvalContext) -> EvalResult {
        if ctx.case.expected_patterns.is_empty() {
            return EvalResult::pass(self.name(), "no expected_patterns configured");
        }

        let mut compiled = Vec::with_capacity(ctx.case.expected_patterns.len());
        for p in &ctx.case.expected_patterns {
            match Regex::new(p) {
                Ok(re) => compiled.push(re),
                Err(e) => {
                    return EvalResult::fail(self.name(), format!("invalid pattern '{p}': {e}"));
                }
            }
        }

        let matched: Vec<String> = compiled
            .iter()
            .filter(|re| re.is_match(&ctx.reply.diff))
            .map(|re| re.as_str().to_string())
            .collect();

        if matched.is_empty() {
            return EvalResult::fail(
                self.name(),
                format!(
                    "no expected pattern matched the diff (tried {})",
                    ctx.case.expected_patterns.len()
                ),
            );
        }

        // Partial-credit score: fraction of patterns that matched.
        let score = matched.len() as f64 / compiled.len() as f64;
        let passed = score > 0.0; // at least one match per docs
        EvalResult {
            evaluator: self.name().to_string(),
            passed,
            score,
            details: format!("{}/{} patterns matched", matched.len(), compiled.len()),
        }
    }
}

// ── Registry ────────────────────────────────────────────────────────

/// Lookup an evaluator by `case.toml` name. Returns `None` for unknown
/// names — callers should treat this as a configuration error.
pub fn evaluator_by_name(name: &str) -> Option<Arc<dyn Evaluator>> {
    match name {
        "diff_nonempty" => Some(Arc::new(DiffNonemptyEvaluator)),
        "diff_applies" => Some(Arc::new(DiffAppliesEvaluator)),
        "rust_compiles" => Some(Arc::new(RustCompilesEvaluator)),
        "tests_pass" => Some(Arc::new(TestsPassEvaluator)),
        "pattern_match" => Some(Arc::new(PatternMatchEvaluator)),
        _ => None,
    }
}

/// The full set of bundled evaluators in canonical order.
pub fn default_evaluators() -> Vec<Arc<dyn Evaluator>> {
    vec![
        Arc::new(DiffNonemptyEvaluator),
        Arc::new(DiffAppliesEvaluator),
        Arc::new(RustCompilesEvaluator),
        Arc::new(TestsPassEvaluator),
        Arc::new(PatternMatchEvaluator),
    ]
}

// ── Helpers ─────────────────────────────────────────────────────────

fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let mut opts = fs_extra::dir::CopyOptions::new();
    opts.content_only = true;
    opts.overwrite = true;
    fs_extra::dir::copy(src, dst, &opts)
        .map_err(|e| anyhow::anyhow!("copy {} -> {}: {e}", src.display(), dst.display()))?;
    Ok(())
}

fn git_init(path: &Path) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(path)
        .output()?;
    if !out.status.success() {
        anyhow::bail!("git init failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// True if any `.rs` file in `dir/src` (or `dir/tests`) contains
/// `#[test]`. Cheap heuristic — `cargo test` runs zero tests fine, so
/// this only exists to skip the expensive cargo invocation for cases
/// that obviously won't have any.
fn workspace_has_tests(dir: &Path) -> bool {
    for sub in ["src", "tests"] {
        let root = dir.join(sub);
        if !root.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.path().extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if text.contains("#[test]") {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cases::{MockResponse, TestCase};
    use newt_acp_worker::TaskReply;
    use std::path::PathBuf;

    fn make_ctx(
        diff: &str,
        expected_patterns: Vec<String>,
        language: &str,
        workspace: PathBuf,
        baseline: PathBuf,
    ) -> EvalContext {
        let reply = TaskReply::new("test-model", "content", diff, false).unwrap();
        let case = TestCase {
            name: "ctx".into(),
            description: "".into(),
            language: language.into(),
            prompt: "".into(),
            evaluators: vec![],
            expected_patterns,
            mock_response: MockResponse { content: "".into() },
            case_dir: PathBuf::new(),
        };
        EvalContext {
            case,
            workspace,
            baseline,
            reply,
        }
    }

    fn empty_dirs() -> (PathBuf, PathBuf) {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let pa = a.path().to_path_buf();
        let pb = b.path().to_path_buf();
        // Leak so the dirs survive the test scope.
        std::mem::forget(a);
        std::mem::forget(b);
        (pa, pb)
    }

    #[test]
    fn lookup_known_evaluators() {
        for name in [
            "diff_nonempty",
            "diff_applies",
            "rust_compiles",
            "tests_pass",
            "pattern_match",
        ] {
            let ev = evaluator_by_name(name).unwrap_or_else(|| panic!("missing: {name}"));
            assert_eq!(ev.name(), name);
        }
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(evaluator_by_name("nope").is_none());
    }

    #[test]
    fn default_set_has_five() {
        let evs = default_evaluators();
        assert_eq!(evs.len(), 5);
    }

    #[test]
    fn diff_nonempty_passes_on_diff() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx(
            "--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n-a\n+b\n",
            vec![],
            "rust",
            ws,
            bl,
        );
        let r = DiffNonemptyEvaluator.evaluate(&ctx);
        assert!(r.passed, "{}", r.details);
    }

    #[test]
    fn diff_nonempty_fails_on_empty() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx("", vec![], "rust", ws, bl);
        let r = DiffNonemptyEvaluator.evaluate(&ctx);
        assert!(!r.passed);
        assert!(r.details.contains("empty"));
    }

    #[test]
    fn diff_applies_passes_on_clean_apply() {
        let baseline = tempfile::tempdir().unwrap();
        std::fs::write(baseline.path().join("hello.txt"), "before\n").unwrap();
        let workspace = tempfile::tempdir().unwrap();

        let diff = "--- a/hello.txt\n+++ b/hello.txt\n@@ -1,1 +1,1 @@\n-before\n+after\n";
        let ctx = make_ctx(
            diff,
            vec![],
            "text",
            workspace.path().to_path_buf(),
            baseline.path().to_path_buf(),
        );

        let r = DiffAppliesEvaluator.evaluate(&ctx);
        assert!(r.passed, "expected apply to succeed: {}", r.details);
    }

    #[test]
    fn diff_applies_fails_on_dirty_context() {
        let baseline = tempfile::tempdir().unwrap();
        std::fs::write(baseline.path().join("hello.txt"), "actually different\n").unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let diff = "--- a/hello.txt\n+++ b/hello.txt\n@@ -1,1 +1,1 @@\n-before\n+after\n";
        let ctx = make_ctx(
            diff,
            vec![],
            "text",
            workspace.path().to_path_buf(),
            baseline.path().to_path_buf(),
        );

        let r = DiffAppliesEvaluator.evaluate(&ctx);
        assert!(!r.passed, "expected apply to fail");
    }

    #[test]
    fn diff_applies_fails_on_empty_diff() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx("", vec![], "text", ws, bl);
        let r = DiffAppliesEvaluator.evaluate(&ctx);
        assert!(!r.passed);
        assert!(r.details.contains("no diff"));
    }

    #[test]
    fn rust_compiles_skipped_for_non_rust() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx("diff", vec![], "python", ws, bl);
        let r = RustCompilesEvaluator.evaluate(&ctx);
        assert!(r.passed);
        assert!(r.details.contains("skipped"));
    }

    #[test]
    fn rust_compiles_fails_when_no_cargo_toml() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx("diff", vec![], "rust", ws, bl);
        let r = RustCompilesEvaluator.evaluate(&ctx);
        assert!(!r.passed);
        assert!(r.details.contains("Cargo.toml"));
    }

    #[test]
    fn tests_pass_skipped_for_non_rust() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx("diff", vec![], "python", ws, bl);
        let r = TestsPassEvaluator.evaluate(&ctx);
        assert!(r.passed);
        assert!(r.details.contains("skipped"));
    }

    #[test]
    fn tests_pass_skipped_when_no_tests() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let (_, bl) = empty_dirs();
        let ctx = make_ctx("diff", vec![], "rust", workspace.path().to_path_buf(), bl);
        let r = TestsPassEvaluator.evaluate(&ctx);
        assert!(r.passed);
        assert!(r.details.contains("no #[test]"));
    }

    #[test]
    fn workspace_has_tests_detects_test_attr() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "#[test]\nfn t() {}").unwrap();
        assert!(workspace_has_tests(dir.path()));
    }

    #[test]
    fn workspace_has_tests_false_without_test() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn f() {}").unwrap();
        assert!(!workspace_has_tests(dir.path()));
    }

    #[test]
    fn pattern_match_passes_when_pattern_present() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx(
            "+fn hello() {}\n",
            vec!["fn hello".to_string()],
            "rust",
            ws,
            bl,
        );
        let r = PatternMatchEvaluator.evaluate(&ctx);
        assert!(r.passed);
        assert!(r.score > 0.0);
    }

    #[test]
    fn pattern_match_fails_when_pattern_absent() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx(
            "+fn other() {}\n",
            vec!["fn hello".to_string()],
            "rust",
            ws,
            bl,
        );
        let r = PatternMatchEvaluator.evaluate(&ctx);
        assert!(!r.passed);
    }

    #[test]
    fn pattern_match_passes_when_no_patterns_configured() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx("+anything\n", vec![], "rust", ws, bl);
        let r = PatternMatchEvaluator.evaluate(&ctx);
        assert!(r.passed);
    }

    #[test]
    fn pattern_match_invalid_regex_fails_cleanly() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx("diff", vec!["[unclosed".to_string()], "rust", ws, bl);
        let r = PatternMatchEvaluator.evaluate(&ctx);
        assert!(!r.passed);
        assert!(r.details.contains("invalid pattern"));
    }

    #[test]
    fn pattern_match_partial_credit_score() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx(
            "+fn hello() {}\n+let x = 1;\n",
            vec!["fn hello".to_string(), "missing".to_string()],
            "rust",
            ws,
            bl,
        );
        let r = PatternMatchEvaluator.evaluate(&ctx);
        assert!(r.passed);
        assert!((r.score - 0.5).abs() < 1e-9, "score = {}", r.score);
    }
}
