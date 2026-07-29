//! `newt bench` — Terminal-Bench matrix orchestration (#1490).
//!
//! newt owns the **roster, sequencing, scoreboard, and gates**; a pluggable
//! [`executor`] backs the actual task execution (harbor today, a native runner
//! later). The roster is a manifest ([`config::MatrixConfig`]) — a new model is
//! a config entry, never code.
//!
//! Hard rule: **one model at a time.** The model loop is strictly sequential —
//! dgx1's shared unified-memory pool holds a single model, and concurrent loads
//! fail. Suite-level concurrency (`n_concurrent_trials`) applies only within one
//! already-loaded model.

pub mod config;
pub mod executor;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;

use config::MatrixConfig;
use executor::{BenchExecutor, HarborExecutor, RunSpec};

/// `newt bench <cmd>` — matrix orchestration.
#[derive(Subcommand, Debug)]
pub enum BenchCmd {
    /// Run the matrix — one model at a time, both requested lanes — via the
    /// configured executor.
    Run {
        /// The matrix manifest (roster + suite + harness).
        #[arg(long, value_name = "FILE", default_value = "~/.newt/bench/matrix.toml")]
        matrix: String,
        /// Restrict to a single model id (default: the whole roster).
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
        /// Restrict to one OCAP lane, or `both` (default: the configured lanes).
        #[arg(long, value_parser = ["off", "on", "both"])]
        lane: Option<String>,
        /// Root under which each run's jobs dir is created.
        #[arg(long, value_name = "DIR", default_value = "/var/tmp")]
        jobs_root: String,
        /// The harbor adapter dir put on PYTHONPATH.
        #[arg(long, value_name = "DIR", default_value = "scripts/eval/harbor")]
        pythonpath: String,
    },
}

/// Dispatch a `newt bench` subcommand.
pub async fn run(cmd: BenchCmd) -> Result<()> {
    match cmd {
        BenchCmd::Run {
            matrix,
            model,
            lane,
            jobs_root,
            pythonpath,
        } => {
            let path = expand_tilde(&matrix);
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading matrix {}", path.display()))?;
            let cfg = MatrixConfig::parse(&text)?;
            let executor = HarborExecutor { pythonpath };
            let outcomes = run_matrix(
                &cfg,
                lane.as_deref(),
                model.as_deref(),
                &jobs_root,
                &executor,
            )
            .await?;
            print_summary(&cfg, &outcomes);
            Ok(())
        }
    }
}

/// Expand a leading `~/` to the home dir; leave everything else untouched.
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Print what ran/skipped plus, for each completed run, the ready-to-paste
/// `bench_scoreboard.py ingest` command (model id + lane + suite/window from the
/// manifest) — so results flow into the scoreboard as one copy step.
fn print_summary(cfg: &MatrixConfig, outcomes: &[Outcome]) {
    let ran = outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Ran { .. }))
        .count();
    let skipped = outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::SkippedPreflight { .. }))
        .count();
    let failed = outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Failed { .. }))
        .count();
    println!("\nbench matrix complete: {ran} run(s), {skipped} skipped, {failed} failed\n");
    for o in outcomes {
        match o {
            Outcome::SkippedPreflight { model } => {
                println!("  SKIP  {model} — endpoint could not load it");
            }
            Outcome::Failed { model, lane, error } => {
                println!("  FAIL  {model} [{lane}] — {error}");
            }
            Outcome::Ran {
                model,
                lane,
                run_dir,
            } => {
                let entry = cfg.models.iter().find(|m| &m.name == model);
                // Placeholders (not empty strings) when a field is unset, so the
                // printed command is obviously fill-in-the-blank, never silently
                // missing a flag value.
                let family = entry
                    .and_then(|m| m.family.clone())
                    .unwrap_or_else(|| "<family>".into());
                let window = entry
                    .and_then(|m| m.effective_window(&cfg.harness))
                    .map(|w| w.to_string())
                    .unwrap_or_else(|| "<window>".into());
                println!("  RAN   {model} [{lane}] → {run_dir}");
                println!(
                    "        ingest: bench_scoreboard.py ingest {run_dir} \
                     --model {model} --family {family} --ocap {lane} \
                     --suite {} --window {window} --version <V> --date <YYYY-MM-DD>",
                    cfg.suite.name,
                );
            }
        }
    }
}

