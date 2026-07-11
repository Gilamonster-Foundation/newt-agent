//! #1030 node evaluators: decide whether a Roadmap→Phase→Plan→Task node is DONE
//! from OBJECTIVE state (git / forge / CI), never the model's self-report.
//!
//! Fact-gathering is behind trait seams bundled in [`Facts`] so the unit tier is
//! fully mocked (no real git, subprocess, `gh`, or CI). The completion rules are
//! a pure reducer ([`evaluate_node`]).
//!
//! Completion rules (#1030):
//! - **Task** done = its commit is on the branch AND its `verify` gate passes.
//! - **Plan** done = every child Task is Done AND the plan's branch `verify` passes.
//! - **Phase** done = every child Plan is Done AND its PR is merged to main.
//! - **Roadmap** done = every child Phase is Done AND the pipelines are green.
//!
//! When a required remote fact is unavailable (no `gh`, no CI), the verdict is
//! [`NodeVerdict::Unsupported`] — a node is never *falsely* Done (CLAUDE.md).

use crate::plan::{NodeKind, Plan, Subtask, SubtaskStatus};

/// Whether a node satisfies its #1030 completion rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeVerdict {
    /// The completion rule is satisfied — the caller may mark the node Done.
    Done,
    /// Not yet complete, with a human-readable reason (missing commit, failing
    /// verify, children not done, PR not merged, CI red).
    NotYet(String),
    /// Cannot be evaluated — a required objective fact is unavailable (no `gh`,
    /// no CI, no artifact reference). Never falsely Done.
    Unsupported(String),
}

/// The objective facts an evaluator reduces, gathered via the [`Facts`] seams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeFacts {
    /// The node's `artifact_ref.commit` is present on its branch (Task/Plan).
    pub commit_present: bool,
    /// The node's `verify` command result — `None` when it has no verify command.
    pub verify: Option<bool>,
    /// The node has children and every one is Done (Plan/Phase/Roadmap).
    pub children_all_done: bool,
    /// The node's `artifact_ref.pr` merge state (Phase): `Some(true)` merged,
    /// `Some(false)` open/unmerged, `None` = no PR reference or no forge access.
    pub pr_merged: Option<bool>,
    /// The pipelines' green state (Roadmap): `Some(true)` green, `Some(false)`
    /// red, `None` = no CI access.
    pub ci_green: Option<bool>,
    /// The referenced forge issue's state (#1083, any node kind): `None` = no
    /// `artifact_ref.issue` reference — the issue gate does not apply.
    pub issue: Option<IssueState>,
}

/// The state of a node's referenced forge issue (#1083). An additional gate on
/// the node's completion rule: an [`Open`](IssueState::Open) issue blocks Done,
/// an [`Unknown`](IssueState::Unknown) one is Unsupported — a verdict input,
/// never a direct Done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    /// The forge reports the issue CLOSED.
    Closed,
    /// The forge reports the issue still open.
    Open,
    /// The issue is referenced but the forge cannot answer (no `gh`, no remote).
    Unknown,
}

/// Read objective git state. Mocked in the unit tier; the real impl (newt-tui)
/// reads the repo via `newt-git`.
pub trait GitFacts {
    /// Is `commit` present on `branch` (or the current branch when `None`)?
    fn commit_present(&self, commit: &str, branch: Option<&str>) -> bool;
}

/// Run a node's verify command, returning pass/fail. Mocked in the unit tier;
/// the real impl (newt-tui) runs a subprocess.
pub trait VerifyRunner {
    fn run(&self, cmd: &str) -> bool;
}

/// Read forge (pull-request) state. Mocked in the unit tier; the real impl
/// (newt-tui) shells out to `gh`. `None` when the forge cannot be reached.
pub trait ForgeFacts {
    /// Is pull-request `pr` merged? `None` when it cannot be determined.
    fn pr_merged(&self, pr: u64) -> Option<bool>;
    /// Is forge issue `issue` closed (#1083)? `None` when it cannot be
    /// determined. Default `None` keeps the trait additive: an impl that
    /// doesn't override degrades to Unsupported, never a false answer.
    fn issue_closed(&self, issue: u64) -> Option<bool> {
        let _ = issue;
        None
    }
}

