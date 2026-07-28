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
}
