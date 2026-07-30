//! Tenacity — how hard the harness pushes the model from exploration to action.
//!
//! The measured failure this answers (2026-07-28, steering-regressions): even a
//! capable coding model, given valid context and a full time budget, will read /
//! search / plan indefinitely and never emit an edit. A clean 25-minute drive on
//! `qwen3-coder_30b` produced 41 read/inspect/plan tool calls and **zero**
//! mutations. Context fixes (objective spine, working-set pin) were necessary but
//! not sufficient: the model *had* the context and still would not act. The
//! bottleneck is action *initiation*, and tenacity is the dial for it — our
//! answer to little-coder's tight action loop.
//!
//! Tenacity is a single operator-facing LEVEL that maps to concrete
//! action-forcing knobs, so a per-model-family default can pick one level rather
//! than tune raw numbers. Higher tenacity forces the edit sooner and makes plan
//! mode hand off to a mandatory action. [`Tenacity::Standard`] reproduces the
//! historical hardcoded behaviour (`READ_ONLY_NUDGE_AFTER = 3`), so it is a
//! behaviour-preserving default.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// How insistently the harness drives the model from reading to acting.
///
/// Ordered from most patient to most forcing. Small / over-exploring model
/// families default to a higher tenacity; frontier models that explore with
/// purpose can sit at [`Tenacity::Standard`] or [`Tenacity::Relaxed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tenacity {
    /// Most patient: tolerate a long look-around before a forcing nudge, and
    /// leave plan-mode exit advisory. For models whose exploration is
    /// purposeful and who edit on their own.
    Relaxed,
    /// The historical default: nudge toward action after 3 read-only rounds;
    /// plan-mode exit is advisory. Behaviour-preserving.
    #[default]
    Standard,
    /// Force sooner (2 read-only rounds) and make `exit_plan_mode` hand off to a
    /// mandatory edit. For families that tend to over-explore.
    Insistent,
    /// Most forcing: nudge after a single read-only round and require an edit on
    /// plan-mode exit. For small models that otherwise never act.
    Relentless,
}

impl Tenacity {
    /// Number of consecutive read-only rounds tolerated before the harness
    /// injects an action-forcing nudge. Lower = more tenacious. `Standard` = 3,
    /// the historical `READ_ONLY_NUDGE_AFTER`.
    pub fn read_only_nudge_after(self) -> usize {
        match self {
            Self::Relaxed => 6,
            Self::Standard => 3,
            Self::Insistent => 2,
            Self::Relentless => 1,
        }
    }

    /// Whether leaving plan mode (`exit_plan_mode`) must hand off to a concrete
    /// edit — the harness steers the very next turn toward a mutation rather than
    /// letting the model slide back into more reading.
    pub fn exit_plan_requires_edit(self) -> bool {
        matches!(self, Self::Insistent | Self::Relentless)
    }

    /// Stable lowercase label — the wire/config/`/tenacity` spelling.
    pub fn label(self) -> &'static str {
        match self {
            Self::Relaxed => "relaxed",
            Self::Standard => "standard",
            Self::Insistent => "insistent",
            Self::Relentless => "relentless",
        }
    }

    /// One-line description of what this level does, for `/tenacity` and the
    /// footer indicator.
    pub fn describe(self) -> String {
        let edit = if self.exit_plan_requires_edit() {
            "exit_plan_mode requires an edit"
        } else {
            "exit_plan_mode is advisory"
        };
        format!(
            "force an edit after {} read-only round(s); {edit}",
            self.read_only_nudge_after()
        )
    }

    /// All levels, patient → forcing (for `/tenacity` listings and menus).
    pub fn all() -> [Self; 4] {
        [
            Self::Relaxed,
            Self::Standard,
            Self::Insistent,
            Self::Relentless,
        ]
    }
}

impl fmt::Display for Tenacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Tenacity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "relaxed" => Ok(Self::Relaxed),
            "standard" | "default" => Ok(Self::Standard),
            "insistent" => Ok(Self::Insistent),
            "relentless" => Ok(Self::Relentless),
            other => Err(format!(
                "unknown tenacity '{other}' (relaxed|standard|insistent|relentless)"
            )),
        }
    }
}

