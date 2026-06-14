//! The canonical agent **plan**: one `serde` [`Plan`] / [`Subtask`] definition
//! shared by the human-driven collaborative `/plan` surface (#334) and the
//! swarm scheduler (Workstream C / the future `newt-scheduler`).
//!
//! The plain-text plan file is the single source of truth across tiers
//! (newt/drake *author* → headless wyvern *executes*), so this struct is the
//! **only** definition both sides deserialize. Landing it once prevents the
//! "two plan shapes wearing one filename" drift flagged in #334: the `/plan`
//! S3a author and the C1 scheduler consume the *same* `struct`, or they diverge.
//!
//! Three properties are load-bearing and tested (§ `tests`):
//!
//! 1. **Fragment-validity.** A bare `[[subtask]]` list (no top-level `goal`)
//!    deserializes on its own, so a *slice* of a plan can be handed to one
//!    flight via `wyvern --plan`/stdin (#334 S3f). The handoff is a parse
//!    invariant, not a hope.
//! 2. **Default-deny authority.** An omitted [`Subtask::caveat_policy`]
//!    deserializes to the *narrowest* policy — every capability axis denied —
//!    never `Caveats::top()` minus a few axes. A model-*proposed* plan must not
//!    acquire authority by omission; the model names what a subtask needs and
//!    the harness grants no more. This is the #319/#332 lesson ("the harness
//!    stamps, the model never asserts") applied at the orchestration layer, and
//!    it pre-wires Workstream C §7: *the plan requests, the parent grants, and
//!    `delegate` enforces `⊑`* (attenuation can only narrow, never widen).
//! 3. **Resumability.** Per-subtask [`Subtask::status`] / [`Subtask::result`]
//!    make the plan file both the proposal *and* the run-log, so `/plan resume`
//!    and `/plan status` (#334 S3e) have a real thing to read and aggregation
//!    has a destination.

use serde::{Deserialize, Serialize};

use crate::role_profile::{ScopeKeyword, ScopeSpec};
use crate::{Caveats, CountBound, Scope};

/// A complete plan, or a fragment of one.
///
/// Serializes to TOML as an optional `goal` + `aggregation` scalar plus a
/// `[[subtask]]` array-of-tables. `goal` is optional precisely so a bare
/// `[[subtask]]` fragment parses (fragment-validity, §module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The overall goal this plan pursues. `None` in a fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    /// How child results are combined back into the plan (default `Concat`).
    #[serde(default)]
    pub aggregation: Aggregation,
    /// The subtasks, as TOML `[[subtask]]` tables. (Rust field `subtasks`,
    /// TOML key `subtask`.)
    #[serde(default, rename = "subtask")]
    pub subtasks: Vec<Subtask>,
}

impl Plan {
    /// Parse a plan (or fragment) from its TOML form.
    ///
    /// # Errors
    /// Returns the `toml` deserialization error on malformed input or an
    /// unknown field (the schema is `deny_unknown_fields` — one canonical shape).
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize this plan to its canonical TOML form.
    ///
    /// # Errors
    /// Returns the `toml` serialization error (should not occur for a
    /// well-formed [`Plan`]).
    pub fn to_toml_string(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

/// One unit of work in a [`Plan`] — the serialized form of a single scheduler
/// dispatch. A fragment-valid `[[subtask]]` table is exactly this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subtask {
    /// Stable identifier, referenced by other subtasks' [`Subtask::deps`].
    pub id: String,
    /// What the child agent is asked to do.
    pub instruction: String,
    /// Ids of subtasks that must complete before this one may start.
    #[serde(default)]
    pub deps: Vec<String>,
    /// May this subtask run concurrently with its ready siblings?
    #[serde(default)]
    pub parallel_ok: bool,
    /// Files the model **nominates** as this subtask's curated context. The
    /// harness stamps the verbatim bytes at dispatch — the model names the
    /// paths, it does not assert their contents (the disclosure discipline).
    #[serde(default)]
    pub context: Vec<String>,
    /// Optional verify command whose **enforced** result gates the subtask
    /// (#332 S1). Absent = no per-subtask gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    /// Execution status — makes the plan file a resumable run-log.
    #[serde(default)]
    pub status: SubtaskStatus,
    /// Where the child's output lands on completion (aggregation destination).
    /// `None` until the subtask has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// The authority this subtask declares it needs. **Default-deny**: an
    /// omitted policy denies every capability axis (see [`CaveatPolicy`]).
    ///
    /// Serialized **last** so every scalar field precedes this sub-table — TOML
    /// requires values before tables within a `[[subtask]]` entry.
    #[serde(default)]
    pub caveat_policy: CaveatPolicy,
}

/// How child results combine back into the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Aggregation {
    /// Concatenate child outputs in subtask order (the default).
    #[default]
    Concat,
    /// Keep only the last completed subtask's output.
    LastWins,
    /// Fold children through a reducer (semantics resolved by the scheduler).
    Reduce,
    /// A caller-defined strategy, resolved by the scheduler.
    Custom,
}

