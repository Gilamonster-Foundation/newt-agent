//! `newt bench` matrix config — the **roster as manifest** (#1490).
//!
//! One TOML file describes the whole Terminal-Bench matrix: the suite (which
//! tasks), the harness (how to run them), and the roster (which models). A new
//! model is one `[[model]]` entry — config, never code (the three Cs).
//!
//! Two invariants this file enforces at parse time:
//!
//! - **Each model id is its own identity.** Model names must be UNIQUE; the
//!   roster never lumps variants together (the router alone serves six distinct
//!   nemotrons — `nemotron-3-nano_30b`, `-canonical`, `-nano-omni`,
//!   `nemotron-3-super_120b`, `nemotron-mini_4b`, `nemotron_70b-instruct`). Each
//!   is an independent row with its own ratchet and parity verdict; `family` is
//!   a display label ONLY, never the key.
//! - **Lanes are `off` and/or `on`.** Any other lane name is a config error.
//!
//! The "one model at a time" rule is an *orchestration* invariant (the run loop,
//! not this file): dgx1's shared unified-memory pool holds a single model, so
//! the model loop is strictly sequential. `concurrent` here is
//! `n_concurrent_trials` *within* one already-loaded model's suite.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// The whole matrix: suite × harness × roster.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixConfig {
    pub suite: SuiteConfig,
    #[serde(default)]
    pub harness: HarnessConfig,
    /// The roster. `[[model]]` in TOML; each entry is an independent identity.
    #[serde(default, rename = "model")]
    pub models: Vec<ModelEntry>,
}

/// Which tasks the matrix runs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteConfig {
    /// Suite label recorded on every scoreboard row (e.g. `tb-30`).
    pub name: String,
    /// The Terminal-Bench dataset path the tasks live under.
    #[serde(default)]
    pub dataset: String,
    /// The fixed task list — the cross-model instrument.
    #[serde(default)]
    pub tasks: Vec<String>,
}

/// How the suite is executed — shared across every model.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HarnessConfig {
    /// Which [`BenchExecutor`](super::executor) backs the run. `harbor` today.
    pub executor: String,
    /// `n_concurrent_trials` WITHIN one already-loaded model's suite. NOT a
    /// license to load two models at once — the model loop stays sequential.
    pub concurrent: usize,
    /// Multiplies each task's own timeout (long tasks under a big window).
    pub timeout_multiplier: f64,
    /// Tool-round cap handed to `newt solve --max-rounds`.
    pub max_rounds: usize,
    /// The container-portable `newt` binary injected into each task container.
    pub binary: String,
    /// The OCAP lanes to run: `off` (the `--yolo` bench), `on` (confined), or both.
    pub lanes: Vec<String>,
    /// Served context window default (per-model `window` overrides it).
    pub context_window: Option<usize>,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            executor: "harbor".into(),
            concurrent: 2,
            timeout_multiplier: 1.5,
            max_rounds: 40,
            binary: String::new(),
            lanes: vec!["off".into()],
            context_window: None,
        }
    }
}

/// One model in the roster — an independent identity.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// The EXACT served model id. This is the identity: the scoreboard's
    /// champion/gate/parity all key on it. Never a family bucket.
    pub name: String,
    /// The backend profile toml (host-secret endpoint + model) to inject.
    #[serde(default)]
    pub profile: String,
    /// Per-model served window override (else the harness default).
    #[serde(default)]
    pub window: Option<usize>,
    /// Display/tenacity-family label ONLY — never the identity.
    #[serde(default)]
    pub family: Option<String>,
    /// Optional tenacity dial for this model's runs.
    #[serde(default)]
    pub tenacity: Option<String>,
}

impl ModelEntry {
    /// The served window for this model: its own override, else the harness
    /// default. `None` leaves `newt solve` on its built-in default.
    #[must_use]
    pub fn effective_window(&self, harness: &HarnessConfig) -> Option<usize> {
        self.window.or(harness.context_window)
    }
}