/// The `[tenacity]` config section: a baseline level plus per-model-family
/// overrides. Pure data (the three-Cs "knowledge in data" rule) — a new family's
/// default is one map entry, not a new branch, mirroring the model-card
/// [`crate::model_card::family_defaults`] pattern.
///
/// ```toml
/// [tenacity]
/// default = "standard"
/// [tenacity.families]
/// nemotron = "relentless"   # small/over-exploring family → force sooner
/// qwen3    = "standard"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TenacityConfig {
    /// Baseline level when no per-family override matches. `None` ⇒ [`Tenacity`]'s
    /// `Default` (`Standard`), so an empty `[tenacity]` changes nothing.
    pub default: Option<Tenacity>,
    /// Per-model-family overrides, keyed by the card's `family` label (e.g.
    /// `"qwen3"`, `"nemotron"`). Matched case-insensitively. Supersedes
    /// [`default`](Self::default); an explicit CLI `--tenacity` supersedes this.
    pub families: std::collections::BTreeMap<String, Tenacity>,
}

impl TenacityConfig {
    /// The configured level for a model `family` (case-insensitive): a per-family
    /// override if one matches, else [`default`](Self::default), else `Standard`.
    /// `family == None` (unknown/unresolved) skips straight to the default.
    pub fn resolve(&self, family: Option<&str>) -> Tenacity {
        family
            .and_then(|f| {
                self.families
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(f.trim()))
                    .map(|(_, v)| *v)
            })
            .or(self.default)
            .unwrap_or_default()
    }

    /// Attribute a family to a model for per-family resolution, without requiring
    /// a model card per model. The card's `family` wins when it names a configured
    /// family; otherwise infer it by matching a configured family KEY as a
    /// case-insensitive substring of the model NAME — so `"qwen3-coder_30b"` picks
    /// up a `[tenacity.families] qwen3` default, and the whole matrix (gemma,
    /// nemotron, deepseek, kimi, glm…) works from config alone. The match set is
    /// the operator's own `families` keys (data, not a hardcoded list). `None`
    /// when nothing matches ⇒ [`resolve`](Self::resolve) uses the default.
    pub fn family_for(&self, model: &str, card_family: Option<&str>) -> Option<String> {
        if let Some(fam) = card_family {
            let fam = fam.trim();
            if self.families.keys().any(|k| k.eq_ignore_ascii_case(fam)) {
                return Some(fam.to_string());
            }
        }
        let lname = model.to_ascii_lowercase();
        self.families
            .keys()
            .find(|k| lname.contains(&k.to_ascii_lowercase()))
            .cloned()
    }
}

/// Full resolution order, most-specific first: an explicit operator choice
/// (`--tenacity`) wins over any config; config per-family wins over config
/// default; `Standard` is the floor. `config == None` ⇒ CLI-or-`Standard`.
pub fn resolve_tenacity(
    cli: Option<Tenacity>,
    config: Option<&TenacityConfig>,
    family: Option<&str>,
) -> Tenacity {
    cli.unwrap_or_else(|| config.map(|c| c.resolve(family)).unwrap_or_default())
}

// The three tenacity inputs, each stashed by the one site that knows it — the
// operator dial can't be threaded through every loop construction site. They are
// combined lazily by [`effective_tenacity`] via [`resolve_tenacity`], so each
// setter is independent and order-free:
//   - CLI `--tenacity` flag (highest), set in the CLI dispatch,
//   - the `[tenacity]` config, stashed at `Config::resolve`,
//   - the active model's family, set at model selection.
// All absent ⇒ [`Tenacity`]'s `Default` (`Standard`) — behaviour-preserving.
static CLI_TENACITY: std::sync::Mutex<Option<Tenacity>> = std::sync::Mutex::new(None);
static TENACITY_CONFIG: std::sync::Mutex<Option<TenacityConfig>> = std::sync::Mutex::new(None);
static ACTIVE_FAMILY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Install the explicit CLI `--tenacity` override (highest priority). Call once,
/// before the agentic loop starts.
pub fn set_cli_tenacity(level: Tenacity) {
    if let Ok(mut slot) = CLI_TENACITY.lock() {
        *slot = Some(level);
    }
}

