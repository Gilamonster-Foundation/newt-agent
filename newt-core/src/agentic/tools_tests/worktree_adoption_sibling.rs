//! #2757: ground destination approvals in real confined Git with bounded reads.
use super::*;
use crate::agentic::permissions::widen_caveats;
use crate::worktree_adoption::WorktreeSession;
use crate::{ExecOutcome, Scope};

#[derive(Default)]
struct AllowOnce(Vec<PermissionRequest>, Vec<(DenialKind, String)>);
impl PermissionGate for AllowOnce {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        panic!("approval must preserve the caller baseline")
    }
    fn ask_with_caveats(
        &mut self,
        base: &Caveats,
        requests: &[PermissionRequest],
    ) -> PermissionDecision {
        self.0.extend_from_slice(requests);
        let grants: Vec<_> = requests
            .iter()
            .map(|r| (r.kind, r.target.clone()))
            .collect();
        PermissionDecision::Allow(widen_caveats(base, &grants))
    }
    fn consume_pending_once(&mut self, kind: DenialKind, target: &str) {
        self.1.push((kind, target.into()));
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        HumanQuestionOutcome::Unavailable
    }
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
/// after the one creation call, without opening any neighboring checkout.
#[tokio::test]
async fn sibling_grant_once_creates_and_keeps_task_readable_writable() {
    let _env = crate::process_env::lock();
    let _engine =
        crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    assert!(crate::confined_exec::kernel_fs_fence_available());
    for relative in ["task", "missing-parent/task"] {
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
    }
}

/// #2757: approval may not create a directory if it lacks either required axis.
#[test]
fn sibling_preparation_requires_read_and_write_before_mkdir() {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    std::fs::remove_dir(&policy.worktree).unwrap();
    let original = policy.worktree.parent().unwrap().join("main");
    let mut candidate = Creation::before(&original, &policy.worktree, &Caveats::top()).unwrap();
    for (read, write) in [(Scope::All, Scope::none()), (Scope::none(), Scope::All)] {
        let authority = Caveats {
            fs_read: read,
            fs_write: write,
            ..Caveats::top()
        };
        assert!(candidate.prepare(&authority).is_err());
        assert!(!policy.worktree.exists());
    }
}

/// #2757: a destination replaced with a symlink while the operator approves
/// cannot redirect the harness's directory creation into a neighbor.
#[test]
fn sibling_preparation_refuses_destination_symlink_swap() {
    let (_temp, policy, neighbor) = crate::worktree_adoption::tests::fixture(false);
    std::fs::remove_dir(&policy.worktree).unwrap();
    let original = policy.worktree.parent().unwrap().join("main");
    let mut candidate = Creation::before(&original, &policy.worktree, &Caveats::top()).unwrap();
    std::os::unix::fs::symlink(&neighbor, &policy.worktree).unwrap();
    assert!(candidate.prepare(&Caveats::top()).is_err());
    assert_eq!(std::fs::read_dir(neighbor).unwrap().count(), 0);
}

/// #2757: replacing the approved path's parent while prompting must not
/// redirect mkdir into a different directory that happens to share its name.
#[test]
fn sibling_preparation_refuses_replaced_parent() {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(true);
    std::fs::remove_dir(&policy.worktree).unwrap();
    let parent = policy.worktree.parent().unwrap();
    let original = parent.parent().unwrap();
    let mut candidate = Creation::before(original, &policy.worktree, &Caveats::top()).unwrap();
    std::fs::rename(parent, parent.with_extension("held")).unwrap();
    std::fs::create_dir(parent).unwrap();
    assert!(candidate.prepare(&Caveats::top()).is_err());
    assert!(!policy.worktree.exists());
}