/// Read CI/pipeline state. Mocked in the unit tier; the real impl (newt-tui)
/// shells out to `gh`. `None` when CI cannot be reached.
pub trait CiFacts {
    /// Are the pipelines green? `None` when it cannot be determined.
    fn pipelines_green(&self) -> Option<bool>;
}

/// The injected fact-gathering seams for evaluation, bundled so the evaluator
/// signature stays small as objective sources (git → forge → CI) are added.
pub struct Facts<'a> {
    pub git: &'a dyn GitFacts,
    pub verify: &'a dyn VerifyRunner,
    pub forge: &'a dyn ForgeFacts,
    pub ci: &'a dyn CiFacts,
}

/// Gather the [`NodeFacts`] for `node` in `tree` via the injected seams.
#[must_use]
pub fn gather_facts(node: &Subtask, tree: &Plan, facts: &Facts) -> NodeFacts {
    let artifact = node.artifact_ref.as_ref();
    let commit_present = artifact
        .and_then(|a| a.commit.as_deref())
        .map(|c| {
            facts
                .git
                .commit_present(c, artifact.and_then(|a| a.branch.as_deref()))
        })
        .unwrap_or(false);
    let verify = node.verify.as_deref().map(|cmd| facts.verify.run(cmd));
    // A node with NO children is NOT "done by vacuity": an empty Plan/Phase has
    // accomplished nothing. The Task rule ignores this field.
    let kids = tree.children(&node.id);
    let children_all_done =
        !kids.is_empty() && kids.iter().all(|c| c.status == SubtaskStatus::Done);
    // Remote facts are only gathered where they gate: a PR ref for a Phase, CI
    // for a Roadmap. `None` = the source could not answer (no ref / no access).
    let pr_merged = artifact
        .and_then(|a| a.pr)
        .and_then(|pr| facts.forge.pr_merged(pr));
    let ci_green = if node.kind == NodeKind::Roadmap {
        facts.ci.pipelines_green()
    } else {
        None
    };
    // #1083: the issue gate applies to any node that carries a reference —
    // "no answer from the forge" is distinct from "no reference".
    let issue = artifact
        .and_then(|a| a.issue)
        .map(|n| match facts.forge.issue_closed(n) {
            Some(true) => IssueState::Closed,
            Some(false) => IssueState::Open,
            None => IssueState::Unknown,
        });
    NodeFacts {
        commit_present,
        verify,
        children_all_done,
        pr_merged,
        ci_green,
        issue,
    }
}

/// Apply `node`'s #1030 completion rule to its [`NodeFacts`] (a pure reducer).
#[must_use]
pub fn evaluate_node(node: &Subtask, facts: &NodeFacts) -> NodeVerdict {
    // #1083: the issue gate is common to every kind — a referenced issue must
    // be CLOSED before any other rule may conclude Done. It only ever blocks;
    // a closed issue proves nothing by itself.
    match facts.issue {
        Some(IssueState::Open) => {
            return NodeVerdict::NotYet("the referenced issue is still open".into());
        }
        Some(IssueState::Unknown) => {
            return NodeVerdict::Unsupported(
                "the node references an issue but its state is unavailable — ensure `gh` \
                 can reach the forge"
                    .into(),
            );
        }
        Some(IssueState::Closed) | None => {}
    }
    // A verify command that ran and FAILED blocks; absent (`None`) or passing is fine.
    let verify_ok = facts.verify != Some(false);
    match node.kind {
        NodeKind::Task => {
            if !facts.commit_present {
                NodeVerdict::NotYet("no commit on the branch yet (set artifact_ref.commit)".into())
            } else if !verify_ok {
                NodeVerdict::NotYet("the task's verify command is failing".into())
            } else {
                NodeVerdict::Done
            }
        }
        NodeKind::Plan => {
            if !facts.children_all_done {
                NodeVerdict::NotYet("not all child tasks are done".into())
            } else if !verify_ok {
                NodeVerdict::NotYet("the plan's branch verify is failing".into())
            } else {
                NodeVerdict::Done
            }
        }
        NodeKind::Phase => {
            if !facts.children_all_done {
                NodeVerdict::NotYet("not all child plans are done".into())
            } else {
                match facts.pr_merged {
                    Some(true) => NodeVerdict::Done,
                    Some(false) => NodeVerdict::NotYet("the phase's PR is not merged yet".into()),
                    None => NodeVerdict::Unsupported(
                        "phase completion needs a merged PR — set artifact_ref.pr and ensure `gh` is available".into(),
                    ),
                }
            }
        }
        NodeKind::Roadmap => {
            if !facts.children_all_done {
                NodeVerdict::NotYet("not all child phases are done".into())
            } else {
                match facts.ci_green {
                    Some(true) => NodeVerdict::Done,
                    Some(false) => NodeVerdict::NotYet("the pipelines are not green".into()),
                    None => NodeVerdict::Unsupported(
                        "roadmap completion needs green CI — no pipeline status available".into(),
                    ),
                }
            }
        }
    }
}

