use super::*;
use crate::worktree_adoption::{
    tests::{fixture, link},
    WorktreeSession,
};
use crate::Scope;

struct BroadGate;
impl PermissionGate for BroadGate {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        PermissionDecision::Allow(Caveats::top())
    }
    fn refresh_caveats(&mut self, _: &Caveats) -> PermissionDecision {
        PermissionDecision::Allow(Caveats::top())
    }
    fn apply_pending_once(&mut self, _: DenialKind, _: &str, _: &Caveats) -> Caveats {
        Caveats::top()
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        HumanQuestionOutcome::Unavailable
    }
}

/// #2733: creation parsing recognizes real command input, not child output;
/// a listed, existing, failed, dynamic, or unrelated worktree never adopts.
#[test]
fn worktree_adoption_creation_requires_the_command_and_verified_metadata() {
    let (_temp, policy, _) = fixture(false);
    let root = policy.worktree.parent().unwrap().join("main");
    let command = format!(
        "cd '{}' && git worktree add -b task '{}' 2>&1 | tail -5",
        root.display(),
        policy.worktree.display()
    );
    let args = serde_json::json!({"command":command});
    let candidate = creation(
        "run_command",
        &args,
        root.to_str().unwrap(),
        &Caveats::top(),
    )
    .unwrap();
    assert!(candidate.verify().is_none());
    let candidate = creation(
        "run_command",
        &args,
        root.to_str().unwrap(),
        &Caveats::top(),
    )
    .unwrap();
    link(&policy);
    assert!(candidate.verify().is_some());
    assert!(creation(
        "run_command",
        &args,
        root.to_str().unwrap(),
        &Caveats::top()
    )
    .is_none());
    for cmd in [
        "echo 'git worktree add /tmp/fake'",
        "git worktree list",
        "git worktree add $DEST",
        "cd other && git worktree add task",
    ] {
        assert!(
            creation(
                "run_command",
                &serde_json::json!({"command":cmd}),
                root.to_str().unwrap(),
                &Caveats::top()
            )
            .is_none(),
            "{cmd}"
        );
    }
}

/// #2733: a broad held/once grant cannot restore original-root authority.
#[test]
fn worktree_adoption_clamps_refresh_retry_and_build_requests() {
    let (_temp, policy, _) = fixture(true);
    let original = policy.worktree.parent().unwrap().parent().unwrap();
    let mut inner = BroadGate;
    let mut gate = Guard {
        policy: &policy,
        inner: Some(&mut inner),
    };
    for decision in [gate.refresh_caveats(&Caveats::top()), gate.ask(&[])] {
        let PermissionDecision::Allow(c) = decision else {
            panic!("allowed attenuated grant")
        };
        assert!(!crate::caveats::permits_path(
            &c.fs_write,
            original.to_str().unwrap()
        ));
    }
    let c = gate.apply_pending_once(DenialKind::Exec, "python3", &Caveats::top());
    assert!(!crate::caveats::permits_path(
        &c.fs_write,
        original.to_str().unwrap()
    ));
    for kind in [DenialKind::FsWrite, DenialKind::Build] {
        let request = PermissionRequest {
            kind,
            target: original.to_string_lossy().into_owned(),
            ..request_fixture()
        };
        assert!(matches!(
            gate.ask_with_caveats(&Caveats::top(), &[request]),
            PermissionDecision::Deny
        ));
    }
}

fn request_fixture() -> PermissionRequest {
    PermissionRequest {
        tool: "test".into(),
        kind: DenialKind::FsWrite,
        target: "unused".into(),
        reason: "fixture".into(),
        harness_bound: true,
    }
}

