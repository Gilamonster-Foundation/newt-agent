//! The five bundled evaluators.
//!
//! Every evaluator is a small post-condition check against an
//! [`EvalContext`]. The runner has already executed the worker by the
//! time these run; they look at the captured diff, the workspace
//! state, and (for Rust cases) shell out to `cargo`.
//!
//! Adding a new evaluator: implement [`Evaluator`], register it in
//! [`evaluator_by_name`] and [`default_evaluators`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use newt_core::symbols::{extract_references, Lang};
use regex::Regex;
use serde::Deserialize;

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
        // #30B: judge the model's first raw emission when it is diff-shaped
        // (the strict oracle on what the model actually produced), else fall
        // back to the captured workspace diff. The captured diff is
        // well-formed by construction and so always "applies" — checking it
        // is what let header-lying diffs score `ok` despite real
        // `git apply --check` rejecting them.
        let (target, artifact) = select_apply_target(ctx);
        if target.trim().is_empty() {
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

        // Pipe the diff to `git apply --check`. env_remove() ensures the
        // check targets `scratch`, not whichever repo the caller's
        // inherited GIT_DIR points at.
        //
        // `--ignore-space-change` makes the check tolerant of CRLF/LF and
        // whitespace-run differences so a worker diff still validates across
        // platforms (Windows checkouts are CRLF). Trade-off: the evaluator no
        // longer rejects a diff that ONLY differs by whitespace — acceptable
        // here, since we are gating "does a real change apply", not whitespace
        // fidelity.
        let mut child = match Command::new("git")
            .args(["apply", "--check", "--ignore-space-change"])
            .current_dir(scratch.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_PREFIX")
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
            if let Err(e) = stdin.write_all(target.as_bytes()) {
                return EvalResult::fail(self.name(), format!("write diff to git apply: {e}"));
            }
        }

        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => return EvalResult::fail(self.name(), format!("git apply wait: {e}")),
        };

        if output.status.success() {
            EvalResult::pass(
                self.name(),
                format!("git apply --check accepted the {artifact}"),
            )
        } else {
            EvalResult::fail(
                self.name(),
                format!(
                    "git apply --check rejected the {artifact}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            )
        }
    }
}

/// Choose what `diff_applies` feeds to `git apply --check`.
///
/// Returns the diff text plus a human label for the verdict message. When
/// the worker surfaced the model's first raw emission and it is diff-shaped
/// (after peeling a single ``` fence), we judge *that* — the strict oracle
/// on the model's actual output (#30B). Otherwise (whole-file or prose
/// emissions, or legacy replies with no `raw_emission`) we fall back to the
/// captured workspace diff, preserving the previous behavior.
fn select_apply_target(ctx: &EvalContext) -> (String, &'static str) {
    if let Some(raw) = ctx.reply.raw_emission.as_deref() {
        let mut stripped = strip_outer_fences(raw);
        stripped = stripped.trim_start_matches(['\r', '\n']).to_string();
        if looks_like_unified_diff(&stripped) {
            // `strip_outer_fences` trims the trailing newline (along with any
            // closing fence); `git apply` wants one on the final hunk line.
            if !stripped.ends_with('\n') {
                stripped.push('\n');
            }
            return (stripped, "raw emission");
        }
    }
    (ctx.reply.diff.clone(), "captured diff")
}