/// Per-subtask execution status; the plan file doubles as a run-log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SubtaskStatus {
    /// Not yet started (the default).
    #[default]
    Pending,
    /// Currently executing.
    Running,
    /// Completed successfully.
    Done,
    /// Failed (verify gate or execution error).
    Failed,
}

/// The authority a [`Subtask`] declares it needs — **default-deny**.
///
/// Reuses the same human-friendly axis vocabulary as
/// [`crate::role_profile::CaveatProfile`] ([`ScopeSpec`] per axis), but with the
/// **opposite default**: where a role profile omits an axis to mean
/// *unrestricted* (top of the axis, matching `Caveats::top()`), a plan omits an
/// axis to mean *denied* (`none`). A model-proposed plan must not gain authority
/// by leaving a field out.
///
/// The `Default` is fully denied; [`to_caveats`](CaveatPolicy::to_caveats)
/// lowers the declared policy into the canonical [`Caveats`] lattice element the
/// scheduler attenuates the parent key against. Because attenuation takes the
/// *meet* with the parent, even an unspecified count bound (`None` → `Unlimited`
/// request) is clamped down to the parent's finite bound — the request can never
/// widen the grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaveatPolicy {
    /// Filesystem read scope (default: `none`).
    #[serde(default = "denied_axis")]
    pub fs_read: ScopeSpec,
    /// Filesystem write scope (default: `none`).
    #[serde(default = "denied_axis")]
    pub fs_write: ScopeSpec,
    /// Command-execution scope (default: `none`).
    #[serde(default = "denied_axis")]
    pub exec: ScopeSpec,
    /// Network scope (default: `none`).
    #[serde(default = "denied_axis")]
    pub net: ScopeSpec,
    /// Tool-call ceiling. `None` = unspecified; the scheduler clamps it to the
    /// parent's bound (it can never widen it).
    #[serde(default)]
    pub max_calls: Option<u64>,
}

/// A single denied axis (`Scope::none`) — the opposite of
/// [`ScopeSpec::default`] (which is `all`). The deny default is what makes a
/// model-proposed plan safe by omission.
fn denied_axis() -> ScopeSpec {
    ScopeSpec::Keyword(ScopeKeyword::None)
}

impl Default for CaveatPolicy {
    fn default() -> Self {
        Self {
            fs_read: denied_axis(),
            fs_write: denied_axis(),
            exec: denied_axis(),
            net: denied_axis(),
            max_calls: None,
        }
    }
}