impl MatrixConfig {
    /// Parse and validate a matrix from TOML text.
    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Self = toml::from_str(text).context("parsing bench matrix TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Enforce the two parse-time invariants: unique model ids (no lumping) and
    /// only `off`/`on` lanes.
    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        for m in &self.models {
            if m.name.trim().is_empty() {
                bail!("a [[model]] entry has an empty name");
            }
            if !seen.insert(m.name.as_str()) {
                bail!(
                    "duplicate model id `{}` in the roster — each model is its own \
                     independent identity; never list one twice or lump variants",
                    m.name
                );
            }
        }
        for lane in &self.harness.lanes {
            if lane != "off" && lane != "on" {
                bail!("unknown OCAP lane `{lane}` (must be `off` or `on`)");
            }
        }
        // A run needs a real suite: an empty task list or dataset would launch the
        // harness against nothing and silently record a 0/0 row.
        if self.suite.tasks.is_empty() {
            bail!("[suite] has no tasks — the matrix would run against an empty suite");
        }
        if self.suite.dataset.trim().is_empty() {
            bail!("[suite] dataset is empty — set the Terminal-Bench dataset path");
        }
        // Only `harbor` is wired today; a typo or a not-yet-built executor must
        // fail loudly rather than silently fall through to harbor.
        if self.harness.executor != "harbor" {
            bail!(
                "unknown [harness] executor `{}` (only `harbor` is available)",
                self.harness.executor
            );
        }
        // The harbor executor injects this binary into every task container; an
        // empty path would shadow the adapter's fallback and break injection.
        if self.harness.binary.trim().is_empty() {
            bail!("[harness] binary is empty — set the container-portable newt binary path");
        }
        Ok(())
    }

    /// The lanes to run, honoring a CLI override (`--lane off|on|both`). `both`
    /// or `None` falls back to the configured `[harness] lanes`.
    #[must_use]
    pub fn resolve_lanes(&self, cli_lane: Option<&str>) -> Vec<String> {
        match cli_lane {
            Some("off") => vec!["off".into()],
            Some("on") => vec!["on".into()],
            _ => self.harness.lanes.clone(),
        }
    }