/// Peel a single enclosing ``` fence so a correct diff wrapped in a
/// ```diff … ``` block is not falsely rejected by `git apply --check`.
///
/// Unlike newt-coder's `strip_outer_fences` (which trims aggressively for
/// parsing), this preserves the diff bytes verbatim when there is no fence
/// — a trailing blank *context* line (` `) is significant to `git apply`
/// and must not be trimmed away.
fn strip_outer_fences(raw: &str) -> String {
    if !raw.trim_matches('\n').starts_with("```") {
        return raw.to_string();
    }
    // Fenced: drop the opening fence line (with optional language tag) and a
    // trailing line that is only ```. Rejoin verbatim with a trailing newline.
    let mut lines: Vec<&str> = raw.trim_matches('\n').lines().collect();
    if !lines.is_empty() {
        lines.remove(0);
    }
    if lines.last().is_some_and(|l| l.trim() == "```") {
        lines.pop();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Detect a unified diff by header pattern (mirrors newt-coder's
/// `try_parse_unified_diff`): a `--- `/`+++ ` header pair plus a `@@ ` hunk.
fn looks_like_unified_diff(body: &str) -> bool {
    let has_minus = body.starts_with("--- ") || body.contains("\n--- ");
    let has_plus = body.contains("\n+++ ");
    let has_hunk = body.contains("\n@@ ") || body.contains("@@ -");
    has_minus && has_plus && has_hunk
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

// ── python_imports ──────────────────────────────────────────────────

/// The declared "real" Python surface for a case — the modules the project
/// genuinely exposes. Loaded from `<case_dir>/python_surface.json`. Later this
/// same shape is auto-generated by the FFI-introspection manifest (#74), so the
/// PyO3 cross-language surface (e.g. `newt_agent.core`) becomes ground truth
/// rather than a hand-maintained list.
#[derive(Debug, Clone, Default, Deserialize)]
struct PythonSurface {
    /// Fully-qualified module paths that really exist (`"newt_agent.core"`).
    #[serde(default)]
    modules: Vec<String>,
}

/// Resolve Python imports in the worker's output against the known module
/// surface — the build-free *cheap tier* of the S1 verify oracle (#332/#339),
/// applied as an eval scorer. It catches the **fabricated-module** class
/// (`from newt_core import ...` when no such module exists) that
/// `rust_compiles` / `tests_pass` auto-pass on and `python -m py_compile` is
/// blind to.
///
/// CRAWL scope: **module-existence only**. The symbol-level check ("does
/// `newt_agent.core` actually export `Router`?") needs the full FFI manifest
/// (#74) and is a later increment. Skipped (auto-pass) for non-Python cases and
/// when no `python_surface.json` declares the real surface.
pub struct PythonImportsEvaluator;

impl Evaluator for PythonImportsEvaluator {
    fn name(&self) -> &str {
        "python_imports"
    }

    fn evaluate(&self, ctx: &EvalContext) -> EvalResult {
        if !ctx.case.language.eq_ignore_ascii_case("python") {
            return EvalResult::pass(self.name(), "non-Python case — skipped");
        }
        // The declared real surface (project modules). Without it the crawl
        // check has no ground truth — skip honestly rather than guess.
        let surface_path = ctx.case.case_dir.join("python_surface.json");
        let surface: PythonSurface = match std::fs::read_to_string(&surface_path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(e) => {
                    return EvalResult::fail(
                        self.name(),
                        format!("invalid {}: {e}", surface_path.display()),
                    );
                }
            },
            Err(_) => {
                return EvalResult::pass(
                    self.name(),
                    "no python_surface.json — module check skipped",
                );
            }
        };

        let mut known = newt_core::symbols::python_stdlib_modules();
        known.extend(surface.modules.iter().cloned());

        let files = collect_python_files(&ctx.workspace);
        if files.is_empty() {
            return EvalResult::pass(self.name(), "no Python files in output");
        }

        let report = score_python_imports(&known, &files);
        if report.total == 0 {
            return EvalResult::pass(self.name(), "no imports to resolve");
        }
        let score = report.resolved as f64 / report.total as f64;
        if report.fabricated.is_empty() {
            EvalResult::pass(
                self.name(),
                format!(
                    "{}/{} imports resolve to real modules",
                    report.resolved, report.total
                ),
            )
        } else {
            let names: Vec<String> = report
                .fabricated
                .iter()
                .map(|f| format!("{}:{} `{}`", f.file, f.line, f.module))
                .collect();
            EvalResult {
                evaluator: self.name().to_string(),
                passed: false,
                score,
                details: format!(
                    "{} fabricated module import(s) [module-level/crawl]: {}",
                    report.fabricated.len(),
                    names.join(", ")
                ),
            }
        }
    }
}

/// One fabricated module reference found in the output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fabrication {
    file: String,
    line: usize,
    module: String,
}

/// The result of scoring a set of Python files' imports against a known surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ImportReport {
    total: usize,
    resolved: usize,
    fabricated: Vec<Fabrication>,
}

/// Pure scorer (unit-testable): for every import in every file, a module is
/// "known" if it — or any dotted prefix of it — is in `known`. References to
/// unknown modules are fabrications.
fn score_python_imports(known: &BTreeSet<String>, files: &[(String, String)]) -> ImportReport {
    let mut report = ImportReport::default();
    for (path, source) in files {
        for r in extract_references(source, Lang::Python) {
            report.total += 1;
            if newt_core::symbols::module_is_known(&r.module, known) {
                report.resolved += 1;
            } else {
                report.fabricated.push(Fabrication {
                    file: path.clone(),
                    line: r.line,
                    module: r.module.clone(),
                });
            }
        }
    }
    report
}

/// Recursively collect `*.py` files under `root` (skipping the usual noise
/// dirs), returning `(relative-path, contents)` pairs sorted by path.
fn collect_python_files(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    collect_py_into(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn collect_py_into(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name.as_ref(),
                "target" | ".venv" | "venv" | "__pycache__" | ".git" | "node_modules"
            ) {
                continue;
            }
            collect_py_into(root, &path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
            if let Ok(src) = std::fs::read_to_string(&path) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                out.push((rel, src));
            }
        }
    }
}

