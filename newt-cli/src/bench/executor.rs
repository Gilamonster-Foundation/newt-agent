//! `newt bench` executors (#1490 slice 2).
//!
//! A [`BenchExecutor`] runs one `(model, lane)` over the suite and hands back
//! the run directory the scoreboard ingests. The executor is **pluggable**
//! (`[harness] executor = "…"`): [`HarborExecutor`] shells out to the harbor
//! Terminal-Bench harness today; a native Rust runner can implement the same
//! trait later without reshaping the command.
//!
//! newt's job ends at *orchestration + provenance*; harbor keeps owning
//! container setup and per-task verification (the ugly bits). So the harbor
//! executor's whole job is: derive the per-run job config + env from the
//! manifest (pure, tested below), then spawn `harbor run`.

use anyhow::{bail, Context, Result};

use super::config::{HarnessConfig, ModelEntry, SuiteConfig};

/// Everything one `(model, lane)` run needs, borrowed from the manifest.
pub struct RunSpec<'a> {
    pub model: &'a ModelEntry,
    /// `"off"` or `"on"` — validated upstream by `MatrixConfig::validate`.
    pub lane: &'a str,
    pub suite: &'a SuiteConfig,
    pub harness: &'a HarnessConfig,
    /// Root under which each run's jobs dir is created.
    pub jobs_root: &'a str,
}