    /// The roster filtered to a single model id when `--model` is given, else
    /// the whole roster. Returns an error if a named model isn't in the roster.
    pub fn select_models(&self, only: Option<&str>) -> Result<Vec<&ModelEntry>> {
        match only {
            None => Ok(self.models.iter().collect()),
            Some(name) => {
                let hit: Vec<&ModelEntry> = self.models.iter().filter(|m| m.name == name).collect();
                if hit.is_empty() {
                    bail!("model `{name}` is not in the roster");
                }
                Ok(hit)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [suite]
        name = "tb-30"
        dataset = "/data/terminal-bench"
        tasks = ["pypi-server", "fix-git"]

        [harness]
        executor = "harbor"
        concurrent = 2
        timeout_multiplier = 1.5
        max_rounds = 40
        binary = "/opt/newt"
        lanes = ["off", "on"]
        context_window = 65536

        [[model]]
        name = "qwen3.6_35b"
        profile = "solve-qwen36.toml"
        family = "qwen"

        [[model]]
        name = "nemotron-3-nano_30b"
        profile = "solve-nemotron.toml"
        window = 32768
        family = "nemotron"
    "#;

    #[test]
    fn parses_suite_harness_and_roster() {
        let m = MatrixConfig::parse(SAMPLE).expect("valid");
        assert_eq!(m.suite.name, "tb-30");
        assert_eq!(m.suite.tasks.len(), 2);
        assert_eq!(m.harness.lanes, vec!["off", "on"]);
        assert_eq!(m.harness.concurrent, 2);
        assert_eq!(m.models.len(), 2);
        assert_eq!(m.models[0].name, "qwen3.6_35b");
    }

    #[test]
    fn per_model_window_overrides_harness_default() {
        let m = MatrixConfig::parse(SAMPLE).unwrap();
        // qwen has no window → harness default 65536; nemotron overrides to 32768.
        assert_eq!(m.models[0].effective_window(&m.harness), Some(65536));
        assert_eq!(m.models[1].effective_window(&m.harness), Some(32768));
    }

    #[test]
    fn harness_defaults_apply_when_section_omitted() {
        // Deserialization defaults are separate from runnability: a suite-only doc
        // deserializes with the [harness] defaults but is NOT a valid matrix
        // (validate() requires tasks/dataset/binary — see the rejection test).
        let m: MatrixConfig = toml::from_str("[suite]\nname = \"tb-30\"\n").unwrap();
        assert_eq!(m.harness.executor, "harbor");
        assert_eq!(m.harness.concurrent, 2);
        assert_eq!(m.harness.lanes, vec!["off"]);
        assert!(m.models.is_empty());
    }

    #[test]
    fn duplicate_model_id_is_rejected_never_lumped() {
        // The never-lump guard: two distinct nemotrons are FINE; the SAME id
        // twice is an error.
        let ok = r#"
            [suite]
            name = "tb-30"
            dataset = "/d"
            tasks = ["t"]
            [harness]
            binary = "/newt"
            [[model]]
            name = "nemotron-3-nano_30b"
            [[model]]
            name = "nemotron-3-super_120b"
        "#;
        assert_eq!(MatrixConfig::parse(ok).unwrap().models.len(), 2);

        let dup = r#"
            [suite]
            name = "tb-30"
            [[model]]
            name = "nemotron-3-nano_30b"
            [[model]]
            name = "nemotron-3-nano_30b"
        "#;
        let err = MatrixConfig::parse(dup).unwrap_err().to_string();
        assert!(err.contains("duplicate model id"), "{err}");
    }

    #[test]
    fn empty_suite_unknown_executor_and_missing_binary_are_rejected() {
        // no tasks
        let e =
            MatrixConfig::parse("[suite]\nname=\"s\"\ndataset=\"/d\"\n[harness]\nbinary=\"/b\"\n")
                .unwrap_err()
                .to_string();
        assert!(e.contains("no tasks"), "{e}");
        // empty dataset
        let e =
            MatrixConfig::parse("[suite]\nname=\"s\"\ntasks=[\"t\"]\n[harness]\nbinary=\"/b\"\n")
                .unwrap_err()
                .to_string();
        assert!(e.contains("dataset is empty"), "{e}");
        // unknown executor
        let e = MatrixConfig::parse(
            "[suite]\nname=\"s\"\ntasks=[\"t\"]\ndataset=\"/d\"\n[harness]\nexecutor=\"native\"\nbinary=\"/b\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("unknown [harness] executor"), "{e}");
        // empty binary (harness defaults binary to "")
        let e = MatrixConfig::parse("[suite]\nname=\"s\"\ntasks=[\"t\"]\ndataset=\"/d\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("binary is empty"), "{e}");
    }

    #[test]
    fn unknown_lane_is_rejected() {
        let bad = r#"
            [suite]
            name = "tb-30"
            [harness]
            lanes = ["off", "sideways"]
        "#;
        let err = MatrixConfig::parse(bad).unwrap_err().to_string();
        assert!(err.contains("unknown OCAP lane"), "{err}");
    }

    #[test]
    fn resolve_lanes_honors_cli_override() {
        let m = MatrixConfig::parse(SAMPLE).unwrap();
        assert_eq!(m.resolve_lanes(Some("on")), vec!["on"]);
        assert_eq!(m.resolve_lanes(Some("off")), vec!["off"]);
        assert_eq!(m.resolve_lanes(Some("both")), vec!["off", "on"]);
        assert_eq!(m.resolve_lanes(None), vec!["off", "on"]);
    }

    #[test]
    fn select_models_filters_or_errors() {
        let m = MatrixConfig::parse(SAMPLE).unwrap();
        assert_eq!(m.select_models(None).unwrap().len(), 2);
        assert_eq!(m.select_models(Some("qwen3.6_35b")).unwrap().len(), 1);
        assert!(m.select_models(Some("no-such-model")).is_err());
    }
}
