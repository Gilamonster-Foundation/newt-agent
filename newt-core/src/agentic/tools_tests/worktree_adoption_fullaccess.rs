use super::*;

/// #3274: verified adoption at the evidence-only ceiling earns one reset.
/// Inject the existing policy fixture; no filesystem or subprocess is needed.
#[test]
fn adoption_progress_resets_evidence_only_ceiling_once() {
    use crate::agentic::{NoProgress, WorkflowRuntimeState};

    for (bypass, detached) in [(false, false), (false, true), (true, false)] {
        let session = crate::worktree_adoption::WorktreeSession::default();
        let mut policy =
            crate::worktree_adoption::tests::policy_fixture("original".into(), "task".into());
        if detached {
            policy.task_branch = None;
        }
        let mut state = WorkflowRuntimeState::default();
        for round in 1..=47 {
            state.record_round_outcome(false, true);
            assert!(
                matches!(
                    state.no_progress_verdict(PromptDisposition::Act, false, Some(&session)),
                    NoProgress::Continue
                ),
                "round {round}"
            );
        }
        assert_eq!(state.rounds_without_action_progress, 47);
        record_verified_creation(&session, policy.clone(), bypass);
        state.record_round_outcome(false, true);
        assert!(
            matches!(
                state.no_progress_verdict(PromptDisposition::Act, false, Some(&session)),
                NoProgress::Continue
            ),
            "adoption must reset the ceiling: bypass={bypass}, detached={detached}"
        );
        assert_eq!(state.rounds_without_action_progress, 0);

        for round in 1..=48 {
            // A redundant receipt or branch hint cannot keep renewing progress.
            record_verified_creation(&session, policy.clone(), bypass);
            if !detached {
                session.record_task_worktree(&policy.worktree, &format!("renamed-{round}"));
            }
            state.record_round_outcome(false, true);
            let verdict = state.no_progress_verdict(PromptDisposition::Act, false, Some(&session));
            assert_eq!(matches!(verdict, NoProgress::Stop), round == 48);
            assert_eq!(state.rounds_without_action_progress, round);
        }
    }
}

/// #3274: failed/refused creation and success-looking text carry no adoption.
#[test]
fn adoption_progress_requires_session_transition() {
    use crate::agentic::{NoProgress, WorkflowRuntimeState};

    for outcome in [
        None,
        Some(crate::ExecOutcome::Denied),
        Some(crate::ExecOutcome::Failed),
    ] {
        let session = crate::worktree_adoption::WorktreeSession::default();
        let args = serde_json::json!({"command":"git worktree add -b task task"});
        nudge::record_unarmed_creation(&session, &args, ".", outcome.as_ref());
        let mut state = WorkflowRuntimeState {
            rounds_without_action_progress: 47,
            ..Default::default()
        };
        let progress = state.record_observation(
            "run_command",
            &args,
            "Adopted task worktree: task",
            true,
            outcome,
            None,
        );
        state.record_round_outcome(false, progress);
        assert!(matches!(
            state.no_progress_verdict(PromptDisposition::Act, false, Some(&session)),
            NoProgress::Stop
        ));
        assert_eq!(state.rounds_without_action_progress, 48);
    }
}

/// #3274: a lift discards pending progress; a new adoption leaves the short
/// brake intact, and its consumed signal cannot be replayed in another turn.
#[test]
fn adoption_progress_lifecycle_preserves_short_brake() {
    use crate::agentic::{NoProgress, WorkflowRuntimeState};

    let session = crate::worktree_adoption::WorktreeSession::default();
    session.record_task_worktree(Path::new("task"), "task");
    session.lift();
    assert!(!session.take_adoption_progress());

    let mut state = WorkflowRuntimeState::default();
    state.rounds_without_change = state.no_progress.stop_after - 1;
    session.record_task_worktree(Path::new("task"), "task");
    assert!(matches!(
        state.no_progress_verdict(PromptDisposition::Act, false, Some(&session)),
        NoProgress::Stop
    ));
    assert_eq!(state.rounds_without_action_progress, 0);
    assert_eq!(state.rounds_without_change, state.no_progress.stop_after);

    let mut next_turn = WorkflowRuntimeState {
        rounds_without_action_progress: 47,
        ..Default::default()
    };
    assert!(matches!(
        next_turn.no_progress_verdict(PromptDisposition::Act, false, Some(&session)),
        NoProgress::Stop
    ));
}

/// #3290: ground the mocked 71-round replay in successful native edits with
/// base read/write caveats confined to the original checkout.
#[cfg(unix)]
#[tokio::test]
async fn projected_task_progress_edit() {
    projected_task_progress("edit_file").await;
}