/// Gather facts for `node` and apply its completion rule.
#[must_use]
pub fn evaluate(node: &Subtask, tree: &Plan, facts: &Facts) -> NodeVerdict {
    evaluate_node(node, &gather_facts(node, tree, facts))
}

/// What one headless traversal step did at the cursor node.
///
/// The driver only *closes* nodes whose OBJECTIVE evaluator already passes — it
/// never runs a model. A [`Blocked`](DriveStep::Blocked) step is exactly where a
/// real driver (interactive or wyvern) would dispatch work on the node before
/// trying again; [`Complete`](DriveStep::Complete) means the tree is finished
/// (or every remaining node is blocked by a dep / open child).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveStep {
    /// The cursor node evaluated [`NodeVerdict::Done`] and was marked Done.
    Advanced { node: String },
    /// The cursor node is not (yet) completable — traversal halts honestly here.
    /// `reason` carries the evaluator's `NotYet`/`Unsupported` message.
    Blocked { node: String, reason: String },
    /// No node is ready — the tree is complete, or nothing more can advance.
    Complete,
}

/// The #1030 headless traversal cursor: the leftmost **not-yet-closed** node
/// (`Pending` OR `Running`) whose deps AND children are all `Done` — the next
/// node `drive` can evaluate to close.
///
/// Unlike the interactive [`roadmap_cursor`](../../newt_tui) this is deliberately
/// NOT "Running-first" (#1080): a bound Plan is `Running`, and Running-first made
/// `drive` evaluate the *Plan* (→ "children not done" → Blocked) and never
/// descend to its ready child Task — so a bound Plan's Tasks (even with commits)
/// never closed. Descending instead evaluates the ready Task first (closing it
/// from git), and — because a `Running` node is *also* eligible here (whereas
/// [`Plan::next_ready_node`] only returns `Pending`) — the bound Plan itself
/// closes once its children are `Done`. `Failed` nodes are excluded: they block
/// honestly (their dependents' `Done` deps stay unmet). The interactive cursor
/// keeps Running-first — staying on the bound node is right for `/roadmap next`.
#[must_use]
pub fn drive_cursor(tree: &Plan) -> Option<&Subtask> {
    tree.subtasks.iter().find(|s| {
        matches!(s.status, SubtaskStatus::Pending | SubtaskStatus::Running)
            && s.deps
                .iter()
                .all(|d| matches!(tree.subtask(d).map(|t| t.status), Some(SubtaskStatus::Done)))
            && tree
                .children(&s.id)
                .iter()
                .all(|c| c.status == SubtaskStatus::Done)
    })
}

