//! Named crew roles, dispatch budgets and policy, and disk loading.
//!
//! Crews validate their role loadouts transitively. The operator clamp remains
//! configuration data; the dispatch layer owns its meet with session authority.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Config;

/// A named crew (`[crews.<name>]` or `crews/<name>.toml`): a role-specialized
/// ensemble over the heterogeneous backend pool. Each role names a `[loadouts.*]`
/// (so a crew is a *composition of loadouts* — the canonical example routes the
/// planner/triage to frontier models and bulk work to cheap local inference,
/// `docs/design/config-scaling-deployment-and-trust.md`). The harness owns the
/// control loop (`run_crew`); these fields pin the workers + budgets.
///
/// ```toml
/// [crews.coder]
/// planner = "planner"          # → [loadouts.planner]  (required)
/// navigator = "navigator"      # → [loadouts.navigator]
/// triage = "triage"            # → [loadouts.triage]
/// loop = "patch-revise"        # control program (default)
///   [crews.coder.budgets]
///   max_attempts = 4
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Crew {
    /// Planner/editor role — must name a `[loadouts.<name>]`. Required (a crew
    /// with no planner can't make edits).
    pub planner: String,
    /// Repo-navigator role — names a `[loadouts.<name>]`. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigator: Option<String>,
    /// Test-triage role — names a `[loadouts.<name>]`. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<String>,
    /// Control program (e.g. `"patch-revise"`). Omitted ⇒ the default loop.
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_program: Option<String>,
    /// Per-role dispatch wall-clock bound, seconds (#698). Omitted ⇒ the
    /// env/default (`NEWT_ROLE_TIMEOUT_SECS` → 600s). Widen it here for a slow
    /// loadout instead of relying on the env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_timeout_secs: Option<u64>,
    /// Verification command override (e.g. `"just check"`). Omitted ⇒ inferred
    /// from the repo (justfile → `just check`, Cargo → `cargo test`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    /// Budgets / safety gates for the control loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budgets: Option<CrewBudgets>,
}

/// `[crew]` dispatch policy (#749 step 2): the operator's structural tightening
/// point for crews/teams the overseer fields.
///
/// A model that fields a crew is the recursion / Confused-Deputy case. Dispatch
/// hands each crew `session ⊓ clamp` (the [`crate::Caveats`] meet), so the crew's
/// authority is **always `≤ session`** (the overseer cannot escalate by
/// dispatching) and **`≤ clamp`** (the operator's bound). With the default
/// `clamp = Caveats::top()` the meet is the identity — today's behavior is
/// unchanged — while the seam exists for tighter clamps (and the per-subtask
/// `team_clamp`, #749 step 8) to plug into.
///
/// ```toml
/// [crew]
/// # crews may reach only this host, even if the session's net grant is wider
/// [crew.clamp]
/// net = { only = ["registry.internal"] }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CrewPolicyConfig {
    /// The authority **clamp** dispatched crews are met against
    /// (`child = session ⊓ clamp`). Defaults to `Caveats::top()` (identity meet —
    /// behavior unchanged). Tighten an axis here to bound every crew below the
    /// session ceiling; later steps (#749 step 8) compose a per-subtask clamp on
    /// top of this at the same `dispatch` seam.
    #[serde(default)]
    pub clamp: crate::caveats::Caveats,
}

/// Budgets + review gates for a crew's control loop (`crew-loadout.md` §budgets).
/// Consumed by the front door; an honest cap-exit at `max_attempts` returns
/// `NeedsHumanReview`, never a false success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CrewBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files_touched: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines_changed: Option<u32>,
    /// Topics that force a human-review pause (e.g. `["auth","crypto","migrations"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_human_review_on: Vec<String>,
}

impl Crew {
    /// Validate the crew's role references against `cfg`: each named role
    /// (`planner`/`navigator`/`triage`) must name a known `[loadouts.<name>]`,
    /// and that loadout must itself validate (so a crew transitively checks the
    /// whole `crew → loadout → {backend,bundle,profile}` chain). A dangling role
    /// is a hard error — a crew that silently dropped a worker would be a false
    /// claim.
    ///
    /// # Errors
    /// The first dangling or invalid role reference, as a message.
    pub fn validate(&self, cfg: &Config) -> std::result::Result<(), String> {
        let check = |label: &str, name: &str| -> std::result::Result<(), String> {
            let loadout = cfg.loadouts.get(name).ok_or_else(|| {
                let known = if cfg.loadouts.is_empty() {
                    "none defined".to_string()
                } else {
                    cfg.loadouts.keys().cloned().collect::<Vec<_>>().join(", ")
                };
                format!(
                    "crew {label} '{name}': no [loadouts] entry named '{name}' (known: {known})"
                )
            })?;
            loadout
                .validate(cfg)
                .map_err(|e| format!("crew {label} '{name}': {e}"))
        };
        check("planner", &self.planner)?;
        if let Some(nav) = &self.navigator {
            check("navigator", nav)?;
        }
        if let Some(tri) = &self.triage {
            check("triage", tri)?;
        }
        Ok(())
    }
}

impl Config {
    /// Merge per-file crews from the `crews/` dirs next to the config:
    /// `~/.newt/crews/*.toml` first, then the project `.newt/crews/` (so project
    /// overrides home overrides inline `[crews.*]`). Filename stem = crew name. A
    /// malformed drop-in is skipped with a warning; references inside a crew are
    /// validated when it is selected (`newt crew --crew <name>`), mirroring the
    /// inline `[crews.*]` and disk-loadout paths.
    pub(super) fn merge_disk_crews(&mut self) {
        if let Some(dir) = Self::user_config_dir() {
            self.merge_crews_from_dir(&dir.join("crews"));
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                self.merge_crews_from_dir(&parent.join("crews"));
            }
        }
    }

    /// Load `<dir>/*.toml` as crews (filename stem = name) into `self.crews`,
    /// last-wins on a name clash. A malformed file is skipped with a warning.
    pub(super) fn merge_crews_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no crews dir — fine
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<Crew>(&t)) {
                Ok(Ok(crew)) => {
                    self.crews.insert(stem.to_string(), crew);
                }
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed crew file");
                }
                Err(_) => {}
            }
        }
    }
}
