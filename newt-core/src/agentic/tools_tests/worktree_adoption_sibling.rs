//! #2757: ground destination approvals in real confined Git with bounded reads.
use super::*;
use crate::agentic::permissions::widen_caveats;
use crate::worktree_adoption::WorktreeSession;
use crate::{ExecOutcome, Scope};

#[derive(Default)]
struct AllowOnce(
    Vec<PermissionRequest>,
    Vec<(DenialKind, String)>,
    std::collections::BTreeSet<(DenialKind, String)>,
    bool,
);
impl PermissionGate for AllowOnce {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        panic!("approval must preserve the caller baseline")
    }
    fn ask_with_caveats(
        &mut self,
        base: &Caveats,
        requests: &[PermissionRequest],
    ) -> PermissionDecision {
        if self.3
            && requests
                .iter()
                .any(|r| !self.2.contains(&(r.kind, r.target.clone())))
        {
            return PermissionDecision::Deny;
        }
        self.3 = true;
        self.0.extend_from_slice(requests);
        let grants: Vec<_> = requests
            .iter()
            .map(|r| (r.kind, r.target.clone()))
            .collect();
        self.2.extend(grants.iter().cloned());
        PermissionDecision::Allow(widen_caveats(base, &grants))
    }
    fn consume_pending_once(&mut self, kind: DenialKind, target: &str) {
        self.1.push((kind, target.into()));
        self.2.remove(&(kind, target.into()));
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        HumanQuestionOutcome::Unavailable
    }
}

#[derive(Default)]
struct Notices(Vec<String>);
impl ToolPresentation for Notices {
    fn preview(&mut self, text: &str, _: usize) {
        self.0.push(text.into());
    }
    fn document(&mut self, _: &str) {}
    fn override_result(&mut self, _: String) {}
}

struct ReadOnly;
impl PermissionGate for ReadOnly {
    fn refresh_caveats(&mut self, base: &Caveats) -> PermissionDecision {
        PermissionDecision::Allow(Caveats {
            fs_write: Scope::none(),
            ..base.clone()
        })
    }
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        PermissionDecision::Deny
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        HumanQuestionOutcome::Unavailable
    }
}

