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
/// including destinations the confined verifier rejects.
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
