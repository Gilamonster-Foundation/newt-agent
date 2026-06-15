//! `newt-eval` binary — runs the evaluation suite against the `newt
//! worker` and prints a scorecard.
//!
//! Subcommands:
//!
//! - `list-cases` — enumerate the cases bundled under `cases/`.
//! - `run` — execute one or all cases against a real Ollama in live mode.
//!
//! The mock-mode runner lives in `tests/mock_e2e.rs` so a single
//! `cargo test -p newt-eval` exercises the whole framework in CI.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand, ValueEnum};
use newt_eval::{
    cases, default_evaluators, evaluator_by_name, resolve_worker_bin, run_case, CaseScorecard,
    EvalContext, RunnerConfig, Scorecard, TestCase,
};

#[derive(Parser, Debug)]
#[command(
    name = "newt-eval",
    version,
    about = "End-to-end evaluation framework for the newt worker"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List all bundled test cases.
    ListCases {
        /// Path to the cases directory (defaults to bundled).
        #[arg(long)]
        cases_dir: Option<PathBuf>,
    },

    /// Run cases against the configured backend.
    Run(RunArgs),

    /// Score an arbitrary workspace's Python output with the verify oracle
    /// (#339/#340) — the ground-truth rig's measurement tool. No fixture case
    /// needed: point it at a directory of generated `.py` files plus a
    /// `python_surface.json` declaring the real module surface.
    Score(ScoreArgs),

    /// Extract the authoritative PyO3 import surface (the knowledge-base part,
    /// #74) from a workspace's bindings and print it — the structured fact the
    /// `nemotron` profile injects so the model imports real paths instead of
    /// guessing `newt_core` from the crate name.
    Manifest(ManifestArgs),
}

/// Arguments for the `manifest` subcommand.
#[derive(Args, Debug)]
struct ManifestArgs {
    /// Workspace root to scan for `<crate>/src/pyo3_module.rs` bindings.
    #[arg(long)]
    workspace: PathBuf,
}

/// Arguments for the `score` subcommand.
#[derive(Args, Debug)]
struct ScoreArgs {
    /// Workspace directory to score (the tree the worker/model wrote into).
    #[arg(long)]
    workspace: PathBuf,
    /// Directory holding `python_surface.json` (the real module surface).
    /// Defaults to the workspace itself.
    #[arg(long)]
    surface_dir: Option<PathBuf>,
    /// Emit the verdict as JSON instead of a one-line summary.
    #[arg(long)]
    json: bool,
}

