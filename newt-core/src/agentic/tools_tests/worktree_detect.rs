//! #2791: ground admin-directory detection in real Git and tool dispatch on every OS.
use super::*;
use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
use crate::worktree_adoption::WorktreeSession;

struct NullPresentation;
impl ToolPresentation for NullPresentation {
    fn preview(&mut self, _: &str, _: usize) {}
    fn document(&mut self, _: &str) {}
    fn override_result(&mut self, _: String) {}
}

/// #2791: aggregate shell syntax must not hide a newly created linked checkout.
/// Grounds the directory-delta binding rule in real Git, then verifies command
/// cwd and relative file tools follow it without arming an adoption fence.
#[tokio::test]
async fn worktree_detect_2791_compound_routes_tools() {
    let _lock = env_lock().await;
    let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let _cmd = EnvVar::set("NEWT_WINDOWS_CMD", "1");
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual = EnvVar::unset("VIRTUAL_ENV");
    let _git_env: Vec<_> = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_CEILING_DIRECTORIES",
    ]
    .iter()
    .map(|name| EnvVar::unset(name))
    .collect();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    std::fs::create_dir(&root).unwrap();
    let empty = temp.path().join("empty-config");
    std::fs::write(&empty, "").unwrap();
    let templates = temp.path().join("templates");
    std::fs::create_dir(&templates).unwrap();
    let _global = EnvVar::set("GIT_CONFIG_GLOBAL", empty.to_str().unwrap());
    let _system = EnvVar::set("GIT_CONFIG_NOSYSTEM", "1");
    let _templates = EnvVar::set("GIT_TEMPLATE_DIR", templates.to_str().unwrap());
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
        let out = std::process::Command::new("git")
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let session = WorktreeSession::default();
    let outcome = std::sync::OnceLock::new();
    let text = execute(
        &mut NullPresentation, "run_command",
        &serde_json::json!({"command":"git worktree add -b detected ../detected 2>&1 | git hash-object --stdin && git worktree list"}),
        root.to_str().unwrap(), false, 40, &Caveats::top(), &mut crate::agentic::NoMcp,
        ToolCollaborators { worktree_session: Some(&session), execution: Some(&outcome), ..Default::default() }, false, PromptDisposition::Act,
    ).await;
    assert_eq!(outcome.get(), Some(&crate::ExecOutcome::Passed), "{text}");
    let task = temp.path().join("detected").canonicalize().unwrap();
    assert_eq!(session.task_root(&root), Some(task.clone()), "{text}");
    assert!(text.contains("Task worktree:"), "{text}");
    assert!(
        session.snapshot().is_none(),
        "detection grants no authority"
    );
    std::fs::write(task.join("marker"), "new task marker").unwrap();
    let directory = std::sync::OnceLock::new();
    let text = execute(
        &mut NullPresentation,
        "run_command",
        &serde_json::json!({"command":"git branch --show-current"}),
        root.to_str().unwrap(),
        false,
        40,
        &Caveats::top(),
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(&session),
            command_directory: Some(&directory),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    assert!(text.contains("detected"), "{text}");
    assert_eq!(
        directory.get().map(|p| p.canonicalize().unwrap()),
        Some(task)
    );
    let text = execute(
        &mut NullPresentation,
        "read_file",
        &serde_json::json!({"path":"marker"}),
        root.to_str().unwrap(),
        false,
        40,
        &Caveats::top(),
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(&session),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    assert!(text.contains("new task marker"), "{text}");
    for (command, ambiguous) in [
        ("git status --short", false),
        (
            "git worktree add -b one ../one && git worktree add -b two ../two",
            true,
        ),
    ] {
        let session = WorktreeSession::default();
        let outcome = std::sync::OnceLock::new();
        let text = execute(
            &mut NullPresentation,
            "run_command",
            &serde_json::json!({"command":command}),
            root.to_str().unwrap(),
            false,
            40,
            &Caveats::top(),
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
        assert_eq!(outcome.get(), Some(&crate::ExecOutcome::Passed), "{text}");
        assert!(session.task_hint().is_none(), "{text}");
        assert_eq!(text.contains("Multiple new worktrees"), ambiguous, "{text}");
    }
}

fn metadata_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("main/.git")).unwrap();
    temp
}

fn add_metadata(temp: &Path, name: &str) -> PathBuf {
    let root = temp.join(name);
    let admin = temp.join("main/.git/worktrees").join(name);
    std::fs::create_dir_all(&admin).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join(".git"),
        format!("gitdir: ../main/.git/worktrees/{name}\n"),
    )
    .unwrap();
    std::fs::write(admin.join("gitdir"), format!("../../../../{name}/.git\n")).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{name}\n")).unwrap();
    root.canonicalize().unwrap()
}

