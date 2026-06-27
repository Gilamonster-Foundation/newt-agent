//! Grade an **arbitrary** post-run workspace against a case's structural
//! evaluators — the run-grading core, decoupled from running an agent.
//!
//! `run` (the ACP path) grades what a worker *emitted*; `grade` grades a tree
//! that already exists — a `crew/*` worktree, a hand-edited dir, any
//! post-run output — by reconstructing the diff as the change from the case
//! fixture to that tree, then applying the same [`EvalContext`]-based
//! evaluators (`diff_*`, `pattern_match`, `rust_compiles`, `tests_pass`).
//!
//! This is what the ratchet matrix (`scripts/eval/ratchet.sh`) calls to add the
//! *structural* scorecard for the plan/crew worktrees, alongside the behavioral
//! `grade-run.sh` oracle.

use std::path::Path;
use std::process::Command;

use newt_acp_worker::TaskReply;

use crate::cases::TestCase;
use crate::evaluators::{default_evaluators, evaluator_by_name};
use crate::runner::{copy_fixture, init_baseline_git};
use crate::scorecard::{CaseScorecard, EvalContext, EvalResult};

/// Grade `workspace` against `case`'s evaluators.
///
/// The diff is reconstructed as the change from the case fixture to
/// `workspace`, so `diff_nonempty`/`diff_applies`/`pattern_match` have an
/// artifact to judge and `rust_compiles`/`tests_pass` run against the tree.
/// `EvalContext.baseline` is the case fixture (the diff applies onto it) and
/// `EvalContext.workspace` is the graded tree. When the case names no
/// evaluators, the full default set runs (matching the `run` path, so a
/// misconfigured case never produces a vacuous "pass").
///
/// # Errors
/// Propagates fixture-copy, git, and reply-construction failures.
pub fn grade_workspace(case: &TestCase, workspace: &Path) -> anyhow::Result<CaseScorecard> {
    let diff = reconstruct_diff(case, workspace)?;
    let reply = TaskReply::new("grade", "", &diff, diff.trim().is_empty())?;
    let ctx = EvalContext {
        case: case.clone(),
        workspace: workspace.to_path_buf(),
        baseline: case.workspace_fixture(),
        reply,
    };
    Ok(CaseScorecard {
        case_name: case.name.clone(),
        results: evaluate(&ctx)?,
    })
}

/// Reconstruct the unified diff `fixture → workspace`: commit the fixture in a
/// throwaway repo, overlay `workspace`'s content (minus its own `.git`), and
/// capture `git diff`. The result is well-formed by construction, so it is the
/// relative, applies-onto-baseline shape the `diff_*` evaluators expect.
fn reconstruct_diff(case: &TestCase, workspace: &Path) -> anyhow::Result<String> {
    let tmp = tempfile::tempdir()?;
    copy_fixture(&case.workspace_fixture(), tmp.path())?;
    init_baseline_git(tmp.path())?;
    overlay(workspace, tmp.path())?;
    capture_git_diff(tmp.path())
}

/// Recursively copy `src`'s entries into `dst`, overwriting, but never the
/// source's own `.git` (which would clobber `dst`'s baseline repo).
fn overlay(src: &Path, dst: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            overlay(&entry.path(), &to)?;
        } else {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// `git add -A` + `git diff --cached HEAD` in `dir`. Strips inherited git env
/// so the capture targets `dir`, not whichever repo a caller's `GIT_DIR`
/// points at (e.g. when invoked from inside a git hook).
fn capture_git_diff(dir: &Path) -> anyhow::Result<String> {
    let git = |args: &[&str]| -> anyhow::Result<std::process::Output> {
        Ok(Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_PREFIX")
            .output()?)
    };
    let add = git(&["add", "-A"])?;
    if !add.status.success() {
        anyhow::bail!("git add failed: {}", String::from_utf8_lossy(&add.stderr));
    }
    let diff = git(&["diff", "--cached", "--no-color", "HEAD"])?;
    if !diff.status.success() {
        anyhow::bail!("git diff failed: {}", String::from_utf8_lossy(&diff.stderr));
    }
    Ok(String::from_utf8_lossy(&diff.stdout).to_string())
}

/// Run the case's named evaluators, or the full default set if it names none.
fn evaluate(ctx: &EvalContext) -> anyhow::Result<Vec<EvalResult>> {
    if ctx.case.evaluators.is_empty() {
        return Ok(default_evaluators()
            .iter()
            .map(|ev| ev.evaluate(ctx))
            .collect());
    }
    let mut results = Vec::with_capacity(ctx.case.evaluators.len());
    for name in &ctx.case.evaluators {
        let ev = evaluator_by_name(name).ok_or_else(|| {
            anyhow::anyhow!("unknown evaluator '{name}' in case {}", ctx.case.name)
        })?;
        results.push(ev.evaluate(ctx));
    }
    Ok(results)
}

// End-to-end coverage (real fs + git: the diff reconstruction) lives in the
// expensive tier — `newt-eval/tests/grade.rs` — per the repo's fully-mocked
// unit-tier rule.