/// What happened to one roster entry on one lane: it ran, it was skipped because
/// its endpoint could not load it, or its run failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ran {
        model: String,
        lane: String,
        run_dir: String,
    },
    SkippedPreflight {
        model: String,
    },
    /// A preflight or run errored. Recorded (not `?`-propagated) so one model's
    /// hiccup never aborts a multi-hour matrix — the remaining models still run.
    Failed {
        model: String,
        lane: String,
        error: String,
    },
}

/// Orchestrate the matrix: **one model at a time**, both requested lanes.
///
/// This loop is the enforcement point of the hard rule. The MODEL loop is a
/// plain sequential `for` with `.await` between iterations — there is no join,
/// no fan-out — so no two models are ever loaded at once (dgx1's shared
/// unified-memory pool holds one; concurrent loads 500). A model is preflighted
/// ONCE (it stays resident across its own back-to-back lanes); a preflight miss
/// skips the model without touching the endpoint further.
///
/// Returns the per-run outcomes for the caller to ingest into the scoreboard.
/// Nothing is ingested here, and each `run` leaves a durable run dir on disk, so
/// a crash mid-matrix loses no completed work — re-running ingests what's there.
///
/// A single model/lane failure is RECORDED as [`Outcome::Failed`] and the loop
/// moves on — one transient container hiccup or endpoint 500 must not throw away
/// the hours the other models will still run. `run_matrix` only returns `Err` for
/// a setup error (an unknown `--model` filter) before any model has run.
pub async fn run_matrix(
    cfg: &MatrixConfig,
    lane_override: Option<&str>,
    model_filter: Option<&str>,
    jobs_root: &str,
    executor: &dyn BenchExecutor,
) -> Result<Vec<Outcome>> {
    let models = cfg.select_models(model_filter)?;
    let lanes = cfg.resolve_lanes(lane_override);
    let mut outcomes = Vec::new();
    for model in models {
        // one model at a time — nothing below fans out.
        match executor.preflight(model).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(model = %model.name, "skip — endpoint can't load it now");
                outcomes.push(Outcome::SkippedPreflight {
                    model: model.name.clone(),
                });
                continue;
            }
            Err(e) => {
                tracing::error!(model = %model.name, error = %e, "preflight errored — skipping model");
                outcomes.push(Outcome::Failed {
                    model: model.name.clone(),
                    lane: "preflight".into(),
                    error: e.to_string(),
                });
                continue;
            }
        }
        for lane in &lanes {
            tracing::info!(model = %model.name, lane = %lane, "running suite");
            let spec = RunSpec {
                model,
                lane,
                suite: &cfg.suite,
                harness: &cfg.harness,
                jobs_root,
            };
            match executor.run(&spec).await {
                Ok(out) => outcomes.push(Outcome::Ran {
                    model: model.name.clone(),
                    lane: lane.clone(),
                    run_dir: out.run_dir,
                }),
                Err(e) => {
                    tracing::error!(model = %model.name, lane = %lane, error = %e, "run failed — continuing matrix");
                    outcomes.push(Outcome::Failed {
                        model: model.name.clone(),
                        lane: lane.clone(),
                        error: e.to_string(),
                    });
                }
            }
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::executor::{BenchExecutor, RunOutcome, RunSpec};
    use super::*;
    use config::ModelEntry;
    use std::sync::Mutex;

    /// Records the exact order of preflight/run calls so a test can prove the
    /// sequential invariant. `deny` names models whose preflight returns false;
    /// `fail_run` names models whose `run` returns Err (a simulated hiccup).
    #[derive(Default)]
    struct FakeExec {
        calls: Mutex<Vec<String>>,
        deny: Vec<&'static str>,
        fail_run: Vec<&'static str>,
    }

    #[async_trait::async_trait]
    impl BenchExecutor for FakeExec {
        async fn preflight(&self, model: &ModelEntry) -> Result<bool> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("preflight:{}", model.name));
            Ok(!self.deny.contains(&model.name.as_str()))
        }
        async fn run(&self, spec: &RunSpec<'_>) -> Result<RunOutcome> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("run:{}:{}", spec.model.name, spec.lane));
            if self.fail_run.contains(&spec.model.name.as_str()) {
                anyhow::bail!("simulated run failure for {}", spec.model.name);
            }
            Ok(RunOutcome {
                run_dir: spec.jobs_dir(),
            })
        }
    }

    const TWO: &str = r#"
        [suite]
        name = "tb-30"
        dataset = "/data/tb"
        tasks = ["t1"]
        [harness]
        binary = "/opt/newt"
        lanes = ["off", "on"]
        [[model]]
        name = "A"
        [[model]]
        name = "B"
    "#;

    #[tokio::test]
    async fn runs_one_model_at_a_time_both_lanes_in_order() {
        let cfg = MatrixConfig::parse(TWO).unwrap();
        let ex = FakeExec::default();
        let out = run_matrix(&cfg, None, None, "/j", &ex).await.unwrap();
        // The load-bearing assertion: B is never touched until ALL of A is done.
        assert_eq!(
            *ex.calls.lock().unwrap(),
            vec![
                "preflight:A",
                "run:A:off",
                "run:A:on",
                "preflight:B",
                "run:B:off",
                "run:B:on",
            ]
        );
        assert_eq!(out.len(), 4); // 2 models × 2 lanes
    }

    #[tokio::test]
    async fn preflight_miss_skips_the_model_without_running_it() {
        let cfg = MatrixConfig::parse(TWO).unwrap();
        let ex = FakeExec {
            deny: vec!["A"],
            ..Default::default()
        };
        let out = run_matrix(&cfg, None, None, "/j", &ex).await.unwrap();
        // A is preflighted, denied, and NEVER run; B proceeds.
        assert_eq!(
            *ex.calls.lock().unwrap(),
            vec!["preflight:A", "preflight:B", "run:B:off", "run:B:on"]
        );
        assert_eq!(out[0], Outcome::SkippedPreflight { model: "A".into() });
        assert!(matches!(&out[1], Outcome::Ran { model, .. } if model == "B"));
    }

    #[tokio::test]
    async fn lane_override_restricts_to_one_lane() {
        let cfg = MatrixConfig::parse(TWO).unwrap();
        let ex = FakeExec::default();
        run_matrix(&cfg, Some("on"), Some("A"), "/j", &ex)
            .await
            .unwrap();
        assert_eq!(*ex.calls.lock().unwrap(), vec!["preflight:A", "run:A:on"]);
    }

    #[tokio::test]
    async fn one_model_failure_does_not_abort_the_matrix() {
        let cfg = MatrixConfig::parse(TWO).unwrap();
        // A's runs both fail; B must still run to completion.
        let ex = FakeExec {
            fail_run: vec!["A"],
            ..Default::default()
        };
        let out = run_matrix(&cfg, None, None, "/j", &ex).await.unwrap();
        // A attempted both lanes (each recorded Failed), B ran both.
        assert_eq!(
            *ex.calls.lock().unwrap(),
            vec![
                "preflight:A",
                "run:A:off",
                "run:A:on",
                "preflight:B",
                "run:B:off",
                "run:B:on",
            ]
        );
        let failed: Vec<_> = out
            .iter()
            .filter(|o| matches!(o, Outcome::Failed { .. }))
            .collect();
        let ran: Vec<_> = out
            .iter()
            .filter(|o| matches!(o, Outcome::Ran { .. }))
            .collect();
        assert_eq!(failed.len(), 2, "A's two lanes each recorded Failed");
        assert_eq!(ran.len(), 2, "B's two lanes still ran");
    }
}