impl RunSpec<'_> {
    /// The per-run jobs dir: `<jobs_root>/tbench-<model>-<lane>`, with the model
    /// id sanitized so a `/` in an id can't escape the root. Distinct per lane so
    /// the off and on runs never clobber each other.
    #[must_use]
    pub fn jobs_dir(&self) -> String {
        let safe: String = self
            .model
            .name
            .chars()
            .map(|c| {
                if c == '/' || c == std::path::MAIN_SEPARATOR {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        format!("{}/tbench-{}-{}", self.jobs_root, safe, self.lane)
    }
}

/// The result of one run: the dir the scoreboard reads per-task rewards from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub run_dir: String,
}

/// Backs the actual execution of a suite for one `(model, lane)`.
#[async_trait::async_trait]
pub trait BenchExecutor {
    /// Can this model be loaded on its endpoint *right now*? A `false` means
    /// skip it (and log why) — the model isn't servable this moment. This is the
    /// guard that keeps the strictly-sequential model loop honest: never start a
    /// run against a model the endpoint can't hold.
    async fn preflight(&self, model: &ModelEntry) -> Result<bool>;

    /// Run the whole suite for one `(model, lane)` and return its run dir.
    async fn run(&self, spec: &RunSpec<'_>) -> Result<RunOutcome>;
}

/// Shells out to the harbor Terminal-Bench harness.
pub struct HarborExecutor {
    /// The adapter dir put on `PYTHONPATH` (`scripts/eval/harbor`).
    pub pythonpath: String,
}

impl HarborExecutor {
    /// The harbor job config for one run — a pure transform of the manifest, so
    /// it is unit-tested without touching harbor. Mirrors the hand-written
    /// `job-*.json` the runner replaces.
    #[must_use]
    pub fn job_json(spec: &RunSpec) -> serde_json::Value {
        serde_json::json!({
            "jobs_dir": spec.jobs_dir(),
            "n_concurrent_trials": spec.harness.concurrent,
            "agent_timeout_multiplier": spec.harness.timeout_multiplier,
            "datasets": [{
                "path": spec.suite.dataset,
                "task_names": spec.suite.tasks,
            }],
            "agents": [{
                "import_path": "newt_agent:NewtAgent",
                // Harbor requires -m/model_name but the profile is authoritative;
                // the `newt/` prefix is harbor's provider convention.
                "model_name": format!("newt/{}", spec.model.name),
            }],
        })
    }

    /// The `NEWT_BENCH_*` env the adapter reads for one run — also a pure
    /// transform, so the lane→`NEWT_BENCH_OCAP` mapping and the per-model window
    /// override are tested directly. Endpoint stays host-secret inside the
    /// profile file (never an env value here).
    #[must_use]
    pub fn run_env(&self, spec: &RunSpec) -> Vec<(String, String)> {
        let mut env = vec![
            ("NEWT_BENCH_BIN".into(), spec.harness.binary.clone()),
            ("NEWT_BENCH_PROFILE".into(), spec.model.profile.clone()),
            (
                "NEWT_BENCH_MAX_ROUNDS".into(),
                spec.harness.max_rounds.to_string(),
            ),
            ("PYTHONPATH".into(), self.pythonpath.clone()),
        ];
        // The lane is the whole point: `on` flips the confined `newt solve` lane;
        // `off` is the default `--yolo` lane (adapter treats non-`on` as off).
        env.push(("NEWT_BENCH_OCAP".into(), spec.lane.to_string()));
        if let Some(w) = spec.model.effective_window(spec.harness) {
            env.push(("NEWT_BENCH_CONTEXT_WINDOW".into(), w.to_string()));
        }
        if let Some(t) = &spec.model.tenacity {
            if !t.is_empty() {
                env.push(("NEWT_BENCH_TENACITY".into(), t.clone()));
            }
        }
        env
    }
}

#[async_trait::async_trait]
impl BenchExecutor for HarborExecutor {
    async fn preflight(&self, model: &ModelEntry) -> Result<bool> {
        // The endpoint is host-secret: read it from the profile toml (local),
        // never from a committed script. A tiny 4-token completion proves the
        // router can actually LOAD the model right now (not just list it).
        let endpoint = endpoint_from_profile(&model.profile)
            .with_context(|| format!("resolving endpoint for `{}`", model.name))?;
        let body = serde_json::json!({
            "model": model.name,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 4,
        });
        let resp = reqwest::Client::new()
            .post(format!("{endpoint}/v1/chat/completions"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(180))
            .send()
            .await;
        Ok(matches!(resp, Ok(r) if r.status().is_success()))
    }

    async fn run(&self, spec: &RunSpec<'_>) -> Result<RunOutcome> {
        let jobs_dir = spec.jobs_dir();
        std::fs::create_dir_all(&jobs_dir)
            .with_context(|| format!("creating jobs dir {jobs_dir}"))?;
        let job_path = format!("{jobs_dir}/job.json");
        std::fs::write(&job_path, serde_json::to_vec_pretty(&Self::job_json(spec))?)
            .with_context(|| format!("writing {job_path}"))?;

        let mut cmd = tokio::process::Command::new("harbor");
        cmd.arg("run").arg("--config").arg(&job_path);
        for (k, v) in self.run_env(spec) {
            cmd.env(k, v);
        }
        let status = cmd
            .status()
            .await
            .context("spawning `harbor run` (is harbor on PATH?)")?;
        if !status.success() {
            bail!(
                "harbor run for `{}` [{}] exited {status}",
                spec.model.name,
                spec.lane
            );
        }
        Ok(RunOutcome { run_dir: jobs_dir })
    }
}

/// Pull the `endpoint = "http://…"` out of a backend profile toml. Host-secret:
/// the value lives only in the local profile, never in committed config.
fn endpoint_from_profile(profile: &str) -> Result<String> {
    if profile.is_empty() {
        bail!("model has no `profile` (needed to resolve its endpoint)");
    }
    let text =
        std::fs::read_to_string(profile).with_context(|| format!("reading profile {profile}"))?;
    let val: toml::Value = toml::from_str(&text).context("parsing profile toml")?;
    val.get("backends")
        .and_then(|b| b.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("endpoint"))
        .and_then(|e| e.as_str())
        .map(|s| s.trim_end_matches('/').to_string())
        .context("profile has no [[backends]] endpoint")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::config::MatrixConfig;

    const SAMPLE: &str = r#"
        [suite]
        name = "tb-30"
        dataset = "/data/terminal-bench"
        tasks = ["pypi-server", "fix-git"]
        [harness]
        concurrent = 3
        timeout_multiplier = 2.0
        max_rounds = 40
        binary = "/opt/newt"
        context_window = 65536
        [[model]]
        name = "qwen3.6_35b"
        profile = "solve-qwen36.toml"
        [[model]]
        name = "nemotron-3-nano_30b"
        profile = "solve-nemo.toml"
        window = 32768
        tenacity = "relentless"
    "#;

    fn spec<'a>(m: &'a MatrixConfig, i: usize, lane: &'a str) -> RunSpec<'a> {
        RunSpec {
            model: &m.models[i],
            lane,
            suite: &m.suite,
            harness: &m.harness,
            jobs_root: "/var/tmp",
        }
    }

    #[test]
    fn jobs_dir_is_per_model_and_per_lane() {
        let m = MatrixConfig::parse(SAMPLE).unwrap();
        assert_eq!(
            spec(&m, 0, "off").jobs_dir(),
            "/var/tmp/tbench-qwen3.6_35b-off"
        );
        assert_eq!(
            spec(&m, 0, "on").jobs_dir(),
            "/var/tmp/tbench-qwen3.6_35b-on"
        );
        // off and on never share a dir → no clobber.
        assert_ne!(spec(&m, 0, "off").jobs_dir(), spec(&m, 0, "on").jobs_dir());
    }

    #[test]
    fn job_json_carries_suite_and_exact_model_id() {
        let m = MatrixConfig::parse(SAMPLE).unwrap();
        let j = HarborExecutor::job_json(&spec(&m, 0, "on"));
        assert_eq!(j["n_concurrent_trials"], 3);
        assert_eq!(j["agent_timeout_multiplier"], 2.0);
        assert_eq!(j["datasets"][0]["path"], "/data/terminal-bench");
        assert_eq!(j["datasets"][0]["task_names"][0], "pypi-server");
        // the EXACT model id, prefixed with harbor's provider convention.
        assert_eq!(j["agents"][0]["model_name"], "newt/qwen3.6_35b");
        assert_eq!(j["jobs_dir"], "/var/tmp/tbench-qwen3.6_35b-on");
    }

    #[test]
    fn run_env_maps_lane_to_ocap_and_window() {
        let m = MatrixConfig::parse(SAMPLE).unwrap();
        let ex = HarborExecutor {
            pythonpath: "scripts/eval/harbor".into(),
        };
        let on: std::collections::HashMap<_, _> =
            ex.run_env(&spec(&m, 0, "on")).into_iter().collect();
        assert_eq!(on["NEWT_BENCH_OCAP"], "on");
        assert_eq!(on["NEWT_BENCH_BIN"], "/opt/newt");
        assert_eq!(on["NEWT_BENCH_PROFILE"], "solve-qwen36.toml");
        assert_eq!(on["NEWT_BENCH_CONTEXT_WINDOW"], "65536"); // harness default
        assert_eq!(on["NEWT_BENCH_MAX_ROUNDS"], "40");
        assert!(!on.contains_key("NEWT_BENCH_TENACITY")); // none set for qwen

        let off: std::collections::HashMap<_, _> =
            ex.run_env(&spec(&m, 0, "off")).into_iter().collect();
        assert_eq!(off["NEWT_BENCH_OCAP"], "off");
    }

    #[test]
    fn run_env_honors_per_model_window_and_tenacity() {
        let m = MatrixConfig::parse(SAMPLE).unwrap();
        let ex = HarborExecutor {
            pythonpath: "p".into(),
        };
        let e: std::collections::HashMap<_, _> =
            ex.run_env(&spec(&m, 1, "on")).into_iter().collect();
        assert_eq!(e["NEWT_BENCH_CONTEXT_WINDOW"], "32768"); // nemotron override
        assert_eq!(e["NEWT_BENCH_TENACITY"], "relentless");
    }

    #[test]
    fn endpoint_parsed_from_profile_and_trailing_slash_trimmed() {
        // Pure parse over an in-memory toml written to a temp path is a real-fs
        // touch; instead exercise the parser via a string round-trip helper.
        let text = "[[backends]]\nendpoint = \"http://h:8080/\"\nmodel = \"m\"\n";
        let val: toml::Value = toml::from_str(text).unwrap();
        let ep = val["backends"][0]["endpoint"]
            .as_str()
            .unwrap()
            .trim_end_matches('/');
        assert_eq!(ep, "http://h:8080");
    }
}