// ── output_matches (result oracle, #957) ────────────────────────────

/// A command to run for the `output_matches` oracle.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSpec {
    /// argv to execute; `argv[0]` is the program.
    pub argv: Vec<String>,
    /// Working directory (the graded worktree).
    pub cwd: PathBuf,
    /// Optional wall-clock timeout in milliseconds.
    pub timeout_ms: Option<u64>,
}

/// What a [`CommandRunner`] captured from one run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    pub stdout: String,
    pub stderr: String,
    /// Process exit code; `None` if it did not exit normally (signal / not run).
    pub exit_code: Option<i32>,
    /// True if the runner killed it for exceeding `timeout_ms`.
    pub timed_out: bool,
}

/// The injected seam for running the graded program (#957). Mocked in unit
/// tests and implemented by [`SubprocessRunner`] for real execution, so the
/// evaluator's compare logic is fully exercised without spawning a process.
pub trait CommandRunner: Send + Sync {
    fn run(&self, spec: &RunSpec) -> RunOutcome;
}

/// The real `CommandRunner` (#960): actually executes `spec.argv` in the graded
/// worktree, captures stdout/stderr, and enforces a hard wall-clock timeout.
///
/// **Trust boundary.** The graded program is *untrusted* code produced by a
/// model. This runner does **not** sandbox it — no network isolation, no
/// filesystem confinement beyond `cwd`. That is deliberately out of scope here
/// (the confinement story is the OCAP shell engine; see the harness-attack
/// surface tracked in #887). Only run `output_matches` cases you trust, and only
/// in the weekly/release tiers. A `NEWT_EVAL_SANDBOX=1` marker is set in the
/// child env as an advisory signal, not an enforced boundary.
pub struct SubprocessRunner;

impl CommandRunner for SubprocessRunner {
    fn run(&self, spec: &RunSpec) -> RunOutcome {
        use std::io::Read;
        use std::process::Stdio;
        use std::time::{Duration, Instant};

        let Some((program, rest)) = spec.argv.split_first() else {
            return RunOutcome {
                stdout: String::new(),
                stderr: "output_matches: [output_match].run is empty".to_string(),
                exit_code: None,
                timed_out: false,
            };
        };

        let mut child = match Command::new(program)
            .args(rest)
            .current_dir(&spec.cwd)
            // Advisory marker (see the trust-boundary note above); not enforced.
            .env("NEWT_EVAL_SANDBOX", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            // Missing interpreter/binary is the common misconfiguration — report
            // it honestly and never auto-download.
            Err(e) => {
                return RunOutcome {
                    stdout: String::new(),
                    stderr: format!("runtime not found: {program} ({e})"),
                    exit_code: None,
                    timed_out: false,
                };
            }
        };

        // Drain both pipes on threads so the child can't deadlock on a full pipe
        // while we poll for exit.
        let read_pipe = |pipe: Option<std::process::ChildStdout>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut p) = pipe {
                    let _ = p.read_to_end(&mut buf);
                }
                buf
            })
        };
        let read_err = |pipe: Option<std::process::ChildStderr>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut p) = pipe {
                    let _ = p.read_to_end(&mut buf);
                }
                buf
            })
        };
        let out_handle = read_pipe(child.stdout.take());
        let err_handle = read_err(child.stderr.take());

        // Poll for exit up to the deadline; kill on expiry. `None` timeout means
        // "no wall-clock budget" — wait indefinitely.
        let deadline = spec
            .timeout_ms
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if let Some(dl) = deadline {
                        if Instant::now() >= dl {
                            let _ = child.kill();
                            let _ = child.wait();
                            timed_out = true;
                            break None;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };

        let stdout = String::from_utf8_lossy(&out_handle.join().unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&err_handle.join().unwrap_or_default()).into_owned();
        RunOutcome {
            stdout,
            stderr,
            exit_code: status.and_then(|s| s.code()),
            timed_out,
        }
    }
}

