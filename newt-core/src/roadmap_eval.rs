//! #1030 node evaluators: decide whether a Roadmap→Phase→Plan→Task node is DONE
//! from OBJECTIVE state (git / CI), never the model's self-report.
//!
//! The gathering of facts is behind trait seams ([`GitFacts`], [`VerifyRunner`])
//! so the unit tier is fully mocked (no real git or subprocess); the production
//! implementations live in `newt-tui` (over `newt-git` + a subprocess). The
//! completion rules themselves are a pure reducer ([`evaluate_node`]).
//!
//! Local rules (this module):
//! - **Task** done = its commit is on the branch AND its `verify` gate passes.
//! - **Plan** done = every child Task is Done AND the plan's branch `verify` passes.
//!
//! Remote rules (Phase = PR merged, Roadmap = CI green) need a forge/CI seam and
//! land in PR 8; here they return [`NodeVerdict::Unsupported`] so a node is never
//! *falsely* marked Done (the gates stay honest — CLAUDE.md).

use crate::plan::{NodeKind, Plan, Subtask, SubtaskStatus};

/// Whether a node satisfies its #1030 completion rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeVerdict {
    /// The completion rule is satisfied — the caller may mark the node Done.
    Done,
    /// Not yet complete, with a human-readable reason (missing commit, failing
    /// verify, children not done).
    NotYet(String),
    /// Cannot be evaluated locally — needs forge/CI support (Phase/Roadmap; the
    /// remote evaluators land in PR 8). Never falsely Done.
    Unsupported(String),
}

/// The objective facts an evaluator reduces, gathered via the seams below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeFacts {
    /// The node's `artifact_ref.commit` is present on its branch (Task/Plan).
    pub commit_present: bool,
    /// The node's `verify` command result — `None` when it has no verify command.
    pub verify: Option<bool>,
    /// The node has children and every one is Done (Plan/Phase/Roadmap).
    pub children_all_done: bool,
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

/// Gather the [`NodeFacts`] for `node` in `tree` via the injected seams.
#[must_use]
pub fn gather_facts(
    node: &Subtask,
    tree: &Plan,
    git: &dyn GitFacts,
    verify: &dyn VerifyRunner,
) -> NodeFacts {
    let artifact = node.artifact_ref.as_ref();
    let commit_present = artifact
        .and_then(|a| a.commit.as_deref())
        .map(|c| git.commit_present(c, artifact.and_then(|a| a.branch.as_deref())))
        .unwrap_or(false);
    let verify = node.verify.as_deref().map(|cmd| verify.run(cmd));
    // A node with NO children is NOT "done by vacuity": an empty Plan/Phase has
    // accomplished nothing. The Task rule ignores this field.
    let kids = tree.children(&node.id);
    let children_all_done =
        !kids.is_empty() && kids.iter().all(|c| c.status == SubtaskStatus::Done);
    NodeFacts {
        commit_present,
        verify,
        children_all_done,
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
        NodeKind::Phase => NodeVerdict::Unsupported(
            "phase completion needs a merged PR — remote evaluation lands in PR 8".into(),
        ),
        NodeKind::Roadmap => NodeVerdict::Unsupported(
            "roadmap completion needs green CI — remote evaluation lands in PR 8".into(),
        ),
    }
}

/// Gather facts for `node` and apply its completion rule.
#[must_use]
pub fn evaluate(
    node: &Subtask,
    tree: &Plan,
    git: &dyn GitFacts,
    verify: &dyn VerifyRunner,
) -> NodeVerdict {
    evaluate_node(node, &gather_facts(node, tree, git, verify))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ArtifactRef, Subtask};

    /// A fake GitFacts: a commit is "present" iff it is in the allow-set.
    struct FakeGit(&'static [&'static str]);
    impl GitFacts for FakeGit {
        fn commit_present(&self, commit: &str, _branch: Option<&str>) -> bool {
            self.0.contains(&commit)
        }
    }
    /// A fake VerifyRunner returning a fixed result.
    struct FakeVerify(bool);
    impl VerifyRunner for FakeVerify {
        fn run(&self, _cmd: &str) -> bool {
            self.0
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
        let pass = FakeVerify(true);
        let fail = FakeVerify(false);
        let git = FakeGit(&["deadbeef"]);

        // commit present + verify passes → Done.
        let t = task_with(Some("deadbeef"), Some("cargo test"));
        assert_eq!(evaluate(&t, &tree, &git, &pass), NodeVerdict::Done);
        // commit present, NO verify command → Done (nothing to fail).
        let t = task_with(Some("deadbeef"), None);
        assert_eq!(evaluate(&t, &tree, &git, &pass), NodeVerdict::Done);
        // commit present but verify FAILS → NotYet.
        let t = task_with(Some("deadbeef"), Some("cargo test"));
        assert!(matches!(
            evaluate(&t, &tree, &git, &fail),
            NodeVerdict::NotYet(_)
        ));
        // commit ABSENT → NotYet regardless of verify.
        let t = task_with(Some("nope"), Some("cargo test"));
        assert!(matches!(
            evaluate(&t, &tree, &git, &pass),
            NodeVerdict::NotYet(_)
        ));
        // no artifact commit at all → NotYet.
        let t = task_with(None, None);
        assert!(matches!(
            evaluate(&t, &tree, &git, &pass),
            NodeVerdict::NotYet(_)
        ));
    }

    #[test]
    fn plan_is_done_when_all_child_tasks_are_done_and_verify_passes() {
        // plan-1 with two task children; done-ness depends on the children.
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
        let git = FakeGit(&[]);
        let pass = FakeVerify(true);
        let plan = tree.subtask("plan-1").unwrap().clone();

        // children pending → NotYet.
        assert!(matches!(
            evaluate(&plan, &tree, &git, &pass),
            NodeVerdict::NotYet(_)
        ));
        // both children Done → Done.
        tree.mark("t1", SubtaskStatus::Done, None);
        tree.mark("t2", SubtaskStatus::Done, None);
        assert_eq!(evaluate(&plan, &tree, &git, &pass), NodeVerdict::Done);
        // children done but a failing plan verify → NotYet.
        let mut plan_verify = plan.clone();
        plan_verify.verify = Some("cargo test".into());
        assert!(matches!(
            evaluate(&plan_verify, &tree, &git, &FakeVerify(false)),
            NodeVerdict::NotYet(_)
        ));
    }

    #[test]
    fn phase_and_roadmap_are_unsupported_locally() {
        let tree = Plan::default();
        let git = FakeGit(&[]);
        let v = FakeVerify(true);
        let phase = Subtask::node("ph", "phase", NodeKind::Phase, None);
        let road = Subtask::node("rd", "roadmap", NodeKind::Roadmap, None);
        assert!(matches!(
            evaluate(&phase, &tree, &git, &v),
            NodeVerdict::Unsupported(_)
        ));
        assert!(matches!(
            evaluate(&road, &tree, &git, &v),
            NodeVerdict::Unsupported(_)
        ));
    }

    #[test]
    fn empty_plan_is_not_done_by_vacuity() {
        // A Plan with no children has accomplished nothing → NotYet, never Done.
        let plan = Subtask::node("p", "empty plan", NodeKind::Plan, None);
        let tree = Plan {
            subtasks: vec![plan.clone()],
            ..Plan::default()
        };
        assert!(matches!(
            evaluate(&plan, &tree, &FakeGit(&[]), &FakeVerify(true)),
            NodeVerdict::NotYet(_)
        ));
    }
}
