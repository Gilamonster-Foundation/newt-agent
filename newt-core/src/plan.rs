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

    /// Overwrite a subtask's instruction by id — used by failure-driven
    /// re-grounding (#692) to steer a retried leaf at the symbol's real file.
    pub fn set_instruction(&mut self, id: &str, instruction: &str) {
        if let Some(s) = self.subtasks.iter_mut().find(|s| s.id == id) {
            s.instruction = instruction.to_string();
        }
    }

    /// Drop a subtask's file scope by id (#812). Used by re-grounding: a
    /// reground means the leaf's grounding was demonstrably wrong, so a
    /// derived-from-the-same-grounding fence is stale — the retry runs
    /// unfenced rather than deterministically re-refusing the corrected edit.
    pub fn clear_context(&mut self, id: &str) {
        if let Some(s) = self.subtasks.iter_mut().find(|s| s.id == id) {
            s.context.clear();
        }
    }

    /// #1062: bind a node to the commit that realizes it — set
    /// [`artifact_ref.commit`](ArtifactRef::commit) (and `branch` when given),
    /// preserving any existing `pr`/`branch`. This is what lets the objective
    /// evaluator ([`crate::roadmap_eval`]) close a Task from git truth instead of
    /// a human `mark`. No-op if `id` is absent.
    pub fn set_artifact_commit(&mut self, id: &str, commit: &str, branch: Option<&str>) {
        if let Some(s) = self.subtasks.iter_mut().find(|s| s.id == id) {
            let existing = s.artifact_ref.take().unwrap_or_default();
            s.artifact_ref = Some(ArtifactRef {
                commit: Some(commit.to_string()),
                branch: branch.map(str::to_string).or(existing.branch),
                ..existing
            });
        }
    }

    /// #1083: bind node `id` to the forge issue it realizes, preserving any
    /// existing branch/commit/pr refs, so the objective evaluator
    /// ([`crate::roadmap_eval`]) can require the issue CLOSED before Done.
    /// No-op if `id` is absent (same contract as
    /// [`set_artifact_commit`](Self::set_artifact_commit)).
    pub fn set_artifact_issue(&mut self, id: &str, issue: u64) {
        if let Some(s) = self.subtasks.iter_mut().find(|s| s.id == id) {
            let existing = s.artifact_ref.take().unwrap_or_default();
            s.artifact_ref = Some(ArtifactRef {
                issue: Some(issue),
                ..existing
            });
        }
    }

    /// #1062 auto-capture: the next Task under `plan_id` that still needs a commit
    /// — the leftmost `Pending` Task in `plan_id`'s subtree whose
    /// [`artifact_ref.commit`](ArtifactRef::commit) is unset. `None` if `plan_id`
    /// isn't a Plan, or every Task beneath it is already captured or closed.
    /// Document order encodes sibling order, so "leftmost" is the first match —
    /// the same rule the DFS cursor uses — giving the 1-commit-→-1-Task default.
    #[must_use]
    pub fn next_uncaptured_task_under(&self, plan_id: &str) -> Option<&Subtask> {
        if self.subtask(plan_id)?.kind != NodeKind::Plan {
            return None;
        }
        self.subtasks.iter().find(|s| {
            s.kind == NodeKind::Task
                && s.status == SubtaskStatus::Pending
                && s.artifact_ref
                    .as_ref()
                    .and_then(|a| a.commit.as_deref())
                    .is_none()
                && self.path_to(&s.id).iter().any(|p| p == plan_id)
        })
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

    /// #1030 tree cursor: the next node to act on in depth-first, sibling order
    /// — the leftmost [`SubtaskStatus::Pending`] subtask whose every [`dep`] is
    /// `Done` **and** whose direct children are all `Done`. Generalizes
    /// [`next_ready_leaf`] to interior nodes: a leaf (no children) is ready as
    /// soon as its deps clear; a branch (Roadmap/Phase/Plan grouping) becomes
    /// ready only once its subtree has completed — which is exactly when its
    /// evaluator should run and fold the children's results upward, then the
    /// cursor returns to the branch's parent. Document order encodes sibling
    /// order, so "leftmost" is the first match. `None` when nothing is ready
    /// (all done, or every pending node is blocked by a dep or an open child).
    ///
    /// [`next_ready_leaf`]: Plan::next_ready_leaf
    /// [`dep`]: Subtask::deps
    #[must_use]
    pub fn next_ready_node(&self) -> Option<&Subtask> {
        self.subtasks.iter().find(|s| {
            s.status == SubtaskStatus::Pending
                && s.deps
                    .iter()
                    .all(|d| matches!(self.subtask(d).map(|t| t.status), Some(SubtaskStatus::Done)))
                && self
                    .children(&s.id)
                    .iter()
                    .all(|c| c.status == SubtaskStatus::Done)
        })
    }

    /// #1030: is node `id` **and its entire subtree** `Done`? A missing id is
    /// `false` — a node we cannot see is not provably complete (mirroring the
    /// unsatisfied-absent-`dep` rule). The traversal uses this to decide when a
    /// branch's children have all finished so it may return up to the parent.
    #[must_use]
    pub fn subtree_complete(&self, id: &str) -> bool {
        match self.subtask(id) {
            None => false,
            Some(node) => {
                node.status == SubtaskStatus::Done
                    && self
                        .children(id)
                        .iter()
                        .all(|c| self.subtree_complete(&c.id))
            }
        }
    }

    /// #1030: the ancestor chain from the root down to `id` (inclusive) as a vec
    /// of ids — the breadcrumb a driver shows and a resume uses to descend.
    /// Empty when `id` is absent. A cycle in the soft [`parent`](Subtask::parent)
    /// pointers is broken by stopping at the first revisited id.
    #[must_use]
    pub fn path_to(&self, id: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cur = self.subtask(id);
        while let Some(node) = cur {
            if !seen.insert(node.id.as_str()) {
                break; // cycle guard: a parent pointer looped back
            }
            chain.push(node.id.clone());
            cur = node.parent.as_deref().and_then(|p| self.subtask(p));
        }
        chain.reverse();
        chain
    }

    /// #1030: all subtasks of a given [`NodeKind`], in document (sibling) order.
    #[must_use]
    pub fn nodes_of_kind(&self, kind: NodeKind) -> Vec<&Subtask> {
        self.subtasks.iter().filter(|s| s.kind == kind).collect()
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
    /// The leaf's FILE SCOPE — its write lane (#812), not reading material.
    /// At dispatch this is forwarded as `args["scope"]` and intersected into
    /// the effective writable set (worktree ∩ fs_write ∩ scope): a meet-only
    /// convenience fence that can only narrow, never widen. Empty = unfenced
    /// (pre-#812 behavior). Matching is exact-file or directory-prefix, with
    /// `./` and trailing-`/` normalized; degenerate entries (`""`, `"."`)
    /// fail open. Populated by the harness's own def-site grounding, with
    /// model-declared `files` appended as untrusted augmentation.
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
    /// #1030 tree node kind — this subtask's role in a Roadmap→Phase→Plan→Task
    /// tree. Defaults to [`NodeKind::Task`] so a legacy flat plan (every subtask
    /// a work unit) is unchanged: the existing leaf/crew semantics read
    /// `status`/`parent`/`deps`, never `kind`. A scalar — precedes the tables.
    #[serde(default)]
    pub kind: NodeKind,
    /// #1030: for a [`NodeKind::Plan`] node, the id of the `conversations` row
    /// that IS this plan's context window ("each plan is a conversational
    /// context"). `None` for Roadmap/Phase grouping nodes, Task leaves, and
    /// every legacy subtask — a soft pointer, like [`Subtask::parent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// #1030: the objective work artifact an evaluator reads to close this node
    /// (a branch/commit for a Task or Plan, a PR for a Phase). `None` until the
    /// node is bound to real work. Serialized as a sub-table, so — like
    /// [`caveat_policy`](Subtask::caveat_policy) — every scalar field precedes
    /// it; it is placed before `caveat_policy` and both tables trail the scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<ArtifactRef>,
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
    /// The leaf's file scope — its write lane (#812; see
    /// [`Subtask::context`]). Forwarded as `args["scope"]` and intersected
    /// into the effective writable set at apply; empty = unfenced.
    pub context: Vec<String>,
    /// The optional verify command that gates this task — forwarded to the crew
    /// op so the child's work is checked before it is accepted (the #332
    /// per-subtask gate). `None` = no per-task check.
    pub verify: Option<String>,
}

impl Subtask {
    /// #1030: a tree node with the given `id`, `instruction`, [`NodeKind`], and
    /// optional `parent`, everything else defaulted — `Pending`, no deps/verify/
    /// result/artifact/conversation, and the default-deny [`CaveatPolicy`]. The
    /// convenience constructor for authoring a roadmap tree node by node.
    #[must_use]
    pub fn node(
        id: impl Into<String>,
        instruction: impl Into<String>,
        kind: NodeKind,
        parent: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            instruction: instruction.into(),
            deps: Vec::new(),
            parallel_ok: false,
            context: Vec::new(),
            verify: None,
            status: SubtaskStatus::default(),
            result: None,
            parent,
            kind,
            conversation_id: None,
            artifact_ref: None,
            caveat_policy: CaveatPolicy::default(),
        }
    }

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
            verify: self.verify.clone(),
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

/// #1030 "Plans within Plans": the role of a [`Subtask`] node in a
/// Roadmap→Phase→Plan→Task tree.
///
/// - `Roadmap` / `Phase` are lightweight grouping nodes (no bound conversation,
///   no turns) — the outline a tiny model never has to hold all at once.
/// - `Plan` is the pivot: it binds a whole `conversations` row via
///   [`Subtask::conversation_id`] — that row IS the model's bounded context
///   window ("each plan is a conversational context").
/// - `Task` is a commit-granular leaf; git is the source of truth, read from
///   [`Subtask::artifact_ref`].
///
/// Defaults to `Task` so a legacy flat plan (every subtask a work unit)
/// deserializes unchanged — `kind` is additive working metadata the existing
/// leaf/crew semantics (`status`/`parent`/`deps`) never read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// The tree root — the whole body of work. Done when every child Phase is
    /// merged to main and the pipelines are green.
    Roadmap,
    /// A group of Plans that lands as one PR. Done when every child Plan is
    /// complete and the PR is merged to main.
    Phase,
    /// One conversational context (a `conversations` row). Done when every
    /// child Task is on the branch and the branch's tests pass.
    Plan,
    /// A commit-granular leaf. Done when its commit is on the branch and its
    /// [`verify`](Subtask::verify) gate passes. The default kind.
    #[default]
    Task,
}