/// The result oracle: run the graded program via the injected [`CommandRunner`]
/// and assert its stdout equals the case's `expected_output`, compared through
/// the case's [`normalize`](crate::normalize) pipeline (empty list = exact
/// compare). Opt-in per case via `evaluators = ["output_matches"]`; passes with
/// a skip note when the case declares no `expected_output` / `[output_match]`.
pub struct OutputMatchesEvaluator<R: CommandRunner> {
    runner: R,
}

impl<R: CommandRunner> OutputMatchesEvaluator<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }
}

impl<R: CommandRunner> Evaluator for OutputMatchesEvaluator<R> {
    fn name(&self) -> &str {
        "output_matches"
    }

    fn evaluate(&self, ctx: &EvalContext) -> EvalResult {
        let Some(expected) = ctx.case.expected_output.as_ref() else {
            return EvalResult::pass(self.name(), "no expected_output configured");
        };
        let Some(om) = ctx.case.output_match.as_ref() else {
            return EvalResult::pass(self.name(), "no [output_match] configured");
        };
        if om.run.is_empty() {
            return EvalResult::fail(self.name(), "[output_match].run is empty");
        }
        let spec = RunSpec {
            argv: om.run.clone(),
            cwd: ctx.workspace.clone(),
            timeout_ms: om.timeout_ms,
        };
        let outcome = self.runner.run(&spec);
        if outcome.timed_out {
            return EvalResult::fail(
                self.name(),
                format!("run timed out after {:?} ms: {:?}", om.timeout_ms, om.run),
            );
        }
        if outcome.exit_code != Some(0) {
            return EvalResult::fail(
                self.name(),
                format!(
                    "run exited with {:?} (stderr: {})",
                    outcome.exit_code,
                    outcome.stderr.trim()
                ),
            );
        }
        // Compare through the case's normalization pipeline (#959). An empty
        // `normalize` list is an exact byte compare (the step-1 default).
        let cmp = crate::normalize::compare(&outcome.stdout, expected, om);
        let warn_note = if cmp.warnings.is_empty() {
            String::new()
        } else {
            format!(" (normalize warnings: {})", cmp.warnings.join("; "))
        };
        if cmp.equal {
            EvalResult::pass(
                self.name(),
                format!("stdout matched expected_output{warn_note}"),
            )
        } else {
            EvalResult::fail(
                self.name(),
                format!(
                    "stdout != expected_output ({} vs {} bytes){warn_note}",
                    outcome.stdout.len(),
                    expected.len()
                ),
            )
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
        "python_imports" => Some(Arc::new(PythonImportsEvaluator)),
        // Opt-in result oracle (#957); NOT in `default_evaluators`. Executes the
        // graded program for real via `SubprocessRunner` (#960) — see its
        // trust-boundary note; run only trusted cases, weekly/release tier.
        "output_matches" => Some(Arc::new(OutputMatchesEvaluator::new(SubprocessRunner))),
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
        Arc::new(PythonImportsEvaluator),
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
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_PREFIX")
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
            expected_output: None,
            output_match: None,
            mock_response: MockResponse { content: "".into() },
            difficulty: "L1".into(),
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

    // ── output_matches (#957 step 1) — fully mocked runner ──────────

    struct MockRunner(RunOutcome);
    impl CommandRunner for MockRunner {
        fn run(&self, _spec: &RunSpec) -> RunOutcome {
            self.0.clone()
        }
    }
    fn oc(stdout: &str, exit: Option<i32>, timed_out: bool) -> RunOutcome {
        RunOutcome {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: exit,
            timed_out,
        }
    }
    fn oracle_ctx(expected: Option<&str>, run: &[&str]) -> EvalContext {
        let (ws, base) = empty_dirs();
        let mut ctx = make_ctx("", vec![], "rust", ws, base);
        ctx.case.expected_output = expected.map(str::to_string);
        ctx.case.output_match = Some(crate::cases::OutputMatch {
            run: run.iter().map(|s| (*s).to_string()).collect(),
            timeout_ms: Some(1000),
            ..Default::default()
        });
        ctx
    }

    #[test]
    fn output_matches_passes_when_stdout_equals_expected() {
        let ctx = oracle_ctx(Some("42\n"), &["echo", "42"]);
        let r = OutputMatchesEvaluator::new(MockRunner(oc("42\n", Some(0), false))).evaluate(&ctx);
        assert!(r.passed, "{r:?}");
    }

    #[test]
    fn output_matches_fails_when_stdout_differs() {
        let ctx = oracle_ctx(Some("42\n"), &["echo", "43"]);
        let r = OutputMatchesEvaluator::new(MockRunner(oc("43\n", Some(0), false))).evaluate(&ctx);
        assert!(!r.passed, "{r:?}");
    }

    #[test]
    fn output_matches_fails_on_nonzero_exit() {
        let ctx = oracle_ctx(Some("42\n"), &["false"]);
        let r = OutputMatchesEvaluator::new(MockRunner(oc("", Some(1), false))).evaluate(&ctx);
        assert!(!r.passed && r.details.contains("exited"), "{r:?}");
    }

    #[test]
    fn output_matches_fails_on_timeout() {
        let ctx = oracle_ctx(Some("42\n"), &["sleep", "99"]);
        let r = OutputMatchesEvaluator::new(MockRunner(oc("", None, true))).evaluate(&ctx);
        assert!(!r.passed && r.details.contains("timed out"), "{r:?}");
    }

    #[test]
    fn output_matches_skips_without_expected_output() {
        let (ws, base) = empty_dirs();
        let ctx = make_ctx("", vec![], "rust", ws, base); // expected_output = None
        let r = OutputMatchesEvaluator::new(MockRunner(oc("x", Some(0), false))).evaluate(&ctx);
        assert!(
            r.passed && r.details.contains("no expected_output"),
            "{r:?}"
        );
    }

    #[test]
    fn output_matches_applies_normalize_pipeline_before_compare() {
        // stdout has a trailing newline + extra whitespace the expected lacks;
        // trim + collapse_whitespace reconcile them, so the oracle passes (#959).
        let mut ctx = oracle_ctx(Some("a b"), &["echo", "a b"]);
        if let Some(om) = ctx.case.output_match.as_mut() {
            om.normalize = vec!["trim".into(), "collapse_whitespace".into()];
        }
        let r = OutputMatchesEvaluator::new(MockRunner(oc("  a   b \n", Some(0), false)))
            .evaluate(&ctx);
        assert!(r.passed, "{r:?}");
    }

    #[test]
    fn output_matches_surfaces_normalize_warning_for_unknown_strategy() {
        let mut ctx = oracle_ctx(Some("x"), &["echo", "x"]);
        if let Some(om) = ctx.case.output_match.as_mut() {
            om.normalize = vec!["bogus".into()];
        }
        let r = OutputMatchesEvaluator::new(MockRunner(oc("x", Some(0), false))).evaluate(&ctx);
        assert!(r.passed, "unknown strategy is non-fatal: {r:?}");
        assert!(r.details.contains("bogus"), "warning surfaced: {r:?}");
    }

    #[test]
    fn subprocess_runner_reports_runtime_not_found_for_missing_binary() {
        // Hermetic + instant (no real heavy process): spawning a non-existent
        // program fails immediately with a clear diagnostic (#960).
        let spec = RunSpec {
            argv: vec!["newt-eval-no-such-binary-xyzzy".to_string()],
            cwd: std::env::temp_dir(),
            timeout_ms: Some(1000),
        };
        let out = SubprocessRunner.run(&spec);
        assert_eq!(out.exit_code, None);
        assert!(!out.timed_out);
        assert!(
            out.stderr.contains("runtime not found"),
            "stderr: {}",
            out.stderr
        );
    }

    #[test]
    fn subprocess_runner_reports_empty_argv() {
        let spec = RunSpec {
            argv: vec![],
            cwd: std::env::temp_dir(),
            timeout_ms: None,
        };
        let out = SubprocessRunner.run(&spec);
        assert_eq!(out.exit_code, None);
        assert!(out.stderr.contains("empty"), "stderr: {}", out.stderr);
    }

    #[test]
    fn output_matches_resolves_by_name_and_is_not_a_default() {
        assert_eq!(
            evaluator_by_name("output_matches").unwrap().name(),
            "output_matches"
        );
        assert!(
            !default_evaluators()
                .iter()
                .any(|e| e.name() == "output_matches"),
            "output_matches must stay opt-in (not in default_evaluators)"
        );
    }

    #[test]
    fn case_toml_deserializes_output_match_fields() {
        let toml = r#"
name = "d"
description = ""
language = "python"
prompt = ""
evaluators = ["output_matches"]
expected_output = "42\n"
[output_match]
run = ["python3", "solution.py"]
timeout_ms = 5000
[mock_response]
content = ""
"#;
        let c: crate::cases::TestCase = toml::from_str(toml).unwrap();
        assert_eq!(c.expected_output.as_deref(), Some("42\n"));
        let om = c.output_match.expect("output_match parsed");
        assert_eq!(
            om.run,
            vec!["python3".to_string(), "solution.py".to_string()]
        );
        assert_eq!(om.timeout_ms, Some(5000));
        assert!(c.evaluators.contains(&"output_matches".to_string()));
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
    fn default_set_has_all_evaluators() {
        let evs = default_evaluators();
        assert_eq!(evs.len(), 6);
        assert!(evs.iter().any(|e| e.name() == "python_imports"));
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

    // ── #30B: judge the model's raw first emission ──────────────────────

    /// The bundled `001-rename-function` seed (13 lines).
    const SEED_001: &str = "pub fn greet(name: &str) -> String {\n    format!(\"Hello, {name}!\")\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn greets() {\n        assert_eq!(greet(\"a\"), \"Hello, a!\");\n    }\n}\n";

    /// A correct rename diff with an accurate header (blank context lines
    /// carry their leading space).
    const CLEAN_RENAME_DIFF: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,13 +1,13 @@\n-pub fn greet(name: &str) -> String {\n+pub fn hello(name: &str) -> String {\n     format!(\"Hello, {name}!\")\n }\n \n #[cfg(test)]\n mod tests {\n     use super::*;\n \n     #[test]\n     fn greets() {\n-        assert_eq!(greet(\"a\"), \"Hello, a!\");\n+        assert_eq!(hello(\"a\"), \"Hello, a!\");\n     }\n }\n";

    /// The devstral lying diff from #30: header claims 11 old lines but the
    /// body spans 13, and `pub fn greet` is left as context. `git apply
    /// --check` rejects it; the fuzzy worker would rescue it.
    const LYING_DIFF: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,11 +1,11 @@\n pub fn greet(name: &str) -> String {\n     format!(\"Hello, {name}!\")\n }\n \n #[cfg(test)]\n mod tests {\n     use super::*;\n \n     #[test]\n     fn greets() {\n-        assert_eq!(greet(\"a\"), \"Hello, a!\");\n+        assert_eq!(hello(\"a\"), \"Hello, a!\");\n     }\n }\n";

    fn make_ctx_raw(captured_diff: &str, raw_emission: &str, baseline: PathBuf) -> EvalContext {
        let reply = TaskReply::new("test-model", "content", captured_diff, false)
            .unwrap()
            .with_raw_emission(raw_emission);
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path().to_path_buf();
        std::mem::forget(workspace);
        let case = TestCase {
            name: "ctx".into(),
            description: "".into(),
            language: "rust".into(),
            prompt: "".into(),
            evaluators: vec![],
            expected_patterns: vec![],
            expected_output: None,
            output_match: None,
            mock_response: MockResponse { content: "".into() },
            difficulty: "L1".into(),
            case_dir: PathBuf::new(),
        };
        EvalContext {
            case,
            workspace: ws,
            baseline,
            reply,
        }
    }

    fn seed_baseline(contents: &str) -> PathBuf {
        let baseline = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(baseline.path().join("src")).unwrap();
        std::fs::write(baseline.path().join("src/lib.rs"), contents).unwrap();
        let p = baseline.path().to_path_buf();
        std::mem::forget(baseline);
        p
    }

    #[test]
    fn diff_applies_judges_raw_emission_over_captured_diff() {
        // Regression for #30B: the captured diff is clean and would apply,
        // but the model's RAW first emission is a header-lying diff that
        // real `git apply --check` rejects. The evaluator must judge the raw
        // emission and FAIL. Under the old behavior (checking the captured
        // diff) this scored a false `ok`.
        let baseline = seed_baseline(SEED_001);
        let ctx = make_ctx_raw(CLEAN_RENAME_DIFF, LYING_DIFF, baseline);
        let r = DiffAppliesEvaluator.evaluate(&ctx);
        assert!(!r.passed, "lying raw emission must fail: {}", r.details);
        assert!(
            r.details.contains("raw emission"),
            "should report it judged the raw emission: {}",
            r.details
        );
    }

    #[test]
    fn diff_applies_strips_fence_on_correct_raw_emission() {
        // A correct diff wrapped in a ```diff fence must still pass — the
        // evaluator peels the fence before `git apply --check`, so a model
        // is not penalized for fencing its (valid) output.
        let baseline = seed_baseline(SEED_001);
        let fenced = format!("```diff\n{CLEAN_RENAME_DIFF}```");
        let ctx = make_ctx_raw("", &fenced, baseline);
        let r = DiffAppliesEvaluator.evaluate(&ctx);
        assert!(r.passed, "fenced correct diff must pass: {}", r.details);
        assert!(r.details.contains("raw emission"));
    }

    #[test]
    fn diff_applies_falls_back_to_captured_for_whole_file_emission() {
        // A whole-file (non-diff) raw emission has no diff to lie about, so
        // the evaluator falls back to the captured diff (previous behavior).
        let baseline = seed_baseline("before\n");
        let captured = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-before\n+after\n";
        let whole_file = "FILE: src/lib.rs\npub fn hello() {}\nEND-FILE\n";
        let ctx = make_ctx_raw(captured, whole_file, baseline);
        let r = DiffAppliesEvaluator.evaluate(&ctx);
        assert!(r.passed, "should fall back to captured diff: {}", r.details);
        assert!(r.details.contains("captured diff"));
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

    // ── python_imports ──────────────────────────────────────────────

    #[test]
    fn module_is_known_matches_dotted_prefixes() {
        let known: BTreeSet<String> = ["newt_agent".to_string(), "os".to_string()]
            .into_iter()
            .collect();
        use newt_core::symbols::module_is_known;
        assert!(module_is_known("newt_agent", &known));
        assert!(module_is_known("newt_agent.core", &known)); // submodule via prefix
        assert!(module_is_known("os.path", &known));
        assert!(!module_is_known("newt_core", &known));
        assert!(!module_is_known("newt_data", &known));
    }

    #[test]
    fn score_python_imports_catches_the_second_nemotron_incident() {
        // Real surface: the umbrella `newt_agent` with submodules, plus stdlib.
        let mut known = newt_core::symbols::python_stdlib_modules();
        known.insert("newt_agent.core".to_string());
        known.insert("newt_agent.data".to_string());

        let files = vec![(
            "examples/ex.py".to_string(),
            "from newt_core import classify\n\
             from newt_data import DataStore\n\
             from newt_agent.core import Router\n\
             import os\n"
                .to_string(),
        )];
        let report = score_python_imports(&known, &files);
        assert_eq!(report.total, 4);
        assert_eq!(report.resolved, 2); // newt_agent.core.Router + os
        let fab: Vec<&str> = report
            .fabricated
            .iter()
            .map(|f| f.module.as_str())
            .collect();
        assert_eq!(fab, vec!["newt_core", "newt_data"]);
    }

    #[test]
    fn python_imports_skips_non_python_cases() {
        let (ws, bl) = empty_dirs();
        let ctx = make_ctx("", vec![], "rust", ws, bl);
        let r = PythonImportsEvaluator.evaluate(&ctx);
        assert!(r.passed);
        assert!(r.details.contains("non-Python"), "details = {}", r.details);
    }
}