/// #3290: ground the same replay's tree snapshots in real confined sed writes.
#[cfg(all(target_os = "linux", feature = "confine"))]
#[tokio::test]
async fn projected_task_progress_shell() {
    projected_task_progress("run_command").await;
}

#[cfg(unix)]
async fn projected_task_progress(name: &str) {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    let _ocap = EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    let (_temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    let original = policy.worktree.parent().unwrap().join("main");
    let task = policy.worktree.clone();
    let mut candidate = Creation::before(&original, &task, &Caveats::top()).unwrap();
    candidate.prepare(&Caveats::top()).unwrap();
    crate::worktree_adoption::tests::link(&policy);
    let session = crate::worktree_adoption::WorktreeSession::default();
    record_verified_creation(&session, candidate.verify().unwrap(), false);
    let base = Caveats {
        fs_read: crate::Scope::only([original.to_string_lossy().into_owned()]),
        fs_write: crate::Scope::only([original.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    std::fs::write(task.join("probe.txt"), "before\n").unwrap();
    let mut changes = Vec::new();
    for (old, new) in [
        ("before", "first"),
        ("first", "second"),
        ("second", "second"),
    ] {
        let args = if name == "edit_file" {
            serde_json::json!({"path":"probe.txt", "old_string":old, "new_string":new})
        } else {
            serde_json::json!({"command":format!("sed -i 's/{old}/{new}/' probe.txt")})
        };
        let workspace_change = std::sync::OnceLock::new();
        let outcome = std::sync::OnceLock::new();
        let result = execute(
            &mut Notices::default(),
            name,
            &args,
            original.to_str().unwrap(),
            false,
            20,
            &base,
            &mut crate::agentic::NoMcp,
            ToolCollaborators {
                worktree_session: Some(&session),
                execution: Some(&outcome),
                workspace_change: Some(&workspace_change),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
        )
        .await;
        assert!(
            crate::agentic::tools::tool_ok(&result, outcome.get().copied()),
            "{result}"
        );
        assert_eq!(
            std::fs::read_to_string(task.join("probe.txt")).unwrap(),
            format!("{new}\n")
        );
        for scope in [&base.fs_read, &base.fs_write] {
            assert!(!newt_policy::caveats::permits_path(
                scope,
                task.to_str().unwrap()
            ));
        }
        let (before, after) = workspace_change.get().copied().unzip();
        assert!(
            before.is_some() && after.is_some(),
            "{name}: missing snapshots"
        );
        if old == new {
            assert!(
                !crate::agentic::WorkflowRuntimeState::default()
                    .record_workspace_change(before, after),
                "a no-op is not progress"
            );
        } else {
            changes.push((before, after));
        }
    }
    crate::agentic::repeat_call_guard_tests::projected_task_progress_replay(&changes);
}

struct Approve {
    asked: bool,
}
impl PermissionGate for Approve {
    fn ask_dependency_fetch(
        &mut self,
        _prepared: &newt_policy::caveats::Caveats,
        _request: &newt_policy::permissions::PermissionRequest,
    ) -> newt_policy::permissions::PermissionDecision {
        newt_policy::permissions::PermissionDecision::Deny
    }
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        self.asked = true;
        PermissionDecision::Allow(Caveats::top())
    }
    fn ask_question(&mut self, _: &str) -> newt_policy::permissions::HumanQuestionOutcome {
        newt_policy::permissions::HumanQuestionOutcome::Unavailable
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
/// #3274 grounds the mocked adoption-progress reset in actual Git creation.
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
        assert_eq!(session.take_adoption_progress(), matches!(index, 0 | 3));
        assert!(!session.take_adoption_progress());
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

    // #2791: the latest compound creation also updates relative file routing.
    assert_task_handoff(&session, &compound, "compound");
    std::fs::write(root.join("routing_probe"), "original routing marker").unwrap();
    std::fs::write(compound.join("routing_probe"), "task routing marker").unwrap();
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
    // #2780: these relative link operands intentionally belong to the original
    // checkout, not the newly selected task default. Make that base explicit.
    for (branch, args) in [
        (
            "via-c",
            serde_json::json!({"command":"git -C link worktree add -b via-c ../via-c", "cwd":root}),
        ),
        (
            "via-cwd",
            serde_json::json!({"command":"git worktree add -b via-cwd ../via-cwd", "cwd":"link"}),
        ),
        (
            "via-cd",
            serde_json::json!({"command":"cd link && git worktree add -b via-cd ../via-cd", "cwd":root}),
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
        "Task worktree: `{}` (branch {branch}) — run commands there, not in the original checkout.",
        dunce::simplified(path).display()
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
            &Default::default(),
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
        &Default::default(),
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
/// #3278: grounds the mocked progress replay in an actual task-worktree write.
#[cfg(unix)]
#[tokio::test]
async fn relative_task_root_confined() {
    relative_task_root(false).await;
}

/// #2771: recorded ambient worktrees must also become the file-tool base.
/// #3278: grounds the mocked progress replay for file and shell writes.
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
    let args = serde_json::json!({"path":"created.txt","content":"new task file\n"});
    let before = crate::agentic::progress_workspace_state(
        "write_file",
        &args,
        original.to_str().unwrap(),
        &authority.fs_read,
        Some(&session),
    )
    .await;
    let out = call("write_file", args.clone()).await;
    assert!(crate::agentic::tools::tool_ok(&out, None), "{out}");
    assert!(task.join("created.txt").is_file(), "{out}");
    assert!(!original.join("created.txt").exists());
    let after = crate::agentic::progress_workspace_state(
        "write_file",
        &args,
        original.to_str().unwrap(),
        &authority.fs_read,
        Some(&session),
    )
    .await;
    if bypass {
        let args = serde_json::json!({"command":"echo changed > shell.txt"});
        let before = crate::agentic::progress_workspace_state(
            "run_command",
            &args,
            original.to_str().unwrap(),
            &authority.fs_read,
            Some(&session),
        )
        .await;
        let out = call("run_command", args.clone()).await;
        assert!(crate::agentic::tools::tool_ok(&out, None), "{out}");
        assert!(task.join("shell.txt").is_file(), "{out}");
        assert!(!original.join("shell.txt").exists());
        let after = crate::agentic::progress_workspace_state(
            "run_command",
            &args,
            original.to_str().unwrap(),
            &authority.fs_read,
            Some(&session),
        )
        .await;
        crate::agentic::repeat_call_guard_tests::task_worktree_progress_replay(before, after);
    }
    crate::agentic::repeat_call_guard_tests::task_worktree_progress_replay(before, after);
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
        out.contains(&format!(
            "resolved to `{}`",
            dunce::simplified(&task.join("missing.txt")).display()
        )),
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
    let display_root = dunce::simplified(temp.path()).display();
    assert!(
        out.contains(&format!("resolved to `{display_root}")),
        "{out}"
    );
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

/// #2783: real handoff and compaction text must preserve a Windows-style task
/// path through the production Markdown renderer. Windows also exercises the
/// verbatim prefix returned by canonicalize; no filesystem or clock is needed.
#[cfg(feature = "markdown")]
#[test]
fn handoff_2783_task_path_survives_model_and_operator_rendering() {
    use crate::agentic::{
        apply_post_compaction_continuation, cap_exit_progress,
        compress::CompressAction,
        markdown::{render_markdown, RenderOpts},
        prompt_read::PromptReadContext,
    };
    for (plain, literal) in [
        (r"C:\repo\.worktrees\task", r"`C:\repo\.worktrees\task`"),
        (
            r"C:\repo name\.worktrees\[task]",
            r"`C:\repo name\.worktrees\[task]`",
        ),
        (
            r"C:\repo\.worktrees\task`",
            r"`` C:\repo\.worktrees\task` ``",
        ),
    ] {
        let input = if cfg!(windows) {
            format!(r"\\?\{plain}")
        } else {
            plain.into()
        };
        let session = crate::worktree_adoption::WorktreeSession::default();
        session.record_task_worktree(Path::new(&input), "task-branch");
        let captured = cap_exit_progress(Some(&session), None, None).unwrap();
        let displayed = crate::tty::width::strip_ansi(&render_markdown(
            &captured,
            RenderOpts {
                color: true,
                cols: 1000,
            },
        ));
        assert_eq!(displayed, format!("Task worktree: {plain} (branch task-branch) — run commands there, not in the original checkout."));
        assert!(captured.contains(literal), "{captured}");
        assert!(!captured.contains(r"\\?\"), "{captured}");
        for action in [CompressAction::Summarized, CompressAction::StaticFallback] {
            let mut messages =
                vec![serde_json::json!({"role":"system", "content":"compacted history"})];
            apply_post_compaction_continuation(
                Some(&session),
                &mut messages,
                &mut 1,
                action,
                None,
                PromptReadContext::new(None, "Continue the task", None),
                &Default::default(),
                true,
                |_| true,
            );
            let model = messages.last().unwrap()["content"].as_str().unwrap();
            assert!(model.contains(literal), "{model}");
            assert!(!model.contains(r"\\?\"), "{model}");
            let displayed = crate::tty::width::strip_ansi(&render_markdown(
                model,
                RenderOpts {
                    color: true,
                    cols: 1000,
                },
            ));
            assert!(displayed.contains(plain), "{displayed}");
        }
    }
}