/// Arguments for the `run` subcommand.
#[derive(Args, Debug)]
struct RunArgs {
    /// `mock` requires the test runner — this binary only supports
    /// `live`. The flag is here for symmetry with `cargo test -p
    /// newt-eval --test mock_e2e`.
    #[arg(long, value_enum, default_value_t = Mode::Live)]
    mode: Mode,
    /// Only run the case whose name matches this string (substring match).
    #[arg(long)]
    case: Option<String>,
    /// Override the model name (sent via ACP `set_session_model` AND
    /// `NEWT_DEFAULT_MODEL` env on the spawned worker — latter is
    /// the load-bearing one today).
    #[arg(long)]
    model: Option<String>,
    /// Path to the cases directory (defaults to bundled).
    #[arg(long)]
    cases_dir: Option<PathBuf>,
    /// Path to the `newt` worker binary. When unset the resolver
    /// searches `NEWT_WORKER_BIN`, the sibling of the running binary,
    /// `$CARGO_TARGET_DIR/{release,debug}/newt`, and finally
    /// `target/{release,debug}/newt` under the cwd. Issue #40.
    #[arg(long)]
    worker_bin: Option<PathBuf>,
    /// Spawn the worker with `--coder` so the newt-coder plugin
    /// handles prompts (whole-file emit + diff normalization).
    /// Required for local Ollama coder models that can't fabricate
    /// valid hunk headers (failure mode T0b).
    #[arg(long)]
    coder: bool,
    /// Per-case worker wall-clock budget in milliseconds. A model that
    /// takes longer is scored `dispatch_error` even if it would have
    /// produced correct output given more time — the single binding
    /// constraint on evaluating slower models. Raise it (e.g. 180000)
    /// for slow local models. Default 60000 (backward compatible).
    #[arg(long, env = "NEWT_EVAL_WORKER_TIMEOUT_MS", default_value_t = 60_000)]
    worker_timeout_ms: u64,
    /// Only run cases in these difficulty tiers (comma-separated, e.g.
    /// `L2` or `L2,L3`). Default: all tiers. L1 = saturated single edits;
    /// L2 = multi-step single-domain; L3 = cross-domain.
    #[arg(long, value_delimiter = ',')]
    difficulty: Vec<String>,
    /// Restore the pre-#41 exit-code behavior: process exit is always 0
    /// (when the run completes) or 2 (when at least one case failed),
    /// regardless of whether a `runner` evaluator FAIL appears in the
    /// scorecard. The new behavior is to exit 1 on any `runner` FAIL so
    /// CI / shell scripts can fail-fast on a worker that never produced
    /// a diff.
    #[arg(long)]
    legacy_exit_codes: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Mode {
    /// Live mode talks to a real Ollama via OLLAMA_HOST or default endpoints.
    Live,
    /// Mock mode is implemented via `tests/mock_e2e.rs` — use
    /// `cargo test -p newt-eval --test mock_e2e` to run it.
    Mock,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    match real_main().await {
        Ok(RunOutcomeStatus::AllPassed) => ExitCode::SUCCESS,
        // Issue #41: a `runner` evaluator FAIL means the worker never
        // produced a usable patch — fail-fast with exit 1 so the
        // headline CI check is honest. The legacy exit-2 behavior is
        // still available behind `--legacy-exit-codes`.
        Ok(RunOutcomeStatus::RunnerFailed) => ExitCode::from(1),
        Ok(RunOutcomeStatus::CaseFailed) => ExitCode::from(2),
        Err(e) => {
            eprintln!("newt-eval: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Top-level outcome of one `newt-eval` invocation, mapped to the
/// process exit code by `main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOutcomeStatus {
    /// The whole scorecard is green (or we ran `list-cases`).
    AllPassed,
    /// At least one row has `evaluator == "runner"` and `passed ==
    /// false`. Exit 1 — the worker itself failed to produce output.
    RunnerFailed,
    /// The scorecard has at least one evaluator failure, but it isn't
    /// a `runner` FAIL. Exit 2 — same as the pre-#41 behavior.
    CaseFailed,
}

async fn real_main() -> Result<RunOutcomeStatus> {
    let cli = Cli::parse();
    match cli.command {
        Command::ListCases { cases_dir } => {
            let dir = cases_dir.unwrap_or_else(cases::default_cases_dir);
            let cases = cases::load_all(&dir)?;
            println!("{} bundled cases under {}:", cases.len(), dir.display());
            for c in &cases {
                println!("  {:<28}  {}  {}", c.name, c.language, c.description);
            }
            Ok(RunOutcomeStatus::AllPassed)
        }
        Command::Run(args) => run_command(args).await,
        Command::Score(args) => score_command(args),
        Command::Manifest(args) => manifest_command(args),
    }
}

/// `manifest` — extract and print the authoritative PyO3 import surface (the
/// knowledge-base part, #74). The output is exactly the block the harness injects
/// for the `nemotron` profile.
fn manifest_command(args: ManifestArgs) -> Result<RunOutcomeStatus> {
    let manifest = newt_core::ffi_manifest::FfiManifest::from_workspace(&args.workspace)?;
    if manifest.is_empty() {
        anyhow::bail!(
            "no PyO3 bindings found under {} (looked for <crate>/src/pyo3_module.rs)",
            args.workspace.display()
        );
    }
    print!("{}", manifest.render_block());
    Ok(RunOutcomeStatus::AllPassed)
}

/// `score` — run the Python verify oracle over an arbitrary workspace and print
/// the verdict. The rig's measurement tool; no fixture case or backend needed.
fn score_command(args: ScoreArgs) -> Result<RunOutcomeStatus> {
    let surface_dir = args.surface_dir.unwrap_or_else(|| args.workspace.clone());
    let result = newt_eval::score::score_python_workspace(&args.workspace, &surface_dir)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{}: {} (score {:.2})",
            result.evaluator,
            if result.passed { "PASS" } else { "FAIL" },
            result.score
        );
        println!("  {}", result.details);
    }
    Ok(if result.passed {
        RunOutcomeStatus::AllPassed
    } else {
        RunOutcomeStatus::CaseFailed
    })
}

async fn run_command(args: RunArgs) -> Result<RunOutcomeStatus> {
    let RunArgs {
        mode,
        case: case_filter,
        model,
        cases_dir,
        worker_bin,
        coder,
        worker_timeout_ms,
        difficulty,
        legacy_exit_codes,
    } = args;

    if let Mode::Mock = mode {
        anyhow::bail!(
            "mock mode is implemented by `cargo test -p newt-eval --test mock_e2e` — \
             this binary only runs live mode"
        );
    }

    let dir = cases_dir.unwrap_or_else(cases::default_cases_dir);
    let all = cases::load_all(&dir)?;
    let by_name: Vec<TestCase> = match &case_filter {
        Some(needle) => all
            .into_iter()
            .filter(|c| c.name.contains(needle))
            .collect(),
        None => all,
    };
    let cases = cases::filter_by_difficulty(by_name, &difficulty);
    if cases.is_empty() {
        anyhow::bail!("no cases matched filters (case={case_filter:?}, difficulty={difficulty:?})");
    }

    // Issue #40: resolve the worker binary across CLI flag, env var,
    // argv[0] sibling, $CARGO_TARGET_DIR, and the historical cwd
    // fallbacks. On miss we surface every path that was tried.
    let resolution = resolve_worker_bin(worker_bin);
    if !resolution.found {
        // Surface the error via the scorecard so classify_outcome can apply
        // the correct exit code — including --legacy-exit-codes (exit 2).
        // A bare anyhow::bail! here would exit 1 unconditionally, breaking
        // the legacy contract (fixes #65).
        let msg = format!(
            "newt worker binary not found. Tried:\n{}\nPass --worker-bin <PATH>, \
             set NEWT_WORKER_BIN, or run `cargo build --release --bin newt`.",
            resolution.render_candidates()
        );
        eprintln!("newt-eval: {msg}");
        let mut scorecard = Scorecard::new();
        for case in &cases {
            scorecard.cases.push(CaseScorecard {
                case_name: case.name.clone(),
                results: vec![newt_eval::EvalResult::fail("runner", msg.clone())],
            });
        }
        return Ok(classify_outcome(&scorecard, legacy_exit_codes));
    }

    let mut config = RunnerConfig::new(&resolution.path);
    if let Some(m) = model {
        config = config.with_model(m);
    }
    if coder {
        config = config.with_coder_mode(true);
    }
    config = config.with_timeout(Duration::from_millis(worker_timeout_ms));

    let mut scorecard = Scorecard::new();
    for case in &cases {
        let outcome = match run_case(case, &config).await {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[{}] runner error: {e:#}", case.name);
                let cs = CaseScorecard {
                    case_name: case.name.clone(),
                    results: vec![newt_eval::EvalResult::fail("runner", format!("{e:#}"))],
                };
                scorecard.push(cs);
                continue;
            }
        };

        let ctx = EvalContext {
            case: outcome.case.clone(),
            workspace: outcome.workspace.clone(),
            baseline: outcome.baseline.clone(),
            reply: outcome.reply.clone(),
        };

        let results = run_evaluators(&ctx)?;
        scorecard.push(CaseScorecard {
            case_name: case.name.clone(),
            results,
        });
    }

    print!("{}", scorecard.render_table());
    Ok(classify_outcome(&scorecard, legacy_exit_codes))
}

/// Map the finished scorecard to the exit-code-carrying status enum.
///
/// Issue #41: when any row reports `evaluator == "runner"` and the row
/// failed, the worker itself never produced usable output — the
/// process should exit 1 so CI / `set -e` shell scripts fail-fast.
/// `--legacy-exit-codes` opts back into the previous behavior where
/// a runner FAIL was indistinguishable from any other case failure
/// (exit 2).
fn classify_outcome(scorecard: &Scorecard, legacy_exit_codes: bool) -> RunOutcomeStatus {
    if scorecard.all_passed() {
        return RunOutcomeStatus::AllPassed;
    }
    if !legacy_exit_codes && scorecard_has_runner_failure(scorecard) {
        return RunOutcomeStatus::RunnerFailed;
    }
    RunOutcomeStatus::CaseFailed
}

/// True iff any row in the scorecard has `evaluator == "runner"` and
/// `passed == false`. The "runner" evaluator name is the sentinel the
/// run loop above stamps onto a row when `run_case` itself errors
/// (worker spawn failed, ACP handshake failed, worker timed out).
fn scorecard_has_runner_failure(scorecard: &Scorecard) -> bool {
    scorecard
        .cases
        .iter()
        .flat_map(|c| c.results.iter())
        .any(|r| r.evaluator == "runner" && !r.passed)
}

fn run_evaluators(ctx: &EvalContext) -> Result<Vec<newt_eval::EvalResult>> {
    if ctx.case.evaluators.is_empty() {
        // Fall back to the full default set so a misconfigured case
        // doesn't silently produce a vacuous "pass".
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

#[cfg(test)]
mod tests {
    use super::*;
    use newt_eval::EvalResult;

    fn sc_with(rows: Vec<(&str, &str, bool)>) -> Scorecard {
        let mut sc = Scorecard::new();
        for (case_name, evaluator, passed) in rows {
            let r = if passed {
                EvalResult::pass(evaluator, "ok")
            } else {
                EvalResult::fail(evaluator, "nope")
            };
            sc.push(CaseScorecard {
                case_name: case_name.into(),
                results: vec![r],
            });
        }
        sc
    }

    #[test]
    fn classify_outcome_all_passed() {
        let sc = sc_with(vec![("a", "diff_applies", true)]);
        assert_eq!(classify_outcome(&sc, false), RunOutcomeStatus::AllPassed);
    }

    #[test]
    fn classify_outcome_runner_failure_exits_one() {
        // Issue #41: a "runner" FAIL is the headline signal — exit 1.
        let sc = sc_with(vec![("a", "runner", false)]);
        assert_eq!(classify_outcome(&sc, false), RunOutcomeStatus::RunnerFailed);
    }

    #[test]
    fn classify_outcome_non_runner_failure_exits_two() {
        // A non-"runner" evaluator failure stays at exit 2 (pre-#41
        // behavior, unchanged).
        let sc = sc_with(vec![("a", "diff_applies", false)]);
        assert_eq!(classify_outcome(&sc, false), RunOutcomeStatus::CaseFailed);
    }

    #[test]
    fn classify_outcome_legacy_flag_collapses_runner_to_case_failed() {
        // `--legacy-exit-codes` opts back into the old behavior where
        // a runner FAIL was indistinguishable from any other failure
        // (exit 2).
        let sc = sc_with(vec![("a", "runner", false)]);
        assert_eq!(classify_outcome(&sc, true), RunOutcomeStatus::CaseFailed);
    }

    #[test]
    fn scorecard_has_runner_failure_detects_buried_fail() {
        // The detector should find a runner FAIL even when it's not
        // the only row in the scorecard.
        let mut sc = Scorecard::new();
        sc.push(CaseScorecard {
            case_name: "a".into(),
            results: vec![EvalResult::pass("diff_applies", "ok")],
        });
        sc.push(CaseScorecard {
            case_name: "b".into(),
            results: vec![
                EvalResult::pass("diff_applies", "ok"),
                EvalResult::fail("runner", "spawn failed"),
            ],
        });
        assert!(scorecard_has_runner_failure(&sc));
    }

    #[test]
    fn scorecard_has_runner_failure_ignores_runner_passes() {
        // A `runner` row that *passed* must not trip the detector.
        let mut sc = Scorecard::new();
        sc.push(CaseScorecard {
            case_name: "a".into(),
            results: vec![EvalResult::pass("runner", "all good")],
        });
        assert!(!scorecard_has_runner_failure(&sc));
    }
}
