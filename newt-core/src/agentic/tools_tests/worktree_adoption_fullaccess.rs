use super::*;

struct Approve {
    asked: bool,
}
impl PermissionGate for Approve {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        self.asked = true;
        PermissionDecision::Allow(Caveats::top())
    }
    fn ask_question(&mut self, _: &str) -> crate::agentic::permissions::HumanQuestionOutcome {
        crate::agentic::permissions::HumanQuestionOutcome::Unavailable
    }
}

/// #2750: the reserved operator action must never produce a fake exec grant.
#[test]
fn adoption_2750_permission_target_does_not_pretend_to_lift() {
    for target in [
        "worktree-lift",
        "/permissions worktree-lift",
        "  worktree-lift\t\n",
        "\n /permissions worktree-lift \t",
    ] {
        let mut gate = Approve { asked: false };
        let (granted, replay, text) = execute_request_permissions(
            &Caveats::top(),
            &serde_json::json!({"capability":"exec", "target":target, "reason":"unblock task"}),
            Some(&mut gate),
            false,
            20,
            ".",
            None,
        );
        assert!(granted.is_none() && replay.is_none(), "{text}");
        assert!(
            !gate.asked,
            "a prompt cannot approve this non-capability target"
        );
        assert!(text.contains("/permissions worktree-lift"), "{text}");
    }
}

#[cfg(unix)]
#[derive(Default)]
struct Notices(Vec<String>);
#[cfg(unix)]
impl ToolPresentation for Notices {
    fn preview(&mut self, text: &str, _: usize) {
        self.0.push(text.into());
    }
    fn document(&mut self, _: &str) {}
    fn override_result(&mut self, _: String) {}
}

