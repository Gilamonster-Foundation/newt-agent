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
    NodeFacts {
        commit_present,
        verify,
        children_all_done,
        pr_merged,
        ci_green,
    }
}

/// Apply `node`'s #1030 completion rule to its [`NodeFacts`] (a pure reducer).
#[must_use]
pub fn evaluate_node(node: &Subtask, facts: &NodeFacts) -> NodeVerdict {
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
}