/// Clear the explicit CLI `--tenacity` override, so tenacity resolves from config
/// / family again. The complement of [`set_cli_tenacity`]: it lets a surface
/// express "inherit" (no override) rather than pinning the currently-resolved
/// value — e.g. the config panel must not persist an untouched dial.
pub fn clear_cli_tenacity() {
    if let Ok(mut slot) = CLI_TENACITY.lock() {
        *slot = None;
    }
}

/// The raw CLI `--tenacity` override, if one is installed (`None` = inherit from
/// config / family). Distinct from [`effective_tenacity`], which resolves the
/// full precedence ladder to a concrete level.
#[must_use]
pub fn cli_tenacity() -> Option<Tenacity> {
    CLI_TENACITY.lock().ok().and_then(|s| *s)
}

/// Install the resolved `[tenacity]` config (per-family + default). Called from
/// `Config::resolve`, the single canonical config-application entry.
pub fn set_tenacity_config(config: TenacityConfig) {
    if let Ok(mut slot) = TENACITY_CONFIG.lock() {
        *slot = Some(config);
    }
}

/// Install the active model's family (from its model card) so per-family config
/// defaults apply. Called at model selection. `None` clears it.
pub fn set_active_model_family(family: Option<String>) {
    if let Ok(mut slot) = ACTIVE_FAMILY.lock() {
        *slot = family;
    }
}