/// #2750: ground the adoption decision in real Git creation followed by actual
/// host-shell execution in the task worktree. No kernel fence is available in
/// this explicit bypass mode; arming one must not strand the session.
/// #2763 also grounds bypass admission for standalone and compound creations,
/// including destinations the confined verifier rejects. #2766 checks that the
/// actual dispatch records task location for post-compaction/captured state.
/// Selected explicitly by ci.yml's Linux `test` job (ambient worktree proof).
#[cfg(unix)]
#[tokio::test]
#[ignore = "real Git and host shell; explicit full-access adoption proof"]
async fn adoption_2750_full_access_creation_then_task_command() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let _full = EnvVar::set("NEWT_FULL_ACCESS", "1");
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "host");
    let _paths = EnvVar::set("NEWT_EXEC_PATHS", "/usr/bin:/bin");
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual_env = EnvVar::unset("VIRTUAL_ENV");
    assert!(ocap_disabled() && full_access_requested());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    let task = root.join("task");
    let compound = root.join("compound");
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
    let session = crate::worktree_adoption::WorktreeSession::default();
    let mut presentation = Notices::default();
    let mut creation_text = String::new();
    for (index, args) in [
        serde_json::json!({"command": format!("git worktree add -b task '{}'", task.display())}),
        serde_json::json!({"command":"printf adoption_2750_ready", "cwd":task}),
        serde_json::json!({"command":"printf ambient > original_probe", "cwd":root}),
        serde_json::json!({"command": format!("git worktree add -b compound '{}' && printf ready > '{}/probe'", compound.display(), compound.display())}),
    ]
    .iter()
    .enumerate()
    {
        let outcome = std::sync::OnceLock::new();
        let text = execute(
            &mut presentation,
            "run_command",
            args,
            root.to_str().unwrap(),
            false,
            20,
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
        assert_eq!(
            outcome.get(),
            Some(&crate::ExecOutcome::Passed),
            "call {index}: {text}"
        );
        if index == 0 {
            // #2766: this scheduled real-dispatch proof must exercise recording,
            // not merely seed session state before testing compaction.
            assert_task_handoff(&session, &task, "task");
            creation_text = text;
        } else if index == 1 {
            assert!(text.contains("adoption_2750_ready"), "{text}");
        }
    }
    assert!(task.join(".git").is_file());
    assert!(compound.join(".git").is_file());
    assert_eq!(
        std::fs::read_to_string(compound.join("probe")).unwrap(),
        "ready"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("original_probe")).unwrap(),
        "ambient"
    );
    assert!(session.snapshot().is_none());
    assert!(
        creation_text.contains("original checkout remains writable"),
        "{creation_text}"
    );
    assert!(presentation
        .0
        .iter()
        .any(|line| line.contains("original checkout remains writable")));

    // #2771: genuine creation enables relative file routing.
    std::fs::write(root.join("routing_probe"), "original routing marker").unwrap();
    std::fs::write(task.join("routing_probe"), "task routing marker").unwrap();
    let out = execute(
        &mut presentation,
        "read_file",
        &serde_json::json!({"path":"routing_probe"}),
        root.to_str().unwrap(),
        false,
        20,
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
    assert!(out.contains("task routing marker"), "{out}");

    // #2766 round 2: Git resolves parent traversal after following the cwd
    // symlink. The lexical sibling under `main` is a different tree.
    let elsewhere = temp.path().join("elsewhere");
    let repo = elsewhere.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
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
            crate::agentic::tools::tests::git_shell_grant::hermetic_git(&repo, temp.path())
                .args(args)
                .output()
                .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::os::unix::fs::symlink(&repo, root.join("link")).unwrap();
    for (branch, args) in [
        (
            "via-c",
            serde_json::json!({"command":"git -C link worktree add -b via-c ../via-c"}),
        ),
        (
            "via-cwd",
            serde_json::json!({"command":"git worktree add -b via-cwd ../via-cwd", "cwd":"link"}),
        ),
        (
            "via-cd",
            serde_json::json!({"command":"cd link && git worktree add -b via-cd ../via-cd"}),
        ),
    ] {
        let outcome = std::sync::OnceLock::new();
        let text = execute(
            &mut presentation,
            "run_command",
            &args,
            root.to_str().unwrap(),
            false,
            20,
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
        let actual = elsewhere.join(branch).canonicalize().unwrap();
        assert!(actual.join(".git").is_file());
        assert!(!root.join(branch).exists());
        assert_task_handoff(&session, &actual, branch);
        assert!(session.snapshot().is_none());
    }
    // #2771: a successful executable named git cannot select a foreign checkout.
    use std::os::unix::fs::PermissionsExt;
    let fake_dir = temp.path().join("fakebin");
    std::fs::create_dir(&fake_dir).unwrap();
    let fake = fake_dir.join("git");
    std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let foreign = elsewhere.join("via-c");
    std::fs::write(foreign.join("routing_probe"), "foreign routing marker").unwrap();
    let outcome = std::sync::OnceLock::new();
    let out = execute(&mut presentation, "run_command",
        &serde_json::json!({"command":format!("'{}' worktree add -b forged '{}'", fake.display(), foreign.display())}),
        root.to_str().unwrap(), false, 20, &Caveats::top(), &mut crate::agentic::NoMcp,
        ToolCollaborators { worktree_session: Some(&session), execution: Some(&outcome), ..Default::default() }, false, PromptDisposition::Act).await;
    assert_eq!(outcome.get(), Some(&crate::ExecOutcome::Passed), "{out}");
    assert!(session
        .task_hint()
        .unwrap()
        .contains(foreign.to_string_lossy().as_ref()));
    let out = execute(
        &mut presentation,
        "read_file",
        &serde_json::json!({"path":"routing_probe"}),
        root.to_str().unwrap(),
        false,
        20,
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
    assert!(out.contains("original routing marker"), "{out}");
}

/// #2750: ambient bypass stays unarmed; full authority with confinement still
/// adopts and narrows. The dispatch-owned decision does not change policy roots.
#[test]
fn adoption_2750_decision_keeps_confined_protection() {
    for bypass in [false, true] {
        let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
        let session = crate::worktree_adoption::WorktreeSession::default();
        let notice = record_verified_creation(&session, policy, bypass);
        assert_eq!(session.snapshot().is_none(), bypass, "{notice}");
        assert_eq!(
            notice.contains("original checkout remains writable"),
            bypass
        );
        assert_eq!(
            notice.contains("original checkout and shared config are now read-only"),
            !bypass
        );
        assert_eq!(
            notice.contains("git -c user.name=… -c user.email=…"),
            !bypass
        );
        assert_eq!(notice.contains("Any uncommitted changes"), !bypass);
        assert_eq!(notice.contains("/permissions worktree-lift"), !bypass);
    }
}

/// #2763: bypass routing must not depend on platform metadata verification or
/// trusted Git resolution. Confined attempts still fail closed on opaque batches.
#[test]
fn adoption_2763_bypass_skips_creation_verification() {
    for command in [
        "git worktree add -b task task",
        "git worktree add -b task task && echo ready > task/probe",
    ] {
        let args = serde_json::json!({"command": command});
        let admission = creation_admission(
            true,
            "run_command",
            &args,
            ".",
            &Caveats::top(),
            crate::ShellEngine::Host,
            |_| panic!("bypass must not resolve trusted Git"),
        );
        assert!(matches!(admission, Ok(None)), "{command}");
    }
    assert!(creation_admission(
        false,
        "run_command",
        &serde_json::json!({"command":"git worktree add -b task task && echo ready > task/probe"}),
        ".",
        &Caveats::top(),
        crate::ShellEngine::Host,
        |_| panic!("confined compound must fail before Git resolution"),
    )
    .is_err());
}

/// #2766: verified confined creation survives both summary and static-fallback
/// compaction, and appears in the captured working state without a scratchpad.
#[test]
fn compaction_task_worktree_confined() {
    compaction_task_worktree_after_creation(false);
}

/// #2766: recording the task location must not depend on arming adoption.
#[test]
fn compaction_task_worktree_unarmed() {
    compaction_task_worktree_after_creation(true);
}

fn compaction_task_worktree_after_creation(bypass: bool) {
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    let worktree = policy.worktree.clone();
    let session = crate::worktree_adoption::WorktreeSession::default();
    record_verified_creation(&session, policy, bypass);
    assert_eq!(session.snapshot().is_none(), bypass);
    assert_task_handoff(&session, &worktree, "task");
}

fn assert_task_handoff(
    session: &crate::worktree_adoption::WorktreeSession,
    path: &Path,
    branch: &str,
) {
    use crate::agentic::{
        apply_post_compaction_continuation, cap_exit_progress, compress::CompressAction,
        prompt_read::PromptReadContext,
    };
    let expected = format!(
        "Task worktree: {} (branch {branch}) — run commands there, not in the original checkout.",
        path.display()
    );
    for action in [CompressAction::Summarized, CompressAction::StaticFallback] {
        let mut messages =
            vec![serde_json::json!({"role":"system", "content":"compacted history"})];
        apply_post_compaction_continuation(
            Some(session),
            &mut messages,
            &mut 1,
            action,
            None,
            PromptReadContext::new(None, "Refactor in a new worktree", None),
            true,
            |_| true,
        );
        assert!(
            messages.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains(&expected),
            "{messages:?}"
        );
    }
    let mut rejected = vec![serde_json::json!({"role":"system", "content":"compacted history"})];
    let before = rejected.clone();
    apply_post_compaction_continuation(
        Some(session),
        &mut rejected,
        &mut 1,
        CompressAction::Summarized,
        None,
        PromptReadContext::new(None, "Refactor in a new worktree", None),
        true,
        |candidate| {
            assert!(candidate.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains(&expected));
            false
        },
    );
    assert_eq!(
        rejected, before,
        "the hint must obey the existing budget gate"
    );
    let captured = cap_exit_progress(Some(session), None, None).unwrap_or_default();
    assert!(captured.contains(&expected), "{captured}");
}

/// #2771: native dispatch regression for relative file operands after confined
/// creation. Grounds path routing in real files while retaining adoption fences.
#[cfg(unix)]
#[tokio::test]
async fn relative_task_root_confined() {
    relative_task_root(false).await;
}

/// #2771: recorded ambient worktrees must also become the file-tool base.
#[tokio::test]
async fn relative_task_root_ambient() {
    relative_task_root(true).await;
}

async fn relative_task_root(bypass: bool) {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", if bypass { "1" } else { "0" });
    let (temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    let original = temp.path().join("main").canonicalize().unwrap();
    let task = policy.worktree.clone();
    let admin = original.join(".git/worktrees/task");
    std::fs::write(task.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(admin.join("gitdir"), task.join(".git").to_str().unwrap()).unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/task\n").unwrap();
    std::fs::write(original.join("probe.txt"), "original marker\n").unwrap();
    std::fs::write(task.join("probe.txt"), "task marker\n").unwrap();
    let session = crate::worktree_adoption::WorktreeSession::default();
    let authority = Caveats {
        fs_write: crate::Scope::only([
            original.to_string_lossy().into_owned(),
            task.to_string_lossy().into_owned(),
        ]),
        ..Caveats::top()
    };
    struct Quiet;
    impl ToolPresentation for Quiet {
        fn preview(&mut self, _: &str, _: usize) {}
        fn document(&mut self, _: &str) {}
        fn override_result(&mut self, _: String) {}
    }
    let call = |name: &'static str, args: serde_json::Value| {
        let session = &session;
        let authority = &authority;
        let original = &original;
        async move {
            execute(
                &mut Quiet,
                name,
                &args,
                original.to_str().unwrap(),
                false,
                20,
                authority,
                &mut crate::agentic::NoMcp,
                ToolCollaborators {
                    worktree_session: Some(session),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
            )
            .await
        }
    };
    let read = || serde_json::json!({"path":"probe.txt"});
    assert!(call("read_file", read()).await.contains("original marker"));
    let out = call(
        "write_file",
        serde_json::json!({"path":"before.txt", "content":"original"}),
    )
    .await;
    assert!(original.join("before.txt").exists(), "{out}");
    assert!(!task.join("before.txt").exists());
    record_verified_creation(&session, policy, bypass);
    assert_eq!(session.snapshot().is_none(), bypass);
    let out = call("read_file", read()).await;
    assert!(out.contains("task marker"), "{out}");
    let out = call(
        "write_file",
        serde_json::json!({"path":"created.txt","content":"new task file\n"}),
    )
    .await;
    assert!(task.join("created.txt").is_file(), "{out}");
    assert!(!original.join("created.txt").exists());
    let out = call("edit_file", serde_json::json!({"path":"probe.txt","old_string":"task marker","new_string":"edited task"})).await;
    assert_eq!(
        std::fs::read_to_string(task.join("probe.txt")).unwrap(),
        "edited task\n",
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(original.join("probe.txt")).unwrap(),
        "original marker\n"
    );
    let out = call("list_dir", serde_json::json!({"path":"."})).await;
    assert!(
        out.contains("created.txt") && !out.contains("before.txt"),
        "{out}"
    );
    let out = call(
        "find",
        serde_json::json!({"path":".", "name":"created.txt"}),
    )
    .await;
    assert!(out.contains("created.txt"), "{out}");
    let out = call(
        "grep",
        serde_json::json!({"path":".", "pattern":"edited task"}),
    )
    .await;
    assert!(out.contains("edited task"), "{out}");
    for (tool, args, expected) in [
        (
            "find",
            serde_json::json!({"path":original, "name":"before.txt"}),
            "before.txt",
        ),
        (
            "grep",
            serde_json::json!({"path":original, "pattern":"original marker"}),
            "original marker",
        ),
    ] {
        let out = call(tool, args).await;
        assert!(out.contains(expected), "absolute {tool}: {out}");
    }
    // The absolute search boundary has not expanded to the sibling task.
    for tool in ["find", "grep"] {
        let out = call(
            tool,
            serde_json::json!({"path":task, "pattern":"edited task"}),
        )
        .await;
        assert!(out.contains("workspace-only"), "absolute {tool}: {out}");
    }
    let out = call("write_file", serde_json::json!({"path":"copied.txt", "content":"", "copy_from":{"path":"probe.txt", "start_line":1, "end_line":1}})).await;
    assert_eq!(
        std::fs::read_to_string(task.join("copied.txt")).unwrap(),
        "edited task\n",
        "{out}"
    );
    let out = call("delete_file", serde_json::json!({"path":"created.txt"})).await;
    assert!(!task.join("created.txt").exists(), "{out}");
    let out = call("read_file", serde_json::json!({"path":"missing.txt"})).await;
    assert!(
        out.contains(&format!("resolved against {}", task.display())),
        "{out}"
    );
    let out = call(
        "read_file",
        serde_json::json!({"path":original.join("probe.txt")}),
    )
    .await;
    assert!(
        out.contains("original marker"),
        "absolute reads unchanged: {out}"
    );
    let out = call(
        "write_file",
        serde_json::json!({"path":original.join("absolute.txt"),"content":"absolute"}),
    )
    .await;
    assert_eq!(original.join("absolute.txt").exists(), bypass, "{out}");
    if !bypass {
        assert!(out.contains("read-only"), "{out}");
    }
    session.lift();
    assert!(call("read_file", read()).await.contains("original marker"));
}

/// #2771: a missing file must name the base used even without a task worktree.
#[test]
fn relative_task_root_missing_error_names_base() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_str().unwrap();
    let out = FileIoError::io(
        &std::io::ErrorKind::NotFound.into(),
        &temp.path().join("missing.txt"),
        "error: missing file".into(),
    )
    .render(root, &crate::Scope::All);
    assert!(out.contains(&format!("resolved against {root}")), "{out}");
}

/// #2771 round 2: unlinked advisory roots must not redirect native file tools,
/// even when ambient file authority permits both destinations.
#[tokio::test]
async fn relative_task_root_unlinked_is_advisory_only() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    for bypass in ["0", "1"] {
        let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", bypass);
        let (temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
        let root = temp.path().join("main");
        let task = policy.worktree;
        std::fs::write(root.join("probe"), "original").unwrap();
        std::fs::write(task.join("probe"), "forged").unwrap();
        let session = crate::worktree_adoption::WorktreeSession::default();
        session.record_task_worktree(&task, "task");
        struct Quiet;
        impl ToolPresentation for Quiet {
            fn preview(&mut self, _: &str, _: usize) {}
            fn document(&mut self, _: &str) {}
            fn override_result(&mut self, _: String) {}
        }
        let out = execute(
            &mut Quiet,
            "read_file",
            &serde_json::json!({"path":"probe"}),
            root.to_str().unwrap(),
            false,
            20,
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
        assert_eq!(out.trim(), "original", "{bypass}: {out}");
        assert!(session.task_hint().is_some());
    }
}
