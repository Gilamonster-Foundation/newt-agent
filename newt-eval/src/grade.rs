//! Grade an **arbitrary** post-run workspace against a case — the run-grading
//! core, decoupled from running an agent.
//!
//! Two layers, reported separately:
//!
//! * **Structural** — the case's [`EvalContext`] evaluators (`diff_*`,
//!   `pattern_match`, `rust_compiles`, `tests_pass`) over the diff reconstructed
//!   from the case fixture to the tree. Diagnostics, never the grade.
//! * **Behavioral** — [`grade_behavioral`], the ONE canonical grader (#2317):
//!   the case's withheld `grade_spec.rs`, run against a copy of the tree. Single
//!   mode (`newt-eval run`) and crew mode (`newt-eval grade`, from
//!   `scripts/eval/ratchet.sh`) both call it, so the same tree gets the same
//!   verdict, limits and record whichever mode produced it.

use std::path::Path;
use std::process::Command;

use content_addressable::RawContentId;
use newt_acp_worker::TaskReply;
use serde::{Deserialize, Serialize};

use crate::cases::TestCase;
use crate::evaluators::{
    default_evaluators, evaluator_by_name, CommandRunner, RunOutcome, RunSpec, SubprocessRunner,
};
use crate::runner::{copy_fixture, init_baseline_git};
use crate::scorecard::{CaseScorecard, EvalContext, EvalResult};

/// Wall-clock limit on one `cargo test --test grade_spec`, the same for both
/// modes. Seeds and honest solutions measure well under 5 s; the limit exists
/// for a candidate whose build or tests never finish.
pub const GRADE_SPEC_TIMEOUT_MS: u64 = 600_000;

/// Where the spec is installed in the graded COPY, and where a leak would show
/// up in the candidate tree.
const SPEC_IN_TREE: &str = "tests/grade_spec.rs";

/// Build switches that would make the spec build depend on the host. They
/// are SET to empty rather than removed: an empty env value also overrides a
/// user's cargo config (a global `build.rustc-wrapper = "sccache"` otherwise
/// still applies, and a broken wrapper turns every grade into a FAIL).
const EMPTIED_ENV: &[&str] = &[
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
];

/// Coverage instrumentation a caller can carry in (`cargo llvm-cov` sets
/// these for the test process that runs the grader). Removed.
const REMOVED_ENV: &[&str] = &[
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_ENCODED_RUSTDOCFLAGS",
    "LLVM_PROFILE_FILE",
    "CARGO_LLVM_COV",
    "CARGO_LLVM_COV_TARGET_DIR",
];

/// The canonical behavioral verdict. Serialized as the ratchet row's
/// `behavioral` column: `PASS`, `FAIL`, `UNGRADABLE(reason)`, `ERROR(reason)`.
///
/// FAIL is anything the candidate caused (a compile error in its code or in
/// the spec against its API, a failing or hanging test, a #887 harness
/// override, a decoy test target). ERROR is only for a grader that could not
/// run the checks; UNGRADABLE is for checks that do not exist or ran nothing.
/// Neither ERROR nor UNGRADABLE is ever a pass, and both leave the trial count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum BehavioralVerdict {
    Pass,
    Fail,
    Ungradable(String),
    Error(String),
}

impl std::fmt::Display for BehavioralVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => f.write_str("PASS"),
            Self::Fail => f.write_str("FAIL"),
            Self::Ungradable(why) => write!(f, "UNGRADABLE({why})"),
            Self::Error(why) => write!(f, "ERROR({why})"),
        }
    }
}

impl From<BehavioralVerdict> for String {
    fn from(v: BehavioralVerdict) -> Self {
        v.to_string()
    }
}

impl TryFrom<String> for BehavioralVerdict {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        let inner = |prefix: &str| {
            s.strip_prefix(prefix)
                .and_then(|r| r.strip_suffix(')'))
                .map(str::to_string)
        };
        match s.as_str() {
            "PASS" => Ok(Self::Pass),
            "FAIL" => Ok(Self::Fail),
            _ => inner("UNGRADABLE(")
                .map(Self::Ungradable)
                .or_else(|| inner("ERROR(").map(Self::Error))
                .ok_or_else(|| format!("not a verdict: {s}")),
        }
    }
}

