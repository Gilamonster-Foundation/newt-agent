//! #2748: real branch and commit writes through the adoption fence.
use super::*;
use crate::agentic::tools::tests::git_shell_grant::{hermetic_git, FixtureGitTool};
use crate::{worktree_adoption::WorktreeSession, ExecOutcome, Scope};

fn git(root: &Path, home: &Path, args: &[&str]) -> String {
    let output = hermetic_git(root, home).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn run(
    source: &str,
    cwd: &Path,
    original: &Path,
    caveats: &Caveats,
    session: &WorktreeSession,
) -> (String, Option<ExecOutcome>) {
    let outcome = std::sync::OnceLock::new();
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 120, 40, false);
    let text = execute(
        &mut display,
        "run_command",
        &serde_json::json!({"command": source, "cwd": cwd}),
        original.to_str().unwrap(),
        false,
        40,
        caveats,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(session),
            git_tool: Some(&FixtureGitTool),
            execution: Some(&outcome),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    (text, outcome.get().copied())
}

/// #2748: grounds the positive-scope model in real worktree add, checkout -b,
/// and commit. New refs/reflogs must work while original files/HEAD/index and
/// its checked-out branch remain unchanged, including packed branch refs.
#[tokio::test]
async fn adoption_refs_real_checkout_and_commit_preserve_original() {
    let _env = crate::process_env::lock();
    let _engine =
        crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    assert!(crate::confined_exec::kernel_fs_fence_available());
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    std::fs::create_dir(&original).unwrap();
    git(
        &original,
        temp.path(),
        &["init", "-q", "-b", "original-topic"],
    );
    git(&original, temp.path(), &["config", "user.name", "Fixture"]);
    git(
        &original,
        temp.path(),
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(
        &original,
        temp.path(),
        &["config", "commit.gpgsign", "false"],
    );
    std::fs::write(original.join("seed"), "original content\n").unwrap();
    git(&original, temp.path(), &["add", "seed"]);
    git(&original, temp.path(), &["commit", "-qm", "initial"]);
    git(&original, temp.path(), &["branch", "existing"]);
    git(&original, temp.path(), &["pack-refs", "--all"]);
    let original_before = git(&original, temp.path(), &["rev-parse", "original-topic"]);
    let snapshots: Vec<_> = [
        "seed",
        ".git/HEAD",
        ".git/index",
        ".git/packed-refs",
        ".git/config",
    ]
    .iter()
    .map(|p| (original.join(p), std::fs::read(original.join(p)).unwrap()))
    .collect();
    let mut c = Caveats::top();
    c.net = Scope::none();
    c.fs_write = Scope::only([temp.path().to_string_lossy().into_owned()]);
    let session = WorktreeSession::default();
    let target = original.join("task");
    let (text, outcome) = run(
        "git worktree add --detach task HEAD",
        &original,
        &original,
        &c,
        &session,
    )
    .await;
    assert_eq!(outcome, Some(ExecOutcome::Passed), "{text}");
    assert!(session.snapshot().is_some(), "{text}");
    let (text, outcome) = run("git checkout -b feat/x", &target, &original, &c, &session).await;
    assert_eq!(
        outcome,
        Some(ExecOutcome::Passed),
        "branch creation: {text}"
    );
    std::fs::write(target.join("seed"), "task content\n").unwrap();
    let (text, outcome) = run(
        "git add seed && git -c user.name=Fixture -c user.email=fixture@example.invalid commit -m change",
        &target,
        &original,
        &c,
        &session,
    )
    .await;
    assert_eq!(outcome, Some(ExecOutcome::Passed), "commit: {text}");
    assert_ne!(
        git(&target, temp.path(), &["rev-parse", "HEAD"]),
        original_before
    );
    assert_eq!(
        git(&target, temp.path(), &["branch", "--show-current"]),
        "feat/x"
    );
    assert!(original.join(".git/logs/refs/heads/feat/x").is_file());
    for command in [
        "git branch -f original-topic HEAD",
        "git update-ref refs/heads/original-topic HEAD",
        "git config user.name Changed",
        "git checkout -b original-topic",
        "git switch -c existing",
    ] {
        let (text, outcome) = run(command, &target, &original, &c, &session).await;
        assert_ne!(outcome, Some(ExecOutcome::Passed), "{command}: {text}");
    }
    // Same-tip creation also works for a flat ref, without opening refs/heads.
    let (text, outcome) = run(
        "git switch -c second HEAD",
        &target,
        &original,
        &c,
        &session,
    )
    .await;
    assert_eq!(outcome, Some(ExecOutcome::Passed), "{text}");
    for relative in [
        "seed",
        ".git/HEAD",
        ".git/index",
        ".git/packed-refs",
        ".git/refs/heads/original-topic",
    ] {
        let source = format!("printf hostile > '{}'", original.join(relative).display());
        let (text, outcome) = run(&source, &target, &original, &c, &session).await;
        assert_ne!(outcome, Some(ExecOutcome::Passed), "{source}: {text}");
    }
    // Protect even a non-default original branch if the writable task HEAD
    // is redirected to it. This exercises the bounded commit publisher guard.
    let (text, outcome) = run(
        "git checkout --ignore-other-worktrees original-topic",
        &target,
        &original,
        &c,
        &session,
    )
    .await;
    assert_eq!(outcome, Some(ExecOutcome::Passed), "{text}");
    let (text, outcome) = run(
        "git commit --allow-empty -m forbidden",
        &target,
        &original,
        &c,
        &session,
    )
    .await;
    assert_eq!(outcome, Some(ExecOutcome::Denied), "{text}");
    assert!(text.contains("original checkout's branch"), "{text}");
    assert_eq!(
        git(&original, temp.path(), &["rev-parse", "original-topic"]),
        original_before
    );
    for (path, before) in snapshots {
        assert_eq!(std::fs::read(&path).unwrap(), before, "{}", path.display());
    }
}

/// #2748: a literal destination outside the current write grant must reach
/// the normal filesystem permission gate before Git can partially execute.
#[tokio::test]
async fn adoption_refs_external_destination_prompts_before_execution() {
    struct Deny(Vec<PermissionRequest>);
    impl PermissionGate for Deny {
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            self.0.extend_from_slice(requests);
            PermissionDecision::Deny
        }
    }
    let _env = crate::process_env::lock();
    let _engine =
        crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    std::fs::create_dir(&root).unwrap();
    git(&root, temp.path(), &["init", "-q", "-b", "main"]);
    git(
        &root,
        temp.path(),
        &["commit", "--allow-empty", "-qm", "initial"],
    );
    let mut c = Caveats::top();
    c.net = Scope::none();
    c.fs_write = Scope::only([root.to_string_lossy().into_owned()]);
    let session = WorktreeSession::default();
    let mut gate = Deny(Vec::new());
    let target = temp.path().join("sibling");
    let (text, outcome) = super::round2_tests::dispatch(
        serde_json::json!({"command":"git worktree add --detach ../sibling HEAD"}),
        &root,
        &c,
        &session,
        Some(&mut gate),
    )
    .await;
    assert!(
        gate.0
            .iter()
            .any(|r| r.kind == DenialKind::FsWrite && Path::new(&r.target) == temp.path()),
        "missing destination prompt: {text}"
    );
    assert_eq!(outcome, Some(ExecOutcome::Denied), "{text}");
    assert!(!target.exists());
    assert!(session.snapshot().is_none());
}

/// #2748: permission approval adds only the destination's existing parent and lets
/// the same preflighted creation complete for sibling and separate temp roots.
#[tokio::test]
async fn adoption_refs_approved_external_destination_is_adopted() {
    struct Allow(Vec<PermissionRequest>);
    impl PermissionGate for Allow {
        fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
            panic!("expected filesystem baseline");
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            HumanQuestionOutcome::Unavailable
        }
        fn ask_with_caveats(
            &mut self,
            base: &Caveats,
            requests: &[PermissionRequest],
        ) -> PermissionDecision {
            self.0.extend_from_slice(requests);
            let Scope::Only(mut roots) = base.fs_write.clone() else {
                panic!("expected scoped writes")
            };
            for request in requests {
                assert_eq!(request.kind, DenialKind::FsWrite);
                roots.insert(request.target.clone());
            }
            PermissionDecision::Allow(Caveats {
                fs_write: Scope::Only(roots),
                ..base.clone()
            })
        }
    }
    let _env = crate::process_env::lock();
    let _engine =
        crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    let temp = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    std::fs::create_dir(&root).unwrap();
    git(&root, temp.path(), &["init", "-q", "-b", "main"]);
    git(
        &root,
        temp.path(),
        &["commit", "--allow-empty", "-qm", "initial"],
    );
    let mut c = Caveats::top();
    c.net = Scope::none();
    c.fs_write = Scope::only([root.to_string_lossy().into_owned()]);
    for target in [
        temp.path().join("sibling"),
        external.path().join("separate"),
    ] {
        let session = WorktreeSession::default();
        let mut gate = Allow(Vec::new());
        let source = format!("git worktree add --detach '{}' HEAD", target.display());
        let (text, outcome) = super::round2_tests::dispatch(
            serde_json::json!({"command":source}),
            &root,
            &c,
            &session,
            Some(&mut gate),
        )
        .await;
        assert_eq!(outcome, Some(ExecOutcome::Passed), "{text}");
        assert_eq!(gate.0.len(), 1, "{text}");
        assert_eq!(Path::new(&gate.0[0].target), target.parent().unwrap());
        assert_eq!(
            session.snapshot().unwrap().worktree,
            target.canonicalize().unwrap()
        );
    }
}