/// Evaluate the cursor node once and, if it is [`NodeVerdict::Done`], mark it —
/// the headless analogue of `/roadmap eval` on the cursor. This is the pure,
/// fully-mockable core of the wyvern tree driver: it does NOT run a model or
/// touch the network directly; every objective fact arrives through [`Facts`].
///
/// - cursor Done → [`DriveStep::Advanced`] (node marked Done, so the next call
///   sees the next-ready node and completion ripples up the tree).
/// - cursor `NotYet`/`Unsupported` → [`DriveStep::Blocked`] (the reason names
///   what a real driver must make true — a commit, a passing verify, a merged
///   PR, green CI).
/// - no cursor → [`DriveStep::Complete`].
pub fn drive_once(tree: &mut Plan, facts: &Facts) -> DriveStep {
    let Some(node_id) = drive_cursor(tree).map(|s| s.id.clone()) else {
        return DriveStep::Complete;
    };
    // The cursor was just read from `tree`, so the node exists.
    let node = tree
        .subtask(&node_id)
        .cloned()
        .expect("cursor node exists in the tree");
    match evaluate(&node, tree, facts) {
        NodeVerdict::Done => {
            tree.mark(&node_id, SubtaskStatus::Done, None);
            DriveStep::Advanced { node: node_id }
        }
        NodeVerdict::NotYet(reason) | NodeVerdict::Unsupported(reason) => DriveStep::Blocked {
            node: node_id,
            reason,
        },
    }
}

