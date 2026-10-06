//! #2759: ground creation admission and denial classification in real dispatch.
use super::*;
use crate::agentic::tools::tests::git_shell_grant::hermetic_git;
use crate::{worktree_adoption::WorktreeSession, ExecOutcome, Scope};

fn repo() -> (tempfile::TempDir, PathBuf, Caveats) {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    std::fs::create_dir(&original).unwrap();
    for args in [
        vec!["init", "-q", "-b", "original"],
        vec![
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "seed",
        ],
    ] {
        let out = hermetic_git(&original, temp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut c = Caveats::top();
    c.net = Scope::none();
    c.fs_write = Scope::only([temp.path().to_string_lossy().into_owned()]);
    (temp, original, c)
}

#[derive(Default)]
struct GrantExec(usize);
impl PermissionGate for GrantExec {
    fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
        assert!(requests.iter().all(|r| r.kind == DenialKind::Exec));
        self.0 += 1;
        PermissionDecision::Allow(Caveats::top())
    }
    fn ask_question(&mut self, _: &str) -> super::super::super::permissions::HumanQuestionOutcome {
        panic!("unexpected question")
    }
}

/// #2759: reject a nested destination before Git creates either branch or files,
/// including a symlink alias and a Git -C selector below the repository root.
#[tokio::test]
async fn adoption_2759_nested_creation_is_refused_before_git() {
    let _env = crate::process_env::lock();
    let _engine =
        crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    let (temp, original, c) = repo();
    std::fs::create_dir(original.join("sub")).unwrap();
    std::os::unix::fs::symlink(&original, temp.path().join("alias")).unwrap();
    for command in [
        "git worktree add -b nested .worktrees/task",
        "git worktree add -b nested ../alias/task",
        "git -C sub worktree add -b nested ../task",
    ] {
        let session = WorktreeSession::default();
        let mut gate = GrantExec::default();
        let (text, outcome) = round2_tests::dispatch(
            serde_json::json!({"command":command}),
            &original,
            &c,
            &session,
            Some(&mut gate),
        )
        .await;
        assert_eq!(outcome, Some(ExecOutcome::Denied), "{text}");
        assert!(
            text.contains("sibling") && text.contains("../<name>"),
            "{text}"
        );
        assert!(session.snapshot().is_none());
        assert_eq!(gate.0, 0);
        assert!(!original.join(".worktrees").exists());
        assert!(!original.join("task").exists());
        assert!(!original.join(".git/refs/heads/nested").exists());
    }
}

/// #2759: an actual filesystem fence must not ask to grant rm; genuine exec
/// misses must not be mislabeled as that fence, even after an allow-once reply.
#[tokio::test]
async fn adoption_2759_filesystem_and_exec_denials_stay_distinct() {
    let _env = crate::process_env::lock();
    let _engine =
        crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    assert!(crate::confined_exec::kernel_fs_fence_available());
    let (_temp, original, mut c) = repo();
    let session = WorktreeSession::default();
    let (text, outcome) = round2_tests::dispatch(
        serde_json::json!({"command":"git worktree add -b task ../task"}),
        &original,
        &c,
        &session,
        None,
    )
    .await;
    assert_eq!(outcome, Some(ExecOutcome::Passed), "{text}");
    let policy = session.snapshot().unwrap();
    std::fs::write(original.join("sentinel"), "safe").unwrap();
    c.exec = Scope::only(["printf".to_owned()]);
    let mut gate = GrantExec::default();
    for args in [
        serde_json::json!({"command":"printf changed > sentinel && rm -f sentinel", "cwd":original}),
        serde_json::json!({"command":"rm -f sentinel", "cwd":original, "fs_write":[original.join("sentinel")]}),
    ] {
        let (text, outcome) =
            round2_tests::dispatch(args, &original, &c, &session, Some(&mut gate)).await;
        assert_eq!(outcome, Some(ExecOutcome::Denied), "{text}");
        assert!(
            text.contains("original checkout is read-only after worktree adoption"),
            "{text}"
        );
        assert_eq!(gate.0, 0, "filesystem denial must not request exec: {text}");
        assert_eq!(
            std::fs::read_to_string(original.join("sentinel")).unwrap(),
            "safe"
        );
    }
    let (text, outcome) = round2_tests::dispatch(
        serde_json::json!({"command":"printf '' && rm -f absent", "cwd":policy.worktree}),
        &original,
        &c,
        &session,
        Some(&mut gate),
    )
    .await;
    assert_eq!(outcome, Some(ExecOutcome::Denied), "{text}");
    assert_eq!(gate.0, 1, "a real exec miss remains promptable");
    assert!(text.contains("granted:"), "{text}");
    assert!(
        !text.contains("original checkout is read-only after worktree adoption"),
        "false fence diagnosis: {text}"
    );
}
