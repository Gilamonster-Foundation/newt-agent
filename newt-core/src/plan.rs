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

    /// The subtask with id `id`, if present.
    #[must_use]
    pub fn subtask(&self, id: &str) -> Option<&Subtask> {
        self.subtasks.iter().find(|s| s.id == id)
    }

    /// Root subtasks — those with no [`Subtask::parent`] (the top of the
    /// decomposition tree).
    #[must_use]
    pub fn roots(&self) -> Vec<&Subtask> {
        self.subtasks
            .iter()
            .filter(|s| s.parent.is_none())
            .collect()
    }

    /// Direct children of `id` — subtasks whose `parent` is `id`.
    #[must_use]
    pub fn children(&self, id: &str) -> Vec<&Subtask> {
        self.subtasks
            .iter()
            .filter(|s| s.parent.as_deref() == Some(id))
            .collect()
    }

    /// Leaves — subtasks that no other subtask names as `parent`. A leaf is the
    /// dispatch/execute unit (a leaf *is* a `CrewTask`); a non-leaf is a branch
    /// (grouping / aggregation). In a flat, single-level plan *every* subtask is a
    /// leaf, so this degrades to "all subtasks" — the pre-tree behaviour, so
    /// existing flat plans are unaffected.
    #[must_use]
    pub fn leaves(&self) -> Vec<&Subtask> {
        let parented: std::collections::HashSet<&str> = self
            .subtasks
            .iter()
            .filter_map(|s| s.parent.as_deref())
            .collect();
        self.subtasks
            .iter()
            .filter(|s| !parented.contains(s.id.as_str()))
            .collect()
    }

    /// The next **ready leaf** to dispatch — the execution cursor. A [`leaf`] that
    /// is [`SubtaskStatus::Pending`] and whose every [`dep`] is
    /// [`SubtaskStatus::Done`]. `None` when nothing is ready (all done, every
    /// pending leaf is dep-blocked, or work is in flight). A `dep` counts as
    /// satisfied iff the named subtask exists and is `Done`; an absent (e.g.
    /// cross-fragment) dep is treated as unsatisfied, so a plan never runs a leaf
    /// ahead of a prerequisite it cannot see.
    ///
    /// [`leaf`]: Plan::leaves
    /// [`dep`]: Subtask::deps
    #[must_use]
    pub fn next_ready_leaf(&self) -> Option<&Subtask> {
        self.leaves().into_iter().find(|s| {
            s.status == SubtaskStatus::Pending
                && s.deps
                    .iter()
                    .all(|d| matches!(self.subtask(d).map(|t| t.status), Some(SubtaskStatus::Done)))
        })
    }

    /// The next leaf to **dispatch**, as `(id, CrewTask)` — [`next_ready_leaf`]
    /// projected through [`Subtask::to_crew_task`]. This is the drive loop's read
    /// step: dispatch the `CrewTask`, then [`mark`](Plan::mark) the `id`
    /// `Done`/`Failed` and call again. `None` when the plan is complete or stalled
    /// (every remaining leaf blocked by a non-`Done` dep). The `id` is returned
    /// because the projected `CrewTask` deliberately drops it (it is the plan's
    /// bookkeeping, not the child's).
    ///
    /// [`next_ready_leaf`]: Plan::next_ready_leaf
    #[must_use]
    pub fn next_dispatch(&self, parent: &Caveats) -> Option<(String, CrewTask)> {
        self.next_ready_leaf()
            .map(|s| (s.id.clone(), s.to_crew_task(parent)))
    }

    /// Record a leaf's outcome — set its [`status`](Subtask::status) and, when
    /// `result` is `Some`, its [`result`](Subtask::result). No-op if `id` is
    /// absent. The drive loop calls this after each dispatch; marking a leaf
    /// `Done` may unblock its dependents on the next [`next_dispatch`], and
    /// marking it `Failed` leaves them blocked (deps require `Done`), so the run
    /// stops honestly at the first failure without a separate "stop" flag.
    ///
    /// [`next_dispatch`]: Plan::next_dispatch
    pub fn mark(&mut self, id: &str, status: SubtaskStatus, result: Option<String>) {
        if let Some(s) = self.subtasks.iter_mut().find(|s| s.id == id) {
            s.status = status;
            if result.is_some() {
                s.result = result;
            }
        }
    }

    /// Every leaf is `Done` — the plan finished successfully (branches are
    /// grouping nodes, so only leaf completion is load-bearing). An empty plan is
    /// trivially complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.leaves()
            .iter()
            .all(|s| s.status == SubtaskStatus::Done)
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
    /// The id of the subtask this one decomposes — `None` for a root. This is how
    /// a flat `[[subtask]]` list expresses a task→sub-task **tree** (exactly as
    /// [`Subtask::deps`] expresses a DAG via id-pointers, not nesting). A subtask
    /// that no other subtask names as its `parent` is a **leaf** — the unit that
    /// dispatches/executes (a leaf *is* a `CrewTask`); a subtask that *is* named
    /// is a **branch** (a grouping / aggregation node). Kept flat on purpose: a
    /// nested `Vec<Subtask>` would break the fragment handoff (one leaf slice =
    /// one dispatch). `None` is also fine in a fragment whose parent lives outside
    /// the slice (the pointer is soft, like a cross-fragment `dep`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The authority this subtask declares it needs. **Default-deny**: an
    /// omitted policy denies every capability axis (see [`CaveatPolicy`]).
    ///
    /// Serialized **last** so every scalar field precedes this sub-table — TOML
    /// requires values before tables within a `[[subtask]]` entry.
    #[serde(default)]
    pub caveat_policy: CaveatPolicy,
}