/// What the canonical grader concluded about one tree, and what it ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehavioralGrade {
    pub verdict: BehavioralVerdict,
    /// `grade_spec` when the spec was built and run, `none` when nothing ran.
    pub grader: String,
    /// `RawContentId` of the spec bytes that were graded against; `None` when
    /// the case has no spec.
    pub spec_cid: Option<String>,
    /// Tests the spec binary reported, from its final `test result:` line.
    pub tests_run: u64,
    /// Where a wall-clock kill landed: `none`, `compile` (before the spec
    /// binary started: ERROR) or `tests` (after: FAIL).
    pub timeout: String,
    /// The #887 rule that refused the tree before the spec ran, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_subversion: Option<String>,
    /// One-line reason for humans: the decoy target, the compile error.
    pub detail: String,
}

impl BehavioralGrade {
    fn nothing_ran(
        verdict: BehavioralVerdict,
        spec_cid: Option<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            verdict,
            grader: "none".to_string(),
            spec_cid,
            tests_run: 0,
            timeout: "none".to_string(),
            harness_subversion: None,
            detail: detail.into(),
        }
    }
}

/// What the harness saw BEFORE the candidate ran: the spec's content id, and
/// whether the spec was already visible in the tree (a harness leak).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreRun {
    pub spec_cid: Option<String>,
    pub spec_visible: bool,
}