/// #2757: the old parent write-only prompt lets Git mkdir but denies opening
/// the new .git file. An approved task destination must also remain usable
/// after the one creation call and a Plan/approval cycle, without opening any
/// neighboring checkout or restoring writes to the original checkout.
#[tokio::test]
async fn plan_release_preserves_worktree_adoption() {
    let _env = crate::process_env::lock();
    let _engine =
        crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    assert!(crate::confined_exec::kernel_fs_fence_available());
    {
        let relative = "task";
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("original");
        let target = temp.path().join(relative);
        let other = temp.path().join("neighbor");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(root.join("seed"), "original contents").unwrap();
        std::fs::write(other.join("secret"), "neighbor contents").unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["add", "seed"],
            vec!["-c", "commit.gpgsign=false", "commit", "-qm", "seed"],
        ] {
            let output =
                crate::agentic::tools::tests::git_shell_grant::hermetic_git(&root, temp.path())
                    .args(args)
                    .output()
                    .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut base = Caveats::top();
        base.net = Scope::none();
        crate::caveats::lock_fs_to_workspace(&mut base, root.to_str().unwrap(), &[], &[]);
        let session = WorktreeSession::default();
        for refuse_exec in [false, true] {
            let mut failed_gate = AllowOnce::default();
            let mut failed_base = base.clone();
            if refuse_exec {
                failed_base.exec = Scope::none();
            }
            let failed_target = temp.path().join("failed-task");
            let command = if refuse_exec {
                "git worktree add -b failed ../failed-task"
            } else {
                "git worktree add -b failed ../failed-task nonexistent-start"
            };
            let mut notices = Notices::default();
            let outcome = std::sync::OnceLock::new();
            let text = execute(
                &mut notices,
                "run_command",
                &serde_json::json!({"command":command}),
                root.to_str().unwrap(),
                false,
                20,
                &failed_base,
                &mut crate::agentic::NoMcp,
                ToolCollaborators {
                    worktree_session: Some(&session),
                    permission_gate: Some(&mut failed_gate),
                    execution: Some(&outcome),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
            )
            .await;
            let outcome = outcome.get().copied();
            assert!(
                notices.0.iter().any(|n| n
                    == &format!(
                        "left empty directory `{}`; remove it if unwanted",
                        failed_target.display()
                    )),
                "operator missed notice: {:?}",
                notices.0
            );
            assert_ne!(
                outcome,
                Some(ExecOutcome::Passed),
                "failure control: {text}"
            );
            assert!(
                failed_target.is_dir(),
                "failure must retain the empty leaf: {text}"
            );
            assert!(
                text.contains(&format!(
                    "left empty directory `{}`; remove it if unwanted",
                    failed_target.display()
                )),
                "missing leftover notice: {text}"
            );
            assert_eq!(std::fs::read_dir(&failed_target).unwrap().count(), 0);
            assert!(failed_gate.2.is_empty());
            assert!(session.snapshot().is_none());
            std::fs::remove_dir(&failed_target).unwrap();
        }
        let mut gate = AllowOnce::default();
        let (text, outcome) = super::round2_tests::dispatch(
            serde_json::json!({"command":format!("git worktree add -b task ../{relative}")}),
            &root,
            &base,
            &session,
            Some(&mut gate),
        )
        .await;
        assert_eq!(outcome, Some(ExecOutcome::Passed), "{text}");
        assert_eq!(session.snapshot().expect("adopted").worktree, target);
        assert_eq!(gate.0.len(), 2, "separate read and write approval");
        assert_eq!(gate.1.len(), 2, "creation consumes both once grants");
        assert!(gate.2.is_empty(), "no queued approval remains");
        assert!(gate
            .0
            .iter()
            .all(|r| Path::new(&r.target) == target && r.harness_bound));
        assert!(gate.0.iter().any(|r| r.kind == DenialKind::FsRead));
        assert!(gate.0.iter().any(|r| r.kind == DenialKind::FsWrite));
        let (text, outcome) = super::round2_tests::dispatch(
        serde_json::json!({"command":format!("cat '{0}/seed' && printf changed > '{0}/seed' && cat '{0}/seed'", target.display()), "cwd":target}),
        &root, &base, &session, None,
    ).await;
        assert_eq!(outcome, Some(ExecOutcome::Passed), "{text}");
        assert!(
            text.contains("original contents") && text.contains("changed"),
            "{text}"
        );
        let (text, outcome) = super::round2_tests::dispatch(
            serde_json::json!({"command":"git add seed", "cwd":target}),
            &root,
            &base,
            &session,
            None,
        )
        .await;
        assert_eq!(
            outcome,
            Some(ExecOutcome::Passed),
            "task admin must remain writable: {text}"
        );
        assert!(root.join(".git/worktrees/task/index").is_file());
        // The same bounded-read exact-approval path must reach the governed
        // commit broker, not merely write the task's index.
        let original_ref = std::fs::read(root.join(".git/refs/heads/main")).unwrap();
        let original_index = std::fs::read(root.join(".git/index")).unwrap();
        let config = std::fs::read(root.join(".git/config")).unwrap();
        let (text, outcome) = super::refs_tests::run(
            "git -c user.name=Fixture -c user.email=fixture@example.invalid commit -m changed",
            &target,
            &root,
            &base,
            &session,
        )
        .await;
        assert_eq!(
            outcome,
            Some(ExecOutcome::Passed),
            "governed commit: {text}"
        );
        assert_ne!(
            std::fs::read(root.join(".git/refs/heads/task")).unwrap(),
            original_ref
        );
        assert_eq!(
            std::fs::read(root.join(".git/refs/heads/main")).unwrap(),
            original_ref
        );
        assert_eq!(
            std::fs::read(root.join(".git/index")).unwrap(),
            original_index
        );
        assert_eq!(std::fs::read(root.join(".git/config")).unwrap(), config);

        // A new planning phase after adoption must not lose
        // the task root or its projected authority on operator approval.
        use crate::agentic::PlanModeControl as _;
        #[derive(Default)]
        struct Plan(std::sync::atomic::AtomicBool, std::sync::atomic::AtomicBool);
        impl crate::agentic::PlanModeControl for Plan {
            fn is_plan_mode(&self) -> bool {
                self.0.load(std::sync::atomic::Ordering::SeqCst)
            }
            fn set_plan_mode(&self, active: bool) -> Result<(), String> {
                self.0.store(active, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn request_exit(&self) -> Result<(), String> {
                self.1.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn take_exit_requested(&self) -> bool {
                self.1.swap(false, std::sync::atomic::Ordering::SeqCst)
            }
        }
        let plan = Plan::default();
        let ledger = crate::agentic::scheduled::SessionStepLedger::default();
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Phase {
            Act,
            Plan,
            Approved,
        }
        for (name, args, phase) in [
            ("enter_plan_mode", serde_json::json!({}), Phase::Act),
            (
                "write_file",
                serde_json::json!({"path":"plan-guard", "content":"forbidden"}),
                Phase::Plan,
            ),
            ("exit_plan_mode", serde_json::json!({}), Phase::Plan),
            (
                "write_file",
                serde_json::json!({"path":"plan-guard", "content":"approved"}),
                Phase::Approved,
            ),
            (
                "run_command",
                serde_json::json!({"command":"pwd"}),
                Phase::Approved,
            ),
            (
                "write_file",
                serde_json::json!({"path":root.join("seed"), "content":"forbidden"}),
                Phase::Approved,
            ),
        ] {
            if phase == Phase::Approved && plan.is_plan_mode() {
                assert!(
                    plan.take_exit_requested(),
                    "approval follows an exit request"
                );
                // Same operation performed by the TUI's approved verdict.
                plan.set_plan_mode(false).unwrap();
            }
            let current = if phase == Phase::Plan {
                base.meet(&crate::agentic::plan_phase_clamp())
            } else {
                base.clone()
            };
            let mut display = ToolDisplay::new(Vec::new(), false, 80, 20, false);
            let outcome = std::sync::OnceLock::new();
            let text = execute(
                &mut display,
                name,
                &args,
                root.to_str().unwrap(),
                false,
                20,
                &current,
                &mut crate::agentic::NoMcp,
                ToolCollaborators {
                    worktree_session: Some(&session),
                    plan_mode_control: Some(&plan),
                    step_ledger: Some(&ledger),
                    execution: Some(&outcome),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
            )
            .await;
            assert_eq!(session.task_root(&root).as_deref(), Some(target.as_path()));
            assert_eq!(
                plan.is_plan_mode(),
                phase != Phase::Approved,
                "{name}: {text}"
            );
            if name == "enter_plan_mode" {
                assert!(text.contains("entered PLAN MODE"), "{text}");
            }
            if name == "exit_plan_mode" {
                assert!(text.contains("exit requested"), "{text}");
            }
            if phase == Phase::Plan {
                assert!(!target.join("plan-guard").exists(), "{text}");
            }
            if phase == Phase::Approved && name == "write_file" && args["path"] == "plan-guard" {
                assert_eq!(
                    std::fs::read_to_string(target.join("plan-guard")).unwrap(),
                    "approved",
                    "{text}"
                );
            }
            if phase == Phase::Approved && name == "run_command" {
                assert!(text.contains(target.to_str().unwrap()), "{text}");
                assert_eq!(outcome.get(), Some(&ExecOutcome::Passed), "{text}");
            }
            assert_eq!(
                std::fs::read_to_string(root.join("seed")).unwrap(),
                "original contents",
                "{text}"
            );
        }

        // A current gate ceiling must also apply to native file tools, which
        // do not enter the shell's own permission refresh.
        let mut display = ToolDisplay::new(Vec::new(), false, 80, 20, false);
        let mut read_only = ReadOnly;
        let text = execute(
            &mut display,
            "write_file",
            &serde_json::json!({"path":target.join("seed"),"content":"forbidden"}),
            root.to_str().unwrap(),
            false,
            20,
            &base,
            &mut crate::agentic::NoMcp,
            ToolCollaborators {
                worktree_session: Some(&session),
                permission_gate: Some(&mut read_only),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
        )
        .await;
        assert!(
            text.contains("capability denied"),
            "native write bypassed refreshed ceiling: {text}"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("seed")).unwrap(),
            "changed"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("seed")).unwrap(),
            "original contents"
        );
        for source in [
            format!("cat '{}/secret'", other.display()),
            format!("printf bad > '{}/secret'", other.display()),
            format!("printf bad > '{}/seed'", root.display()),
        ] {
            let (text, outcome) = super::round2_tests::dispatch(
                serde_json::json!({"command":source, "cwd":target}),
                &root,
                &base,
                &session,
                None,
            )
            .await;
            assert_ne!(outcome, Some(ExecOutcome::Passed), "{source}: {text}");
        }

        assert_eq!(
            std::fs::read_to_string(other.join("secret")).unwrap(),
            "neighbor contents"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("seed")).unwrap(),
            "original contents"
        );
        session.lift();
        let (text, outcome) = super::round2_tests::dispatch(
            serde_json::json!({"command":format!("cat '{}/seed'", target.display())}),
            &root,
            &base,
            &session,
            None,
        )
        .await;
        assert_ne!(
            outcome,
            Some(ExecOutcome::Passed),
            "task access must end on lift: {text}"
        );
        let output =
            crate::agentic::tools::tests::git_shell_grant::hermetic_git(&root, temp.path())
                .args(["worktree", "remove", "--force", target.to_str().unwrap()])
                .output()
                .unwrap();
        assert!(output.status.success());
        let (text, outcome) = super::round2_tests::dispatch(
            serde_json::json!({"command":format!("git worktree add ../{relative} task")}),
            &root,
            &base,
            &session,
            Some(&mut gate),
        )
        .await;
        assert_eq!(
            outcome,
            Some(ExecOutcome::Denied),
            "spent approval funded a second creation after lift: {text}"
        );
        assert!(!target.exists());
        assert!(gate.2.is_empty());
    }
}

#[path = "worktree_adoption_sibling_preparation.rs"]
mod preparation_tests;