/// The tenacity in effect, resolved from the three inputs (most-specific first):
/// the CLI `--tenacity` flag, then the `[tenacity]` config's per-family override
/// for the active family, then the config default, then `Standard`.
pub fn effective_tenacity() -> Tenacity {
    let cli = CLI_TENACITY.lock().ok().and_then(|s| *s);
    let config = TENACITY_CONFIG.lock().ok().and_then(|s| s.clone());
    let family = ACTIVE_FAMILY.lock().ok().and_then(|s| s.clone());
    resolve_tenacity(cli, config.as_ref(), family.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_is_the_behaviour_preserving_default() {
        // Standard must reproduce the historical hardcoded READ_ONLY_NUDGE_AFTER
        // so adopting tenacity changes nothing until an operator/family opts in.
        assert_eq!(Tenacity::default(), Tenacity::Standard);
        assert_eq!(Tenacity::Standard.read_only_nudge_after(), 3);
        assert!(!Tenacity::Standard.exit_plan_requires_edit());
    }

    #[test]
    fn higher_tenacity_forces_action_sooner() {
        // The budget is monotonically non-increasing as tenacity rises.
        let budgets: Vec<usize> = Tenacity::all()
            .iter()
            .map(|t| t.read_only_nudge_after())
            .collect();
        assert_eq!(budgets, vec![6, 3, 2, 1]);
        for pair in budgets.windows(2) {
            assert!(pair[0] > pair[1], "budget must strictly drop with tenacity");
        }
    }

    #[test]
    fn only_the_two_most_forcing_levels_require_an_edit_on_plan_exit() {
        assert!(!Tenacity::Relaxed.exit_plan_requires_edit());
        assert!(!Tenacity::Standard.exit_plan_requires_edit());
        assert!(Tenacity::Insistent.exit_plan_requires_edit());
        assert!(Tenacity::Relentless.exit_plan_requires_edit());
    }

    #[test]
    fn parse_is_case_insensitive_and_round_trips_the_label() {
        for t in Tenacity::all() {
            assert_eq!(t.label().parse::<Tenacity>().unwrap(), t);
            assert_eq!(t.to_string(), t.label());
        }
        assert_eq!(
            "  RELENTLESS ".parse::<Tenacity>().unwrap(),
            Tenacity::Relentless
        );
        assert_eq!("default".parse::<Tenacity>().unwrap(), Tenacity::Standard);
        assert!("banana".parse::<Tenacity>().is_err());
    }

    #[test]
    fn ordering_runs_patient_to_forcing() {
        assert!(Tenacity::Relaxed < Tenacity::Standard);
        assert!(Tenacity::Standard < Tenacity::Insistent);
        assert!(Tenacity::Insistent < Tenacity::Relentless);
    }

    #[test]
    fn serde_uses_the_lowercase_label() {
        let json = serde_json::to_string(&Tenacity::Insistent).unwrap();
        assert_eq!(json, "\"insistent\"");
        let back: Tenacity = serde_json::from_str("\"relentless\"").unwrap();
        assert_eq!(back, Tenacity::Relentless);
    }

    fn cfg(default: Option<Tenacity>, fams: &[(&str, Tenacity)]) -> TenacityConfig {
        TenacityConfig {
            default,
            families: fams.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    #[test]
    fn config_resolve_prefers_family_then_default_then_standard() {
        let c = cfg(
            Some(Tenacity::Relaxed),
            &[("nemotron", Tenacity::Relentless)],
        );
        // Known family → its override.
        assert_eq!(c.resolve(Some("nemotron")), Tenacity::Relentless);
        // Case-insensitive + trimmed.
        assert_eq!(c.resolve(Some("  NEMOTRON ")), Tenacity::Relentless);
        // Unknown family → the config default.
        assert_eq!(c.resolve(Some("qwen3")), Tenacity::Relaxed);
        // No family → the config default.
        assert_eq!(c.resolve(None), Tenacity::Relaxed);
        // Empty config → Standard.
        assert_eq!(
            TenacityConfig::default().resolve(Some("qwen3")),
            Tenacity::Standard
        );
        assert_eq!(cfg(None, &[]).resolve(None), Tenacity::Standard);
    }

    #[test]
    fn resolve_tenacity_lets_the_cli_flag_win_over_config() {
        let c = cfg(
            Some(Tenacity::Relaxed),
            &[("nemotron", Tenacity::Relentless)],
        );
        // CLI override beats even a matching per-family default.
        assert_eq!(
            resolve_tenacity(Some(Tenacity::Standard), Some(&c), Some("nemotron")),
            Tenacity::Standard
        );
        // No CLI → config per-family.
        assert_eq!(
            resolve_tenacity(None, Some(&c), Some("nemotron")),
            Tenacity::Relentless
        );
        // No CLI, unknown family → config default.
        assert_eq!(
            resolve_tenacity(None, Some(&c), Some("kimi")),
            Tenacity::Relaxed
        );
        // No config at all → CLI-or-Standard.
        assert_eq!(
            resolve_tenacity(None, None, Some("nemotron")),
            Tenacity::Standard
        );
        assert_eq!(
            resolve_tenacity(Some(Tenacity::Insistent), None, None),
            Tenacity::Insistent
        );
    }

    #[test]
    fn family_for_prefers_card_then_infers_from_the_model_name() {
        let c = cfg(
            None,
            &[
                ("qwen3", Tenacity::Standard),
                ("nemotron", Tenacity::Relentless),
            ],
        );
        // Card family that names a configured family wins.
        assert_eq!(
            c.family_for("whatever", Some("qwen3")).as_deref(),
            Some("qwen3")
        );
        // No card → infer from the model name (the matrix case).
        assert_eq!(
            c.family_for("qwen3-coder_30b", None).as_deref(),
            Some("qwen3")
        );
        assert_eq!(
            c.family_for("NVIDIA-Nemotron-3-Nano", None).as_deref(),
            Some("nemotron")
        );
        // No configured family matches the name → None (→ default level).
        assert_eq!(c.family_for("gemma-2-9b", None), None);
        // A card family NOT in the map falls through to name inference.
        assert_eq!(
            c.family_for("qwen3-coder_30b", Some("unlisted")).as_deref(),
            Some("qwen3")
        );
        // Resolving that inferred family gives the per-family level.
        let fam = c.family_for("qwen3-coder_30b", None);
        assert_eq!(c.resolve(fam.as_deref()), Tenacity::Standard);
    }

    #[test]
    fn tenacity_config_parses_from_toml() {
        let c: TenacityConfig = toml::from_str(
            r#"
            default = "insistent"
            [families]
            nemotron = "relentless"
            qwen3 = "standard"
        "#,
        )
        .unwrap();
        assert_eq!(c.default, Some(Tenacity::Insistent));
        assert_eq!(c.resolve(Some("nemotron")), Tenacity::Relentless);
        assert_eq!(c.resolve(Some("qwen3")), Tenacity::Standard);
        assert_eq!(c.resolve(Some("gemma")), Tenacity::Insistent);
    }
}