/// #2733: actual file-tool dispatch refuses stray writes before permission
/// prompts, retains reads, and accepts the same path after explicit lift.
#[tokio::test]
async fn worktree_adoption_guards_file_dispatch_across_calls_and_lift() {
    let (_temp, policy, _) = fixture(false);
    link(&policy);
    let original = policy.worktree.parent().unwrap().join("main");
    std::fs::write(original.join("source.rs"), "before").unwrap();
    let session = WorktreeSession::default();
    session.adopt(policy.clone());
    for name in ["write_file", "edit_file", "delete_file"] {
        let args = serde_json::json!({"path":"source.rs", "content":"after", "old_string":"before", "new_string":"after"});
        let output = call(name, &args, &original, &session).await;
        assert!(
            output.contains("original checkout is read-only"),
            "{output}"
        );
        assert!(output.contains(policy.worktree.to_str().unwrap()));
        assert_eq!(
            std::fs::read_to_string(original.join("source.rs")).unwrap(),
            "before"
        );
    }
    let read = call(
        "read_file",
        &serde_json::json!({"path":"source.rs"}),
        &original,
        &session,
    )
    .await;
    assert!(read.contains("before"), "{read}");
    session.lift();
    let output = call(
        "edit_file",
        &serde_json::json!({"path":"source.rs", "old_string":"before", "new_string":"after"}),
        &original,
        &session,
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(original.join("source.rs")).unwrap(),
        "after",
        "{output}"
    );
}

async fn call(
    name: &str,
    args: &serde_json::Value,
    original: &Path,
    session: &WorktreeSession,
) -> String {
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 20, false);
    let mut gate = BroadGate;
    let mut c = Caveats::top();
    c.fs_write = Scope::only([original.to_string_lossy().into_owned()]);
    execute(
        &mut display,
        name,
        args,
        original.to_str().unwrap(),
        false,
        20,
        &c,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(session),
            permission_gate: Some(&mut gate),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await
}

/// #2733: ground command-observation and positive-scope unit tests in real
/// Git creation and actual interpreter writes under the native kernel fence.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires native kernel confinement, git and python3"]
async fn worktree_adoption_native_creation_and_interpreter_fence() {
    assert!(crate::confined_exec::kernel_fs_fence_available());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    let target = temp.path().join("task");
    std::fs::create_dir(&root).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec![
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
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
    let session = WorktreeSession::default();
    let mut c = Caveats::top();
    c.net = Scope::none();
    c.fs_write = Scope::only([temp.path().to_string_lossy().into_owned()]);
    let outcome = std::sync::OnceLock::new();
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 20, false);
    let output = execute(
        &mut display,
        "run_command",
        &serde_json::json!({"command":format!("git worktree add -b task '{}'", target.display())}),
        root.to_str().unwrap(),
        false,
        20,
        &c,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(&session),
            execution: Some(&outcome),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    let adopted = session
        .snapshot()
        .unwrap_or_else(|| panic!("not adopted: {output}"));
    assert_eq!(adopted.worktree, target.canonicalize().unwrap());
    for (dir, expected) in [(&root, false), (&target, true)] {
        let outcome = std::sync::OnceLock::new();
        let command = format!(
            "python3 -c \"from pathlib import Path; Path('{}').write_text('probe')\"",
            dir.join("probe").display()
        );
        let output = execute(
            &mut display,
            "run_command",
            &serde_json::json!({"command":command, "cwd":target}),
            root.to_str().unwrap(),
            false,
            20,
            &c,
            &mut crate::agentic::NoMcp,
            ToolCollaborators {
                worktree_session: Some(&session),
                execution: Some(&outcome),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
        )
        .await;
        assert_eq!(dir.join("probe").exists(), expected, "{output}");
    }
    let outcome = std::sync::OnceLock::new();
    let output = execute(
        &mut display,
        "run_command",
        &serde_json::json!({"command":"git add probe", "cwd":target}),
        root.to_str().unwrap(),
        false,
        20,
        &c,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(&session),
            execution: Some(&outcome),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    assert_eq!(
        outcome.get(),
        Some(&crate::ExecOutcome::Passed),
        "task Git metadata writes must remain usable: {output}"
    );
}

/// #2733: a same-invocation Python write cannot run before adoption takes effect.
#[test]
fn worktree_adoption_creation_is_an_execution_boundary() {
    for cmd in [
        "git worktree add ../task -b task; python3 mutate.py",
        "git worktree add ../task; echo stray > source.rs",
    ] {
        assert!(!creation_batch_is_read_only_after_add(
            &serde_json::json!({"command":cmd})
        ));
    }
    assert!(creation_batch_is_read_only_after_add(
        &serde_json::json!({"command":"git worktree add -b task ../task 2>&1 | tail -5; git branch --show-current"})
    ));
}