impl CaveatPolicy {
    /// Lower this declared policy into the canonical [`Caveats`] lattice element
    /// the scheduler attenuates the parent key against. Mirrors
    /// `CaveatProfile::to_caveats`, but inherits this type's deny defaults.
    #[must_use]
    pub fn to_caveats(&self) -> Caveats {
        Caveats {
            fs_read: self.fs_read.to_scope(),
            fs_write: self.fs_write.to_scope(),
            exec: self.exec.to_scope(),
            net: self.net.to_scope(),
            max_calls: match self.max_calls {
                Some(n) => CountBound::AtMost(n),
                None => CountBound::Unlimited,
            },
            valid_for_generation: Scope::All,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DENIED: ScopeSpec = ScopeSpec::Keyword(ScopeKeyword::None);

    #[test]
    fn bare_subtask_fragment_parses_without_a_goal() {
        // Fragment-validity: a slice handed to `wyvern --plan` must parse alone.
        let toml = r#"
[[subtask]]
id = "s1"
instruction = "do the first thing"

[[subtask]]
id = "s2"
instruction = "do the second thing"
deps = ["s1"]
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        assert!(plan.goal.is_none());
        assert_eq!(plan.subtasks.len(), 2);
        assert_eq!(plan.subtasks[1].deps, vec!["s1".to_string()]);
    }

    #[test]
    fn omitted_caveat_policy_denies_every_axis() {
        // The load-bearing safety property: no policy => no authority.
        let toml = r#"
[[subtask]]
id = "s1"
instruction = "untrusted, model-proposed"
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        let pol = &plan.subtasks[0].caveat_policy;
        assert_eq!(pol.fs_read, DENIED);
        assert_eq!(pol.fs_write, DENIED);
        assert_eq!(pol.exec, DENIED);
        assert_eq!(pol.net, DENIED);
        // And the struct default agrees with deserialized-absence.
        assert_eq!(*pol, CaveatPolicy::default());
    }

    #[test]
    fn default_policy_lowers_to_a_fully_denied_caveats() {
        // Default-deny must hold at the lattice level too: every capability
        // scope is `none`, never `Scope::All` (top). Compared via to_scope so
        // the assertion does not depend on Caveats' own equality impl.
        let cav = CaveatPolicy::default().to_caveats();
        assert_eq!(cav.fs_read, Scope::none());
        assert_eq!(cav.fs_write, Scope::none());
        assert_eq!(cav.exec, Scope::none());
        assert_eq!(cav.net, Scope::none());
    }

    #[test]
    fn explicit_policy_is_honored_and_lowers_correctly() {
        let toml = r#"
[[subtask]]
id = "s1"
instruction = "scoped"

[subtask.caveat_policy]
fs_read = "all"
fs_write = ["src/", "Cargo.toml"]
exec = ["cargo"]
net = "none"
max_calls = 40
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        let cav = plan.subtasks[0].caveat_policy.to_caveats();
        assert_eq!(cav.fs_read, Scope::All);
        assert_eq!(
            cav.fs_write,
            Scope::only(["src/".to_string(), "Cargo.toml".to_string()])
        );
        assert_eq!(cav.exec, Scope::only(["cargo".to_string()]));
        assert_eq!(cav.net, Scope::none());
        assert_eq!(cav.max_calls, CountBound::AtMost(40));
    }

    #[test]
    fn status_defaults_to_pending() {
        let toml = r#"
[[subtask]]
id = "s1"
instruction = "x"
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        assert_eq!(plan.subtasks[0].status, SubtaskStatus::Pending);
        assert!(plan.subtasks[0].result.is_none());
    }

    #[test]
    fn plan_round_trips_through_toml() {
        let plan = Plan {
            goal: Some("ship the thing".to_string()),
            aggregation: Aggregation::Concat,
            subtasks: vec![Subtask {
                id: "s1".to_string(),
                instruction: "write the module".to_string(),
                deps: vec![],
                parallel_ok: true,
                context: vec!["src/lib.rs".to_string()],
                verify: Some("cargo test -p x".to_string()),
                caveat_policy: CaveatPolicy {
                    fs_write: ScopeSpec::Items(vec!["src/".to_string()]),
                    ..CaveatPolicy::default()
                },
                status: SubtaskStatus::Done,
                result: Some("done".to_string()),
            }],
        };
        let text = plan.to_toml_string().unwrap();
        let back = Plan::from_toml_str(&text).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn full_plan_with_aggregation_parses() {
        let toml = r#"
goal = "refactor the parser"
aggregation = "lastwins"

[[subtask]]
id = "s1"
instruction = "x"
status = "running"
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        assert_eq!(plan.goal.as_deref(), Some("refactor the parser"));
        assert_eq!(plan.aggregation, Aggregation::LastWins);
        assert_eq!(plan.subtasks[0].status, SubtaskStatus::Running);
    }

    #[test]
    fn unknown_field_is_rejected() {
        // deny_unknown_fields: one canonical shape, no silent drift.
        let toml = r#"
[[subtask]]
id = "s1"
instruction = "x"
bogus_field = "should fail"
"#;
        assert!(Plan::from_toml_str(toml).is_err());
    }

    #[test]
    fn empty_input_is_an_empty_plan() {
        let plan = Plan::from_toml_str("").unwrap();
        assert!(plan.goal.is_none());
        assert!(plan.subtasks.is_empty());
        assert_eq!(plan.aggregation, Aggregation::Concat);
    }
}