/// #1030: the objective work artifact an evaluator resolves to decide whether a
/// node is done — never the model's self-reported "done". Every field is
/// optional and skipped when empty; the whole ref is `None` on an unbound node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// The branch a Task's commit / a Plan's tasks land on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit that realizes a Task (its "done" is this commit on `branch`
    /// with the [`verify`](Subtask::verify) gate passing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The pull-request number a Phase merges (its "done" is this PR merged to
    /// main).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// The forge issue this node realizes (#1083). When set, the objective
    /// evaluator additionally requires the issue to be CLOSED before the node
    /// may be Done — a verdict input, never a direct Done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u64>,
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
                kind: NodeKind::Task,
                conversation_id: None,
                artifact_ref: None,
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

    // ── #1030 "Plans within Plans": tree node kind, artifact ref, traversal ──

    #[test]
    fn node_kind_defaults_to_task_on_a_legacy_plan() {
        // Adding `kind` must not perturb a pre-#1030 flat plan: every subtask
        // that omits `kind` deserializes as a Task work unit, so the existing
        // crew/leaf semantics are byte-identical.
        let toml = r#"
[[subtask]]
id = "a"
instruction = "x"

[[subtask]]
id = "b"
instruction = "y"
deps = ["a"]
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        assert!(plan.subtasks.iter().all(|s| s.kind == NodeKind::Task));
        assert_eq!(NodeKind::default(), NodeKind::Task);
    }

    #[test]
    fn node_kind_and_artifact_ref_round_trip_through_toml() {
        // A Plan node bound to a conversation and a Task node bound to a commit
        // survive a TOML round-trip unchanged (the roadmaps table will persist
        // exactly this serialized shape).
        let toml = r#"
[[subtask]]
id = "plan-1"
instruction = "implement the parser"
kind = "plan"
conversation_id = "1720000000000-abc"

[[subtask]]
id = "task-1"
instruction = "commit the AST"
kind = "task"
parent = "plan-1"

[subtask.artifact_ref]
branch = "feat/parser"
commit = "deadbeef"
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        let p = plan.subtask("plan-1").unwrap();
        assert_eq!(p.kind, NodeKind::Plan);
        assert_eq!(p.conversation_id.as_deref(), Some("1720000000000-abc"));
        let t = plan.subtask("task-1").unwrap();
        assert_eq!(t.kind, NodeKind::Task);
        assert_eq!(
            t.artifact_ref.as_ref().unwrap().commit.as_deref(),
            Some("deadbeef")
        );
        // Round-trip: scalars (incl. `kind`, `conversation_id`) precede both the
        // `artifact_ref` and `caveat_policy` sub-tables, so TOML re-serializes.
        let back = Plan::from_toml_str(&plan.to_toml_string().unwrap()).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn next_ready_node_generalizes_the_cursor_to_branches() {
        // The cursor now also visits interior nodes: a Phase branch becomes
        // "ready" (for its evaluator) only once its child Tasks finish, then the
        // traversal returns up to it. Leaves still lead.
        let toml = r#"
[[subtask]]
id = "phase-1"
instruction = "phase"
kind = "phase"

[[subtask]]
id = "t1"
instruction = "task 1"
kind = "task"
parent = "phase-1"

[[subtask]]
id = "t2"
instruction = "task 2"
kind = "task"
parent = "phase-1"
deps = ["t1"]
"#;
        let mut plan = Plan::from_toml_str(toml).unwrap();
        assert_eq!(plan.next_ready_node().map(|s| s.id.as_str()), Some("t1"));
        plan.mark("t1", SubtaskStatus::Done, None);
        assert_eq!(plan.next_ready_node().map(|s| s.id.as_str()), Some("t2"));
        plan.mark("t2", SubtaskStatus::Done, None);
        // Children all Done → the branch is now the cursor (evaluate + fold up).
        assert_eq!(
            plan.next_ready_node().map(|s| s.id.as_str()),
            Some("phase-1")
        );
        plan.mark("phase-1", SubtaskStatus::Done, None);
        assert_eq!(plan.next_ready_node(), None);
    }

    #[test]
    fn subtree_complete_tracks_the_whole_subtree() {
        let toml = r#"
[[subtask]]
id = "p"
instruction = "parent"
kind = "phase"

[[subtask]]
id = "c1"
instruction = "child 1"
parent = "p"

[[subtask]]
id = "c2"
instruction = "child 2"
parent = "p"
"#;
        let mut plan = Plan::from_toml_str(toml).unwrap();
        assert!(!plan.subtree_complete("p"));
        assert!(!plan.subtree_complete("absent"));
        plan.mark("c1", SubtaskStatus::Done, None);
        plan.mark("c2", SubtaskStatus::Done, None);
        // Children done, but the branch itself is not yet marked → incomplete.
        assert!(!plan.subtree_complete("p"));
        plan.mark("p", SubtaskStatus::Done, None);
        assert!(plan.subtree_complete("p"));
        assert!(plan.subtree_complete("c1"));
    }

    #[test]
    fn path_to_returns_the_root_to_node_breadcrumb() {
        let toml = r#"
[[subtask]]
id = "road"
instruction = "roadmap"
kind = "roadmap"

[[subtask]]
id = "ph"
instruction = "phase"
kind = "phase"
parent = "road"

[[subtask]]
id = "pl"
instruction = "plan"
kind = "plan"
parent = "ph"
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        assert_eq!(plan.path_to("pl"), ["road", "ph", "pl"]);
        assert_eq!(plan.path_to("road"), ["road"]);
        assert!(plan.path_to("absent").is_empty());
    }

    #[test]
    fn nodes_of_kind_filters_by_kind() {
        let toml = r#"
[[subtask]]
id = "road"
instruction = "roadmap"
kind = "roadmap"

[[subtask]]
id = "pl1"
instruction = "plan 1"
kind = "plan"
parent = "road"

[[subtask]]
id = "pl2"
instruction = "plan 2"
kind = "plan"
parent = "road"
"#;
        let plan = Plan::from_toml_str(toml).unwrap();
        let plans: Vec<_> = plan
            .nodes_of_kind(NodeKind::Plan)
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(plans, ["pl1", "pl2"]);
        assert_eq!(plan.nodes_of_kind(NodeKind::Roadmap).len(), 1);
        assert!(plan.nodes_of_kind(NodeKind::Task).is_empty());
    }

    #[test]
    fn set_artifact_commit_binds_the_commit_preserving_pr_and_branch() {
        // #1062: binding a Task's commit is what lets the objective evaluator
        // close it from git truth. It must preserve any existing pr, keep the
        // branch when none is given, create a fresh ref on a bare node, and no-op
        // an absent id.
        let toml = r#"
[[subtask]]
id = "t1"
instruction = "task 1"
kind = "task"

[subtask.artifact_ref]
pr = 42
branch = "feat/x"

[[subtask]]
id = "t2"
instruction = "task 2"
kind = "task"
"#;
        let mut plan = Plan::from_toml_str(toml).unwrap();

        // No explicit branch → existing branch kept, pr preserved.
        plan.set_artifact_commit("t1", "b56fefadeadbeef", None);
        let a = plan.subtask("t1").unwrap().artifact_ref.as_ref().unwrap();
        assert_eq!(a.commit.as_deref(), Some("b56fefadeadbeef"));
        assert_eq!(a.branch.as_deref(), Some("feat/x"), "existing branch kept");
        assert_eq!(a.pr, Some(42), "existing pr preserved");

        // Explicit branch overrides; pr still preserved.
        plan.set_artifact_commit("t1", "cafef00d", Some("main"));
        let a = plan.subtask("t1").unwrap().artifact_ref.as_ref().unwrap();
        assert_eq!(a.commit.as_deref(), Some("cafef00d"));
        assert_eq!(a.branch.as_deref(), Some("main"));
        assert_eq!(a.pr, Some(42));

        // A bare node gets a fresh ref (just the commit + branch).
        plan.set_artifact_commit("t2", "abc123", Some("main"));
        let a = plan.subtask("t2").unwrap().artifact_ref.as_ref().unwrap();
        assert_eq!(a.commit.as_deref(), Some("abc123"));
        assert_eq!(a.branch.as_deref(), Some("main"));
        assert_eq!(a.pr, None);

        // Absent id is a no-op (no panic, no phantom node).
        plan.set_artifact_commit("nope", "x", None);
        assert!(plan.subtask("nope").is_none());
    }

    #[test]
    fn set_artifact_issue_binds_the_issue_preserving_other_refs() {
        // #1083: an issue ref is an ADDITIONAL evaluator gate; binding it must
        // not disturb branch/commit/pr, must round-trip through TOML, and must
        // no-op an absent id.
        let toml = r#"
[[subtask]]
id = "t1"
instruction = "task 1"
kind = "task"

[subtask.artifact_ref]
pr = 42
branch = "feat/x"
"#;
        let mut plan = Plan::from_toml_str(toml).unwrap();
        plan.set_artifact_issue("t1", 1083);
        let a = plan.subtask("t1").unwrap().artifact_ref.as_ref().unwrap();
        assert_eq!(a.issue, Some(1083));
        assert_eq!(a.pr, Some(42), "existing pr preserved");
        assert_eq!(a.branch.as_deref(), Some("feat/x"), "branch preserved");

        // Round-trips through TOML (and old files without the field parse:
        // this very fixture had none).
        let text = plan.to_toml_string().unwrap();
        assert!(text.contains("issue = 1083"), "{text}");
        let back = Plan::from_toml_str(&text).unwrap();
        assert_eq!(
            back.subtask("t1")
                .unwrap()
                .artifact_ref
                .as_ref()
                .unwrap()
                .issue,
            Some(1083)
        );

        // Absent id is a no-op.
        plan.set_artifact_issue("nope", 1);
        assert!(plan.subtask("nope").is_none());
    }

    #[test]
    fn next_uncaptured_task_under_walks_the_plan_in_order() {
        // #1062: auto-capture picks the leftmost Pending Task under the Plan whose
        // commit is unset — the 1-commit-→-1-Task cursor.
        let toml = r#"
[[subtask]]
id = "ph"
instruction = "phase"
kind = "phase"

[[subtask]]
id = "pl"
instruction = "plan"
kind = "plan"
parent = "ph"

[[subtask]]
id = "t1"
instruction = "task 1"
kind = "task"
parent = "pl"

[[subtask]]
id = "t2"
instruction = "task 2"
kind = "task"
parent = "pl"
"#;
        let mut plan = Plan::from_toml_str(toml).unwrap();
        let id = |o: Option<&Subtask>| o.map(|s| s.id.clone());
        assert_eq!(id(plan.next_uncaptured_task_under("pl")), Some("t1".into()));
        // Capturing t1 advances the cursor to t2.
        plan.set_artifact_commit("t1", "abc", None);
        assert_eq!(id(plan.next_uncaptured_task_under("pl")), Some("t2".into()));
        // Both captured → nothing left.
        plan.set_artifact_commit("t2", "def", None);
        assert_eq!(plan.next_uncaptured_task_under("pl"), None);
        // A non-Plan target yields None (a phase or a task is not a Plan).
        assert!(plan.next_uncaptured_task_under("ph").is_none());
        assert!(plan.next_uncaptured_task_under("t1").is_none());
        assert!(plan.next_uncaptured_task_under("absent").is_none());
    }
}