/// A leaf [`Subtask`] projected into the unit a `CrewRunner` dispatches — the
/// concrete realization of *"a leaf is a CrewTask"*. Produced by
/// [`Subtask::to_crew_task`]; the runner adds placement (a `workspace_ref`) when
/// it actually spawns the work, so it isn't carried here. No `id`/`deps`/`status`
/// either — those are the plan's bookkeeping, not the child's concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrewTask {
    /// What the child agent is asked to do (the subtask's instruction).
    pub goal: String,
    /// The authority the child runs under — the parent's grant **met** with the
    /// subtask's declared policy. `meet` is the greatest lower bound, so this can
    /// only *narrow* the parent (attenuation, never amplify): a model-proposed
    /// plan can never widen the grant it was handed.
    pub caveats: Caveats,
    /// Files the subtask nominated as curated context (stamped verbatim by the
    /// runner at dispatch).
    pub context: Vec<String>,
}

impl Subtask {
    /// Project this subtask into the [`CrewTask`] the active topology's
    /// `CrewRunner` dispatches — the *same* projection for `/mode
    /// single|crew|mesh|remote`, so a plan authored once lifts across runners
    /// unchanged. `caveats = parent.meet(self.caveat_policy.to_caveats())`: the
    /// plan *requests*, the parent *grants*, `meet` *enforces ⊑* (attenuation
    /// only). Intended for a **leaf** (see [`Plan::leaves`]); a branch is a
    /// grouping node, not a dispatch unit.
    #[must_use]
    pub fn to_crew_task(&self, parent: &Caveats) -> CrewTask {
        CrewTask {
            goal: self.instruction.clone(),
            caveats: parent.meet(&self.caveat_policy.to_caveats()),
            context: self.context.clone(),
        }
    }
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
                parent: Some("epic".to_string()),
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