/// Drive the tree to a fixpoint: repeatedly [`drive_once`] until it stops
/// advancing. Because [`drive_once`] closes the leftmost-ready node and marking
/// it Done can make its parent ready, one call ripples completion as far *up*
/// the tree as the objective facts allow, halting honestly at the first node
/// that still needs work (`Blocked`) or when the tree is finished (`Complete`).
///
/// The loop is bounded by `subtasks.len() + 1` — each `Advanced` closes a
/// distinct node, so no run can advance more times than there are nodes; the
/// bound is a hard backstop against a pathological cursor that never converges.
/// The returned log ends in exactly one non-`Advanced` step (`Blocked` or
/// `Complete`) on any well-formed tree.
pub fn drive_to_fixpoint(tree: &mut Plan, facts: &Facts) -> Vec<DriveStep> {
    let bound = tree.subtasks.len() + 1;
    let mut steps = Vec::new();
    for _ in 0..bound {
        let step = drive_once(tree, facts);
        let advanced = matches!(step, DriveStep::Advanced { .. });
        steps.push(step);
        if !advanced {
            break;
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ArtifactRef, Subtask};

    struct FakeGit(&'static [&'static str]);
    impl GitFacts for FakeGit {
        fn commit_present(&self, commit: &str, _branch: Option<&str>) -> bool {
            self.0.contains(&commit)
        }
    }
    struct FakeVerify(bool);
    impl VerifyRunner for FakeVerify {
        fn run(&self, _cmd: &str) -> bool {
            self.0
        }
    }
    /// Forge fake: a PR merge-state map (pr -> merged); absent pr -> None.
    struct FakeForge(Option<bool>);
    impl ForgeFacts for FakeForge {
        fn pr_merged(&self, _pr: u64) -> Option<bool> {
            self.0
        }
    }
    struct FakeCi(Option<bool>);
    impl CiFacts for FakeCi {
        fn pipelines_green(&self) -> Option<bool> {
            self.0
        }
    }

    /// Build a Facts bundle from the fakes.
    fn facts<'a>(
        git: &'a FakeGit,
        verify: &'a FakeVerify,
        forge: &'a FakeForge,
        ci: &'a FakeCi,
    ) -> Facts<'a> {
        Facts {
            git,
            verify,
            forge,
            ci,
        }
    }

    fn task_with(commit: Option<&str>, verify: Option<&str>) -> Subtask {
        let mut t = Subtask::node("t", "the task", NodeKind::Task, None);
        t.artifact_ref = commit.map(|c| ArtifactRef {
            branch: Some("feat/x".into()),
            commit: Some(c.into()),
            pr: None,
            issue: None,
        });
        t.verify = verify.map(str::to_string);
        t
    }

    #[test]
    fn task_is_done_only_with_a_present_commit_and_passing_verify() {
        let tree = Plan::default();
        let (git, pass, fail, forge, ci) = (
            FakeGit(&["deadbeef"]),
            FakeVerify(true),
            FakeVerify(false),
            FakeForge(None),
            FakeCi(None),
        );
        let ok = facts(&git, &pass, &forge, &ci);
        let bad = facts(&git, &fail, &forge, &ci);

        assert_eq!(
            evaluate(&task_with(Some("deadbeef"), Some("cargo test")), &tree, &ok),
            NodeVerdict::Done
        );
        assert_eq!(
            evaluate(&task_with(Some("deadbeef"), None), &tree, &ok),
            NodeVerdict::Done
        );
        assert!(matches!(
            evaluate(
                &task_with(Some("deadbeef"), Some("cargo test")),
                &tree,
                &bad
            ),
            NodeVerdict::NotYet(_)
        ));
        assert!(matches!(
            evaluate(&task_with(Some("nope"), None), &tree, &ok),
            NodeVerdict::NotYet(_)
        ));
        assert!(matches!(
            evaluate(&task_with(None, None), &tree, &ok),
            NodeVerdict::NotYet(_)
        ));
    }

    // ── #1083: the issue gate ────────────────────────────────────────────────

    /// Forge fake answering BOTH seams: `.0` = pr_merged, `.1` = issue_closed.
    struct FakeIssueForge(Option<bool>, Option<bool>);
    impl ForgeFacts for FakeIssueForge {
        fn pr_merged(&self, _pr: u64) -> Option<bool> {
            self.0
        }
        fn issue_closed(&self, _issue: u64) -> Option<bool> {
            self.1
        }
    }

    /// An otherwise-Done Task (commit present, no verify) referencing `issue`.
    fn done_task_with_issue(issue: u64) -> Subtask {
        let mut t = task_with(Some("deadbeef"), None);
        t.artifact_ref.as_mut().unwrap().issue = Some(issue);
        t
    }

    #[test]
    fn issue_gate_blocks_done_while_the_referenced_issue_is_open() {
        let tree = Plan::default();
        let (git, verify, ci) = (FakeGit(&["deadbeef"]), FakeVerify(true), FakeCi(None));
        let open = FakeIssueForge(None, Some(false));
        let closed = FakeIssueForge(None, Some(true));
        let t = done_task_with_issue(1083);

        let verdict = evaluate(
            &t,
            &tree,
            &Facts {
                git: &git,
                verify: &verify,
                forge: &open,
                ci: &ci,
            },
        );
        assert!(
            matches!(&verdict, NodeVerdict::NotYet(r) if r.contains("issue")),
            "open issue must block: {verdict:?}"
        );
        assert_eq!(
            evaluate(
                &t,
                &tree,
                &Facts {
                    git: &git,
                    verify: &verify,
                    forge: &closed,
                    ci: &ci,
                }
            ),
            NodeVerdict::Done
        );
    }

    #[test]
    fn issue_gate_unknown_state_is_unsupported_never_done() {
        // The default trait method answers None — an impl that never learned
        // about issues degrades to Unsupported, not a false Done (#1083).
        let tree = Plan::default();
        let (git, verify, ci) = (FakeGit(&["deadbeef"]), FakeVerify(true), FakeCi(None));
        let legacy = FakeForge(None); // no issue_closed override
        let verdict = evaluate(
            &done_task_with_issue(1083),
            &tree,
            &Facts {
                git: &git,
                verify: &verify,
                forge: &legacy,
                ci: &ci,
            },
        );
        assert!(
            matches!(&verdict, NodeVerdict::Unsupported(r) if r.contains("issue")),
            "unknown issue state must be Unsupported: {verdict:?}"
        );
    }

    #[test]
    fn nodes_without_an_issue_ref_ignore_the_issue_gate() {
        // A forge screaming "open!" must not gate a node with no reference.
        let tree = Plan::default();
        let (git, verify, ci) = (FakeGit(&["deadbeef"]), FakeVerify(true), FakeCi(None));
        let open = FakeIssueForge(None, Some(false));
        assert_eq!(
            evaluate(
                &task_with(Some("deadbeef"), None),
                &tree,
                &Facts {
                    git: &git,
                    verify: &verify,
                    forge: &open,
                    ci: &ci,
                }
            ),
            NodeVerdict::Done
        );
    }

    #[test]
    fn plan_is_done_when_all_child_tasks_are_done_and_verify_passes() {
        let toml = r#"
[[subtask]]
id = "plan-1"
instruction = "the plan"
kind = "plan"

[[subtask]]
id = "t1"
instruction = "task 1"
kind = "task"
parent = "plan-1"

[[subtask]]
id = "t2"
instruction = "task 2"
kind = "task"
parent = "plan-1"
"#;
        let mut tree = Plan::from_toml_str(toml).unwrap();
        let (git, pass, forge, ci) = (
            FakeGit(&[]),
            FakeVerify(true),
            FakeForge(None),
            FakeCi(None),
        );
        let ok = facts(&git, &pass, &forge, &ci);
        let plan = tree.subtask("plan-1").unwrap().clone();

        assert!(matches!(
            evaluate(&plan, &tree, &ok),
            NodeVerdict::NotYet(_)
        ));
        tree.mark("t1", SubtaskStatus::Done, None);
        tree.mark("t2", SubtaskStatus::Done, None);
        assert_eq!(evaluate(&plan, &tree, &ok), NodeVerdict::Done);
    }

    #[test]
    fn phase_is_done_when_children_done_and_pr_merged() {
        // ph with one Plan child; the phase carries a PR reference.
        let toml = r#"
[[subtask]]
id = "ph"
instruction = "phase one"
kind = "phase"

[[subtask]]
id = "plan-1"
instruction = "a plan"
kind = "plan"
parent = "ph"

[subtask.artifact_ref]
pr = 42
"#;
        // ArtifactRef attaches to the LAST scalar-preceding subtask (plan-1), not
        // ph — so build the phase with an explicit artifact_ref instead.
        let mut tree = Plan::from_toml_str(toml).unwrap();
        // Give the phase (not the plan) the PR ref.
        if let Some(phase) = tree.subtasks.iter_mut().find(|s| s.id == "ph") {
            phase.artifact_ref = Some(ArtifactRef {
                branch: None,
                commit: None,
                pr: Some(42),
                issue: None,
            });
        }
        let git = FakeGit(&[]);
        let verify = FakeVerify(true);
        let ci = FakeCi(None);
        let phase = tree.subtask("ph").unwrap().clone();

        // child plan pending → NotYet regardless of PR.
        let merged = facts(&git, &verify, &FakeForge(Some(true)), &ci);
        assert!(matches!(
            evaluate(&phase, &tree, &merged),
            NodeVerdict::NotYet(_)
        ));
        // child done + PR merged → Done.
        tree.mark("plan-1", SubtaskStatus::Done, None);
        assert_eq!(evaluate(&phase, &tree, &merged), NodeVerdict::Done);
        // child done + PR NOT merged → NotYet.
        let open = facts(&git, &verify, &FakeForge(Some(false)), &ci);
        assert!(matches!(
            evaluate(&phase, &tree, &open),
            NodeVerdict::NotYet(_)
        ));
        // child done but no forge access (or no PR ref) → Unsupported.
        let no_forge = facts(&git, &verify, &FakeForge(None), &ci);
        assert!(matches!(
            evaluate(&phase, &tree, &no_forge),
            NodeVerdict::Unsupported(_)
        ));
    }

    #[test]
    fn roadmap_is_done_when_children_done_and_ci_green() {
        let toml = r#"
[[subtask]]
id = "rd"
instruction = "the roadmap"
kind = "roadmap"

[[subtask]]
id = "ph"
instruction = "a phase"
kind = "phase"
parent = "rd"
"#;
        let mut tree = Plan::from_toml_str(toml).unwrap();
        let (git, verify, forge) = (FakeGit(&[]), FakeVerify(true), FakeForge(None));
        let road = tree.subtask("rd").unwrap().clone();

        // child phase pending → NotYet.
        let green = facts(&git, &verify, &forge, &FakeCi(Some(true)));
        assert!(matches!(
            evaluate(&road, &tree, &green),
            NodeVerdict::NotYet(_)
        ));
        // child done + CI green → Done.
        tree.mark("ph", SubtaskStatus::Done, None);
        assert_eq!(evaluate(&road, &tree, &green), NodeVerdict::Done);
        // child done + CI red → NotYet.
        let red = facts(&git, &verify, &forge, &FakeCi(Some(false)));
        assert!(matches!(
            evaluate(&road, &tree, &red),
            NodeVerdict::NotYet(_)
        ));
        // child done + no CI access → Unsupported.
        let no_ci = facts(&git, &verify, &forge, &FakeCi(None));
        assert!(matches!(
            evaluate(&road, &tree, &no_ci),
            NodeVerdict::Unsupported(_)
        ));
    }

    #[test]
    fn empty_plan_is_not_done_by_vacuity() {
        let plan = Subtask::node("p", "empty plan", NodeKind::Plan, None);
        let tree = Plan {
            subtasks: vec![plan.clone()],
            ..Plan::default()
        };
        let (git, verify, forge, ci) = (
            FakeGit(&[]),
            FakeVerify(true),
            FakeForge(None),
            FakeCi(None),
        );
        assert!(matches!(
            evaluate(&plan, &tree, &facts(&git, &verify, &forge, &ci)),
            NodeVerdict::NotYet(_)
        ));
    }

    /// A Phase→Plan→{Task,Task} tree whose commits, verify, and PR are all
    /// objectively satisfied drives to completion in one fixpoint call: the two
    /// Tasks close, which makes the Plan ready (children done + verify), which
    /// makes the Phase ready (children done + PR merged) — completion ripples
    /// all the way up, then `Complete`.
    fn cascade_tree() -> Plan {
        let toml = r#"
[[subtask]]
id = "ph"
instruction = "phase one"
kind = "phase"

[[subtask]]
id = "plan-1"
instruction = "a plan"
kind = "plan"
parent = "ph"

[[subtask]]
id = "t1"
instruction = "task 1"
kind = "task"
parent = "plan-1"

[[subtask]]
id = "t2"
instruction = "task 2"
kind = "task"
parent = "plan-1"
"#;
        let mut tree = Plan::from_toml_str(toml).unwrap();
        for (id, commit) in [("t1", "c1"), ("t2", "c2")] {
            if let Some(t) = tree.subtasks.iter_mut().find(|s| s.id == id) {
                t.artifact_ref = Some(ArtifactRef {
                    branch: Some("feat/x".into()),
                    commit: Some(commit.into()),
                    pr: None,
                    issue: None,
                });
            }
        }
        if let Some(ph) = tree.subtasks.iter_mut().find(|s| s.id == "ph") {
            ph.artifact_ref = Some(ArtifactRef {
                branch: None,
                commit: None,
                pr: Some(42),
                issue: None,
            });
        }
        tree
    }

    #[test]
    fn drive_once_advances_a_completable_task_and_marks_it_done() {
        let mut tree = cascade_tree();
        let (git, verify, forge, ci) = (
            FakeGit(&["c1", "c2"]),
            FakeVerify(true),
            FakeForge(Some(true)),
            FakeCi(None),
        );
        // The cursor is the leftmost ready node — task t1 (a leaf whose deps and
        // children are clear).
        let step = drive_once(&mut tree, &facts(&git, &verify, &forge, &ci));
        assert_eq!(
            step,
            DriveStep::Advanced {
                node: "t1".to_string()
            }
        );
        assert_eq!(tree.subtask("t1").unwrap().status, SubtaskStatus::Done);
    }

    #[test]
    fn drive_once_blocks_on_a_task_without_a_commit() {
        let mut tree = cascade_tree();
        // No commits present → the cursor task cannot close.
        let (git, verify, forge, ci) = (
            FakeGit(&[]),
            FakeVerify(true),
            FakeForge(None),
            FakeCi(None),
        );
        let step = drive_once(&mut tree, &facts(&git, &verify, &forge, &ci));
        match step {
            DriveStep::Blocked { node, reason } => {
                assert_eq!(node, "t1");
                assert!(reason.contains("commit"), "reason was: {reason}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        // Blocked never mutates status.
        assert_eq!(tree.subtask("t1").unwrap().status, SubtaskStatus::Pending);
    }

    #[test]
    fn drive_to_fixpoint_cascades_completion_up_the_tree() {
        let mut tree = cascade_tree();
        let (git, verify, forge, ci) = (
            FakeGit(&["c1", "c2"]),
            FakeVerify(true),
            FakeForge(Some(true)),
            FakeCi(None),
        );
        let steps = drive_to_fixpoint(&mut tree, &facts(&git, &verify, &forge, &ci));
        // t1, t2, plan-1, ph advance in DFS order, then Complete.
        assert_eq!(
            steps,
            vec![
                DriveStep::Advanced { node: "t1".into() },
                DriveStep::Advanced { node: "t2".into() },
                DriveStep::Advanced {
                    node: "plan-1".into()
                },
                DriveStep::Advanced { node: "ph".into() },
                DriveStep::Complete,
            ]
        );
        assert!(tree
            .subtasks
            .iter()
            .all(|s| s.status == SubtaskStatus::Done));
    }

    #[test]
    fn drive_to_fixpoint_halts_at_the_first_blocked_node() {
        let mut tree = cascade_tree();
        // t1's commit is present but t2's is not: t1 closes, then t2 blocks and
        // the run stops honestly (the Plan/Phase above never falsely close).
        let (git, verify, forge, ci) = (
            FakeGit(&["c1"]),
            FakeVerify(true),
            FakeForge(Some(true)),
            FakeCi(None),
        );
        let steps = drive_to_fixpoint(&mut tree, &facts(&git, &verify, &forge, &ci));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], DriveStep::Advanced { node: "t1".into() });
        assert!(matches!(&steps[1], DriveStep::Blocked { node, .. } if node == "t2"));
        assert_eq!(
            tree.subtask("plan-1").unwrap().status,
            SubtaskStatus::Pending
        );
        assert_eq!(tree.subtask("ph").unwrap().status, SubtaskStatus::Pending);
    }

    #[test]
    fn drive_to_fixpoint_on_an_empty_tree_is_a_single_complete_step() {
        let mut tree = Plan::default();
        let (git, verify, forge, ci) = (
            FakeGit(&[]),
            FakeVerify(true),
            FakeForge(None),
            FakeCi(None),
        );
        let steps = drive_to_fixpoint(&mut tree, &facts(&git, &verify, &forge, &ci));
        assert_eq!(steps, vec![DriveStep::Complete]);
    }

    #[test]
    fn drive_cursor_descends_past_a_running_plan_then_closes_it() {
        // #1080: a bound Plan is Running. The headless cursor must DESCEND to its
        // ready Task (not block on the Plan) — and once the Tasks are Done, the
        // Running Plan itself becomes the cursor so drive can close it (a Running
        // node is eligible here, unlike next_ready_node which returns only Pending).
        let mut tree = cascade_tree(); // ph → plan-1 → {t1, t2}
        tree.mark("plan-1", SubtaskStatus::Running, None); // bind the Plan
        assert_eq!(
            drive_cursor(&tree).map(|s| s.id.as_str()),
            Some("t1"),
            "descend past the Running plan-1 to the leftmost ready Task"
        );
        // With both Tasks Done, the Running Plan becomes the cursor.
        tree.mark("t1", SubtaskStatus::Done, None);
        tree.mark("t2", SubtaskStatus::Done, None);
        assert_eq!(
            drive_cursor(&tree).map(|s| s.id.as_str()),
            Some("plan-1"),
            "a Running Plan with Done children is the cursor so drive closes it"
        );
    }

    #[test]
    fn drive_to_fixpoint_closes_a_bound_running_plan_and_its_tasks() {
        // #1080 end-to-end regression: bind the Plan (Running), its Tasks carry
        // commits + verify passes + the Phase PR is merged → drive closes
        // Task→Task→Plan→Phase unattended (the old Running-first cursor stalled at
        // the Plan and drove 0 nodes).
        let mut tree = cascade_tree();
        tree.mark("plan-1", SubtaskStatus::Running, None); // the bound Plan
        let (git, verify, forge, ci) = (
            FakeGit(&["c1", "c2"]),
            FakeVerify(true),
            FakeForge(Some(true)),
            FakeCi(None),
        );
        let steps = drive_to_fixpoint(&mut tree, &facts(&git, &verify, &forge, &ci));
        assert_eq!(
            steps,
            vec![
                DriveStep::Advanced { node: "t1".into() },
                DriveStep::Advanced { node: "t2".into() },
                DriveStep::Advanced {
                    node: "plan-1".into()
                },
                DriveStep::Advanced { node: "ph".into() },
                DriveStep::Complete,
            ],
            "the bound Running Plan and its Tasks close from git truth, unattended"
        );
        assert!(tree
            .subtasks
            .iter()
            .all(|s| s.status == SubtaskStatus::Done));
    }
}