fn spec_cid(case: &TestCase) -> std::io::Result<Option<(Vec<u8>, String)>> {
    match std::fs::read(case.case_dir.join("grade_spec.rs")) {
        Ok(bytes) => {
            let cid = RawContentId::from_content(&bytes).to_string();
            Ok(Some((bytes, cid)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Record [`PreRun`] for a tree the candidate has not touched yet.
///
/// # Errors
/// An unreadable (but present) spec file.
pub fn pre_run(case: &TestCase, tree: &Path) -> anyhow::Result<PreRun> {
    Ok(PreRun {
        spec_cid: spec_cid(case)?.map(|(_, cid)| cid),
        spec_visible: tree.join(SPEC_IN_TREE).exists(),
    })
}

/// Grade `workspace` against `case`: the structural evaluators plus the
/// canonical behavioral grade.
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
pub fn grade_workspace(
    case: &TestCase,
    workspace: &Path,
    pre: &PreRun,
) -> anyhow::Result<CaseScorecard> {
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
        behavioral: Some(grade_behavioral(case, workspace, pre)),
    })
}

/// The canonical behavioral grade of `tree` (#2317), run for real.
pub fn grade_behavioral(case: &TestCase, tree: &Path, pre: &PreRun) -> BehavioralGrade {
    grade_behavioral_with(&SubprocessRunner, case, tree, pre)
}

/// [`grade_behavioral`] through an injected runner (tests mock grader failure).
pub fn grade_behavioral_with(
    runner: &dyn CommandRunner,
    case: &TestCase,
    tree: &Path,
    pre: &PreRun,
) -> BehavioralGrade {
    let error = |why: &str, cid: Option<String>, detail: String| {
        BehavioralGrade::nothing_ran(BehavioralVerdict::Error(why.to_string()), cid, detail)
    };
    let (spec, cid) = match spec_cid(case) {
        Ok(Some((bytes, cid))) => (Some(bytes), Some(cid)),
        Ok(None) => (None, None),
        Err(e) => return error("io", None, format!("read grade_spec.rs: {e}")),
    };
    if pre.spec_visible {
        return error(
            "spec_leak",
            cid,
            format!("{SPEC_IN_TREE} was in the tree before the run"),
        );
    }
    if cid != pre.spec_cid {
        return error(
            "spec_changed",
            cid.clone(),
            format!("spec content id was {:?} before the run", pre.spec_cid),
        );
    }
    let Some(spec) = spec else {
        return BehavioralGrade::nothing_ran(
            BehavioralVerdict::Ungradable("no_spec".to_string()),
            None,
            "the case has no grade_spec.rs",
        );
    };
    if std::fs::read(tree.join(SPEC_IN_TREE)).is_ok_and(|b| b == spec) {
        return error(
            "spec_leak",
            cid,
            format!("{SPEC_IN_TREE} in the tree is the hidden spec"),
        );
    }
    if let Some(rule) = harness_subversion(tree) {
        return BehavioralGrade {
            harness_subversion: Some(rule.to_string()),
            ..BehavioralGrade::nothing_ran(
                BehavioralVerdict::Fail,
                cid,
                "#887 guard refused the tree",
            )
        };
    }
    let mut grade = run_spec(runner, tree, &spec);
    grade.spec_cid = cid;
    // A build that never started is the host's fault only while the build
    // inputs are the seed's. A candidate that edited its manifest or lockfile
    // (`autotests = false`, a bad dependency) made cargo fail, so it is FAIL.
    if grade.verdict == BehavioralVerdict::Error("build_infra".to_string())
        && BUILD_INPUTS.iter().any(|f| {
            std::fs::read(tree.join(f)).ok() != std::fs::read(case.workspace_fixture().join(f)).ok()
        })
    {
        grade.verdict = BehavioralVerdict::Fail;
    }
    grade
}

/// The files besides the sources that decide whether cargo can build at all.
const BUILD_INPUTS: &[&str] = &["Cargo.toml", "Cargo.lock"];

/// Copy `tree` (without `.git` and `target`), install `spec` in the copy only,
/// and run it: everything [`grade_behavioral_with`] does after its leak and
/// #887 checks. The candidate tree is never written.
pub fn run_spec(runner: &dyn CommandRunner, tree: &Path, spec: &[u8]) -> BehavioralGrade {
    let copy = match tempfile::tempdir()
        .and_then(|d| overlay(tree, d.path(), &[".git", "target"]).map(|()| d))
        .and_then(|d| {
            std::fs::create_dir_all(d.path().join("tests"))?;
            std::fs::write(d.path().join(SPEC_IN_TREE), spec)?;
            Ok(d)
        }) {
        Ok(d) => d,
        Err(e) => {
            return BehavioralGrade::nothing_ran(
                BehavioralVerdict::Error("io".to_string()),
                None,
                format!("copy tree and install spec: {e}"),
            )
        }
    };
    let target = copy.path().join("target").to_string_lossy().into_owned();
    let mut env: Vec<(String, Option<String>)> =
        vec![("CARGO_TARGET_DIR".to_string(), Some(target))];
    env.extend(
        EMPTIED_ENV
            .iter()
            .map(|k| (k.to_string(), Some(String::new()))),
    );
    env.extend(REMOVED_ENV.iter().map(|k| (k.to_string(), None)));
    verdict_from_run(
        &runner.run(&RunSpec {
            argv: ["cargo", "test", "--test", "grade_spec"]
                .map(String::from)
                .to_vec(),
            cwd: copy.path().to_path_buf(),
            timeout_ms: Some(GRADE_SPEC_TIMEOUT_MS),
            env,
        }),
    )
}

/// The #887 harness trust boundary: a tree that can change how cargo builds or
/// which file `--test grade_spec` runs is refused before the spec is installed.
/// No case needs a build script, a cargo config, a toolchain file, or a
/// declared test target.
/// The manifest is parsed rather than grepped, so the inline `test = [...]`
/// form of a `[[test]]` table is caught too.
pub fn harness_subversion(tree: &Path) -> Option<&'static str> {
    let manifest = std::fs::read_to_string(tree.join("Cargo.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok());
    let package_key = |key: &str| {
        manifest
            .as_ref()
            .and_then(|m| m.get("package")?.get(key))
            .is_some()
    };
    if tree.join("build.rs").exists() || package_key("build") {
        Some("build.rs")
    } else if tree.join(".cargo/config.toml").exists() || tree.join(".cargo/config").exists() {
        Some("cargo-config")
    } else if tree.join("rust-toolchain.toml").exists() || tree.join("rust-toolchain").exists() {
        // Selects (and makes rustup fetch and run) a toolchain of the tree's choosing.
        Some("toolchain-file")
    } else if manifest.as_ref().is_some_and(|m| m.contains_key("test")) {
        Some("test-table")
    } else {
        None
    }
}

/// Map one `cargo test --test grade_spec` run to a verdict. Pure.
///
/// Specs that shell out to cargo themselves print extra `Running` and
/// `test result:` lines, so only `Running` lines for the `grade_spec-<hash>`
/// binary count, and the tally is the LAST `test result:` line (the spec
/// binary's own summary, printed after anything its tests echoed).
pub fn verdict_from_run(out: &RunOutcome) -> BehavioralGrade {
    let ran = |verdict, tests_run, timeout: &str, detail: String| BehavioralGrade {
        verdict,
        grader: "grade_spec".to_string(),
        spec_cid: None,
        tests_run,
        timeout: timeout.to_string(),
        harness_subversion: None,
        detail,
    };
    if !out.spawned {
        return BehavioralGrade::nothing_ran(
            BehavioralVerdict::Error("cargo_spawn".to_string()),
            None,
            out.stderr.trim().to_string(),
        );
    }
    let text = format!("{}\n{}", out.stderr, out.stdout);
    let spec_sources: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("Running "))
        .filter_map(|rest| rest.rsplit_once(" ("))
        .filter(|(_, bin)| {
            bin.trim_end_matches(')')
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|name| name.starts_with("grade_spec-"))
        })
        .map(|(source, _)| source)
        .collect();
    let started = !spec_sources.is_empty();
    let summary = text
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix("test result: "));
    let (passed, failed) = summary.map_or((0, 0), |s| (count(s, " passed"), count(s, " failed")));
    if out.timed_out {
        return if started {
            ran(
                BehavioralVerdict::Fail,
                passed + failed,
                "tests",
                "the spec's tests did not finish".into(),
            )
        } else {
            BehavioralGrade {
                timeout: "compile".to_string(),
                ..BehavioralGrade::nothing_ran(
                    BehavioralVerdict::Error("timeout".to_string()),
                    None,
                    "killed before the spec binary started",
                )
            }
        };
    }
    let Some(code) = out.exit_code else {
        return BehavioralGrade::nothing_ran(
            BehavioralVerdict::Error("signal".to_string()),
            None,
            "cargo was killed by a signal",
        );
    };
    if code != 0 {
        let compile_error = text.lines().find(|l| l.contains("could not compile"));
        let first_error = || {
            text.lines()
                .find(|l| l.starts_with("error"))
                .unwrap_or_default()
        };
        return match (compile_error, summary) {
            // Cargo failed before compiling or testing anything: a missing or
            // broken wrapper, a failed download, a missing toolchain. A
            // candidate's compile error always says `could not compile`.
            (None, None) => BehavioralGrade {
                grader: "grade_spec".to_string(),
                ..BehavioralGrade::nothing_ran(
                    BehavioralVerdict::Error("build_infra".to_string()),
                    None,
                    first_error(),
                )
            },
            (Some(line), _) => ran(
                BehavioralVerdict::Fail,
                passed + failed,
                "none",
                line.to_string(),
            ),
            (None, Some(_)) => ran(
                BehavioralVerdict::Fail,
                passed + failed,
                "none",
                format!("{passed} passed, {failed} failed"),
            ),
        };
    }
    if !started
        || spec_sources
            .iter()
            .any(|s| s.replace('\\', "/") != SPEC_IN_TREE)
    {
        return ran(
            BehavioralVerdict::Fail,
            passed,
            "none",
            format!("decoy_target: grade_spec ran {spec_sources:?}"),
        );
    }
    if passed == 0 {
        return ran(
            BehavioralVerdict::Ungradable("no_tests_ran".to_string()),
            0,
            "none",
            "the spec binary ran 0 tests".into(),
        );
    }
    ran(
        BehavioralVerdict::Pass,
        passed,
        "none",
        format!("{passed} passed"),
    )
}

/// `N` from `… N<label>;` in a libtest summary; 0 if absent.
fn count(summary: &str, label: &str) -> u64 {
    summary
        .split(';')
        .find_map(|part| {
            part.trim()
                .trim_start_matches("ok. ")
                .trim_start_matches("FAILED. ")
                .strip_suffix(label)
        })
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// Reconstruct the unified diff `fixture → workspace`: commit the fixture in a
/// throwaway repo, overlay `workspace`'s content (minus its own `.git`), and
/// capture `git diff`. The result is well-formed by construction, so it is the
/// relative, applies-onto-baseline shape the `diff_*` evaluators expect.
fn reconstruct_diff(case: &TestCase, workspace: &Path) -> anyhow::Result<String> {
    let tmp = tempfile::tempdir()?;
    copy_fixture(&case.workspace_fixture(), tmp.path())?;
    init_baseline_git(tmp.path())?;
    overlay(workspace, tmp.path(), &[".git"])?;
    capture_git_diff(tmp.path())
}

/// Recursively copy `src`'s entries into `dst`, overwriting, skipping the
/// top-level names in `skip` (the source's own `.git` would clobber `dst`'s
/// baseline repo).
fn overlay(src: &Path, dst: &Path, skip: &[&str]) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if skip.iter().any(|s| entry.file_name() == *s) {
            continue;
        }
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            overlay(&entry.path(), &to, &[".git"])?;
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

#[cfg(test)]
mod tests {
    //! `verdict_from_run` over REAL `cargo test --test grade_spec` output,
    //! captured from bundled cases with the tree path replaced by `<tree>`
    //! (tests/fixtures/grade_spec_runs). Fully mocked: no process, no fs.
    use super::*;

    /// A captured run, embedded at compile time.
    macro_rules! run {
        ($name:literal, $exit_code:expr) => {
            RunOutcome {
                stdout: include_str!(concat!(
                    "../tests/fixtures/grade_spec_runs/",
                    $name,
                    ".stdout"
                ))
                .into(),
                stderr: include_str!(concat!(
                    "../tests/fixtures/grade_spec_runs/",
                    $name,
                    ".stderr"
                ))
                .into(),
                exit_code: $exit_code,
                timed_out: false,
                spawned: true,
            }
        };
    }

    fn verdict(out: &RunOutcome) -> (String, u64, String) {
        let g = verdict_from_run(out);
        (g.verdict.to_string(), g.tests_run, g.timeout)
    }

    #[test]
    fn an_honest_tree_passes_with_its_test_count() {
        let g = verdict_from_run(&run!("t0-honest-pass", Some(0)));
        assert_eq!(
            (g.verdict, g.tests_run, g.grader.as_str()),
            (BehavioralVerdict::Pass, 1, "grade_spec")
        );
    }

    #[test]
    fn an_unchanged_seed_fails() {
        assert_eq!(
            verdict(&run!("t0-seed-fail", Some(101))),
            ("FAIL".into(), 1, "none".into())
        );
    }

    /// 5 of 18 bundled seeds fail this way: the spec calls the API the task
    /// asks for. Not implementing it is the candidate's FAIL, never an ERROR
    /// that would drop the trial out of n.
    #[test]
    fn a_spec_that_does_not_compile_against_the_candidate_is_a_fail() {
        let g = verdict_from_run(&run!("001-seed-spec-compile-error", Some(101)));
        assert_eq!(g.verdict, BehavioralVerdict::Fail);
        assert!(g.detail.contains("could not compile"), "{g:?}");
    }

    #[test]
    fn a_candidate_lib_that_does_not_compile_is_a_fail() {
        let g = verdict_from_run(&run!("014-round2-3-lib-compile-error", Some(101)));
        assert_eq!(g.verdict, BehavioralVerdict::Fail, "{g:?}");
    }

    /// HALF B of round1-1 (015 gaming corpus): a `[[test]]` table named
    /// `grade_spec` pointed at a decoy. cargo exits 0 with 7 passed and the
    /// real spec never compiles. Exit 0 and N>0 are not enough; the binary
    /// that ran must be the installed spec.
    #[test]
    fn a_decoy_test_target_is_a_fail_despite_exit_zero_and_seven_passed() {
        let out = run!("015-round1-1-decoy", Some(0));
        assert!(
            out.stdout.contains("7 passed"),
            "fixture drift: {}",
            out.stdout
        );
        let g = verdict_from_run(&out);
        assert_eq!(g.verdict, BehavioralVerdict::Fail, "{g:?}");
        assert!(g.detail.starts_with("decoy_target"), "{g:?}");
    }

    /// The spec shells out to cargo, whose `Running unittests` and
    /// `test result:` lines land in the output. Only the spec binary's own,
    /// final summary counts: 8 passed + 10 failed, not a sum over every line.
    #[test]
    fn nested_cargo_output_does_not_confuse_the_tally() {
        assert_eq!(
            verdict(&run!("011-seed-nested-cargo-fail", Some(101))),
            ("FAIL".into(), 18, "none".into())
        );
    }

    /// The vacuous green: exit 0, the real spec binary ran, and it ran nothing.
    #[test]
    fn a_spec_binary_that_ran_zero_tests_is_ungradable_not_a_pass() {
        assert_eq!(
            verdict(&run!("t0-zero-tests", Some(0))),
            ("UNGRADABLE(no_tests_ran)".into(), 0, "none".into())
        );
    }

    /// A cargo that could not build anything (a missing or broken wrapper,
    /// a failed download, a missing toolchain) prints no `could not compile`
    /// and no test summary. That is the host's fault, not the candidate's.
    #[test]
    fn a_cargo_that_could_not_build_anything_is_build_infra() {
        let g = verdict_from_run(&run!("t0-wrapper-missing", Some(101)));
        assert_eq!(
            g.verdict,
            BehavioralVerdict::Error("build_infra".into()),
            "{g:?}"
        );
        assert!(
            g.detail.starts_with("error: could not execute process"),
            "{g:?}"
        );
    }

    /// Twin: the candidate deleting its lib also fails before any test runs,
    /// but cargo says `could not compile`, so it stays the candidate's FAIL.
    #[test]
    fn a_candidate_that_deleted_its_lib_still_fails() {
        let g = verdict_from_run(&run!("t0-deleted-lib", Some(101)));
        assert_eq!(g.verdict, BehavioralVerdict::Fail, "{g:?}");
    }

    #[test]
    fn a_cargo_that_never_started_is_an_error() {
        let out = RunOutcome {
            stderr: "runtime not found: cargo".into(),
            ..RunOutcome::default()
        };
        assert_eq!(verdict(&out).0, "ERROR(cargo_spawn)");
    }

    /// A kill before the spec binary started means the checks never ran: an
    /// ERROR, reported as `timeout=compile` so a slow host is countable.
    #[test]
    fn a_timeout_before_the_spec_binary_started_is_an_error() {
        let mut out = run!("t0-honest-pass", None);
        out.stderr = "   Compiling fix-add v0.1.0 (<tree>)\n".into();
        out.stdout.clear();
        out.timed_out = true;
        assert_eq!(
            verdict(&out),
            ("ERROR(timeout)".into(), 0, "compile".into())
        );
    }

    /// Twin: once the spec binary is running, a hang is the candidate's code.
    #[test]
    fn a_timeout_after_the_spec_binary_started_is_a_fail() {
        let mut out = run!("t0-seed-fail", None);
        out.timed_out = true;
        assert_eq!(verdict(&out).0, "FAIL");
        assert_eq!(verdict(&out).2, "tests");
    }

    #[test]
    fn cargo_killed_by_a_signal_is_an_error() {
        assert_eq!(verdict(&run!("t0-seed-fail", None)).0, "ERROR(signal)");
    }

    #[test]
    fn verdicts_round_trip_through_their_row_spelling() {
        for v in [
            BehavioralVerdict::Pass,
            BehavioralVerdict::Fail,
            BehavioralVerdict::Ungradable("no_spec".into()),
            BehavioralVerdict::Error("timeout".into()),
        ] {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(
                serde_json::from_str::<BehavioralVerdict>(&json).unwrap(),
                v,
                "{json}"
            );
        }
        assert!(serde_json::from_str::<BehavioralVerdict>("\"PASS?gameable\"").is_err());
    }
}