    #[test]
    fn parent_pointers_build_a_tree() {
        // A root branch "epic" decomposes into two leaves; plus a top-level leaf.
        let toml = r#"
[[subtask]]
id = "epic"
instruction = "the big task"

[[subtask]]
id = "a"
instruction = "sub-task a"
parent = "epic"

[[subtask]]
id = "b"
instruction = "sub-task b"
parent = "epic"

[[subtask]]
id = "solo"
instruction = "a top-level leaf"
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        let ids = |v: Vec<&Subtask>| v.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(plan.roots()), vec!["epic", "solo"]);
        assert_eq!(ids(plan.children("epic")), vec!["a", "b"]);
        // epic is a branch (named as a parent) → not a leaf; a, b, solo are leaves.
        assert_eq!(ids(plan.leaves()), vec!["a", "b", "solo"]);
        assert_eq!(plan.subtask("a").unwrap().parent.as_deref(), Some("epic"));
    }

    #[test]
    fn flat_plan_has_every_subtask_as_a_leaf() {
        // Pre-tree behaviour preserved: no parents → every subtask is a leaf.
        let plan = Plan::from_toml_str(
            "[[subtask]]\nid=\"s1\"\ninstruction=\"x\"\n[[subtask]]\nid=\"s2\"\ninstruction=\"y\"\n",
        )
        .unwrap();
        assert_eq!(plan.leaves().len(), 2);
        assert_eq!(plan.roots().len(), 2);
    }

    #[test]
    fn next_ready_leaf_is_the_execution_cursor() {
        // a Done; b Pending with dep a (Done) → b is the ready leaf. c is
        // dep-blocked (b not Done); epic is a branch, never a dispatch unit.
        let toml = r#"
[[subtask]]
id = "epic"
instruction = "branch"

[[subtask]]
id = "a"
instruction = "first leaf"
parent = "epic"
status = "done"

[[subtask]]
id = "b"
instruction = "second leaf"
parent = "epic"
deps = ["a"]

[[subtask]]
id = "c"
instruction = "third leaf"
parent = "epic"
deps = ["b"]
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        assert_eq!(plan.next_ready_leaf().expect("b ready").id, "b");
    }

    #[test]
    fn next_ready_leaf_none_when_all_done_or_blocked() {
        // a Done, b blocked on an absent dep → no ready leaf.
        let toml = r#"
[[subtask]]
id = "a"
instruction = "done"
status = "done"

[[subtask]]
id = "b"
instruction = "blocked"
deps = ["never_exists"]
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        assert!(plan.next_ready_leaf().is_none());
    }

    #[test]
    fn parent_defaults_none_and_fragment_stays_valid() {
        // Fragment-validity: a bare subtask whose parent lives outside the slice
        // still parses (the pointer is soft); an omitted parent defaults to None.
        let frag = Plan::from_toml_str(
            "[[subtask]]\nid=\"leaf\"\ninstruction=\"x\"\nparent=\"outside_the_slice\"\n",
        )
        .unwrap();
        assert_eq!(
            frag.subtasks[0].parent.as_deref(),
            Some("outside_the_slice")
        );
        let root = Plan::from_toml_str("[[subtask]]\nid=\"r\"\ninstruction=\"y\"\n").unwrap();
        assert!(root.subtasks[0].parent.is_none());
        assert_eq!(root.roots().len(), 1);
    }

    #[test]
    fn to_crew_task_projects_goal_context_and_attenuated_caveats() {
        // A leaf declaring fs_write=["src/"] only; the parent grants everything.
        let toml = r#"
[[subtask]]
id = "leaf"
instruction = "write the module"
context = ["src/lib.rs"]

[subtask.caveat_policy]
fs_write = ["src/"]
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        let task = plan.subtasks[0].to_crew_task(&Caveats::top());
        assert_eq!(task.goal, "write the module");
        assert_eq!(task.context, vec!["src/lib.rs".to_string()]);
        // The child gets exactly what it declared (parent=top allows all):
        assert_eq!(task.caveats.fs_write, Scope::only(["src/".to_string()]));
        // Everything else stays DENIED (default-deny leaf) — never widened to top.
        assert_eq!(task.caveats.fs_read, Scope::none());
        assert_eq!(task.caveats.exec, Scope::none());
        assert_eq!(task.caveats.net, Scope::none());
    }

    #[test]
    fn to_crew_task_never_widens_past_the_parent() {
        // The leaf REQUESTS fs_read=all, but the parent only GRANTS fs_read=["a/"];
        // meet clamps the request to the grant — attenuation, never amplify.
        let toml = r#"
[[subtask]]
id = "leaf"
instruction = "x"

[subtask.caveat_policy]
fs_read = "all"
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        let parent = Caveats {
            fs_read: Scope::only(["a/".to_string()]),
            ..Caveats::top()
        };
        let task = plan.subtasks[0].to_crew_task(&parent);
        assert_eq!(task.caveats.fs_read, Scope::only(["a/".to_string()]));
    }

    #[test]
    fn plan_is_a_drivable_execution_state_machine() {
        // An overseer-authored DAG: a → b(deps a) → c(deps b), all under "epic".
        let toml = r#"
[[subtask]]
id = "epic"
instruction = "branch"

[[subtask]]
id = "a"
instruction = "step a"
parent = "epic"

[[subtask]]
id = "b"
instruction = "step b"
parent = "epic"
deps = ["a"]

[[subtask]]
id = "c"
instruction = "step c"
parent = "epic"
deps = ["b"]
"#;
        let mut plan = Plan::from_toml_str(toml).unwrap();
        let top = Caveats::top();
        // The drive loop a real executor runs: dispatch the next ready leaf,
        // mark it Done, repeat. (Here the "dispatch" is a no-op; a CrewRunner
        // would run inference. The state machine is what's under test.)
        let mut order = Vec::new();
        while let Some((id, task)) = plan.next_dispatch(&top) {
            assert!(!task.goal.is_empty());
            plan.mark(&id, SubtaskStatus::Running, None);
            plan.mark(&id, SubtaskStatus::Done, Some(format!("ran {id}")));
            order.push(id);
        }
        // Walked a → b → c in dependency order; epic (a branch) never dispatched.
        assert_eq!(order, vec!["a", "b", "c"]);
        assert!(plan.is_complete());
        assert_eq!(plan.subtask("a").unwrap().result.as_deref(), Some("ran a"));
    }

    #[test]
    fn a_failed_leaf_blocks_its_dependents_and_stops_the_run() {
        let toml = r#"
[[subtask]]
id = "a"
instruction = "x"

[[subtask]]
id = "b"
instruction = "y"
deps = ["a"]
"#;
        let mut plan = Plan::from_toml_str(toml).unwrap();
        let top = Caveats::top();
        let (id, _task) = plan.next_dispatch(&top).expect("a is ready");
        assert_eq!(id, "a");
        plan.mark(&id, SubtaskStatus::Failed, Some("boom".into()));
        // b deps on a (now Failed, not Done) → not ready → nothing to dispatch,
        // so the run stops honestly at the first failure (no separate stop flag).
        assert!(plan.next_dispatch(&top).is_none());
        assert!(!plan.is_complete());
    }

    #[test]
    fn mark_is_a_noop_for_an_absent_id_and_empty_plan_is_complete() {
        let mut plan = Plan::from_toml_str("[[subtask]]\nid=\"a\"\ninstruction=\"x\"\n").unwrap();
        plan.mark("nope", SubtaskStatus::Done, Some("ignored".into()));
        assert_eq!(plan.subtask("a").unwrap().status, SubtaskStatus::Pending);
        assert!(Plan::from_toml_str("").unwrap().is_complete());
    }
}
