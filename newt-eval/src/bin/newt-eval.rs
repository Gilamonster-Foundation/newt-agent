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
    cases, default_evaluators, evaluator_by_name, run_case, CaseScorecard, EvalContext,
    RunnerConfig, Scorecard, TestCase,
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
    /// Path to the `newt` binary (defaults to `target/<profile>/newt`).
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
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(e) => {
            eprintln!("newt-eval: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Returns `Ok(true)` if the run passed every gate, `Ok(false)` if it
/// completed but some case failed, `Err` for a hard error.
async fn real_main() -> Result<bool> {
    let cli = Cli::parse();
    match cli.command {
        Command::ListCases { cases_dir } => {
            let dir = cases_dir.unwrap_or_else(cases::default_cases_dir);
            let cases = cases::load_all(&dir)?;
            println!("{} bundled cases under {}:", cases.len(), dir.display());
            for c in &cases {
                println!("  {:<28}  {}  {}", c.name, c.language, c.description);
            }
            Ok(true)
        }
        Command::Run(args) => run_command(args).await,
    }
}

async fn run_command(args: RunArgs) -> Result<bool> {
    let RunArgs {
        mode,
        case: case_filter,
        model,
        cases_dir,
        worker_bin,
        coder,
        worker_timeout_ms,
        difficulty,
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

    let worker = worker_bin.unwrap_or_else(default_worker_bin);
    let mut config = RunnerConfig::new(&worker);
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
    Ok(scorecard.all_passed())
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

/// Best-effort guess for `target/<profile>/newt` based on common cargo
/// layouts. Callers can always pass `--worker-bin` explicitly.
fn default_worker_bin() -> PathBuf {
    let candidates = [
        "target/release/newt",
        "target/debug/newt",
        "../target/release/newt",
        "../target/debug/newt",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from("target/release/newt")
}