/// #2791: successful non-worktree commands are silent; multiple new entries
/// bind none and explain the ambiguity, preserving the previous task locator.
#[test]
fn worktree_detect_2791_zero_or_multiple_preserve_state() {
    let temp = metadata_repo();
    let root = temp.path().join("main");
    let session = WorktreeSession::default();
    let before = detect::Snapshot::before(&root, &crate::Scope::All).unwrap();
    assert!(before
        .record(&session, &root, Some(&crate::ExecOutcome::Passed))
        .is_none());
    assert!(session.task_hint().is_none());
    let old = add_metadata(temp.path(), "old");
    session.record_task_worktree(&old, "old");
    let before = detect::Snapshot::before(&root, &crate::Scope::All).unwrap();
    add_metadata(temp.path(), "one");
    add_metadata(temp.path(), "two");
    let notice = before
        .record(&session, &root, Some(&crate::ExecOutcome::Passed))
        .unwrap();
    assert!(notice.contains("Multiple new worktrees"));
    assert_eq!(session.task_root(&root), Some(old));
}

/// #2791: a linked starting checkout uses the common directory; detection
/// needs neither ambient mode nor an adoption policy, and emits no repeat.
#[test]
fn worktree_detect_2791_linked_origin_and_once_only() {
    let temp = metadata_repo();
    let original = add_metadata(temp.path(), "original");
    let before = detect::Snapshot::before(&original, &crate::Scope::All).unwrap();
    let task = add_metadata(temp.path(), "task");
    let session = WorktreeSession::default();
    let text = before
        .record(&session, &original, Some(&crate::ExecOutcome::Passed))
        .unwrap();
    assert!(text.contains("branch task"));
    assert_eq!(session.task_root(&original), Some(task));
    assert!(session.snapshot().is_none());
    let before = detect::Snapshot::before(&original, &crate::Scope::All).unwrap();
    assert!(before
        .record(&session, &original, Some(&crate::ExecOutcome::Passed))
        .is_none());
}

/// #2791: failed/unobserved execution and forged/missing reciprocal metadata
/// cannot bind; an alias entry cannot relabel an existing worktree as new.
#[test]
fn worktree_detect_2791_rejects_unproven_candidates() {
    for case in [
        "failed",
        "unobserved",
        "backlink",
        "gitfile",
        "alias",
        "head",
    ] {
        let temp = metadata_repo();
        let root = temp.path().join("main");
        let old = add_metadata(temp.path(), "old");
        let session = WorktreeSession::default();
        session.record_task_worktree(&old, "old");
        let before = detect::Snapshot::before(&root, &crate::Scope::All).unwrap();
        let task = add_metadata(temp.path(), "task");
        let admin = root.join(".git/worktrees/task");
        match case {
            "backlink" => std::fs::write(admin.join("gitdir"), "").unwrap(),
            "gitfile" => std::fs::write(task.join(".git"), "gitdir: missing").unwrap(),
            "alias" => std::fs::write(admin.join("gitdir"), "../../../../old/.git\n").unwrap(),
            "head" => std::fs::write(admin.join("HEAD"), "ref: refs/heads/../bad").unwrap(),
            _ => {}
        }
        let outcome = match case {
            "failed" => Some(&crate::ExecOutcome::Failed),
            "unobserved" => None,
            _ => Some(&crate::ExecOutcome::Passed),
        };
        assert!(before.record(&session, &root, outcome).is_none(), "{case}");
        assert_eq!(session.task_root(&root), Some(old), "{case}");
    }
}

/// #2791: discovery cannot replace or arm a confined adoption, or scan an
/// unreadable/non-directory admin list as though it had been empty.
#[test]
fn worktree_detect_2791_preserves_adoption_and_unknown_snapshot() {
    let (temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    crate::worktree_adoption::tests::link(&policy);
    let root = temp.path().join("main");
    let session = WorktreeSession::default();
    record_verified_creation(&session, policy.clone(), false);
    let before = detect::Snapshot::before(&root, &crate::Scope::All).unwrap();
    add_metadata(temp.path(), "later");
    assert!(before
        .record(&session, &root, Some(&crate::ExecOutcome::Passed))
        .is_none());
    assert_eq!(session.task_root(&root), Some(policy.worktree));
    assert!(session.snapshot().is_some());
    assert!(detect::Snapshot::before(&root, &crate::Scope::none()).is_none());
    let temp = metadata_repo();
    let root = temp.path().join("main");
    std::fs::write(root.join(".git/worktrees"), "not a directory").unwrap();
    assert!(detect::Snapshot::before(&root, &crate::Scope::All).is_none());
}
