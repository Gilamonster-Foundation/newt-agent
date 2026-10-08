//! #2733 review regressions through the actual adoption dispatch boundary.
use super::*;
use crate::worktree_adoption::{
    tests::{fixture, link},
    WorktreeSession,
};
use crate::Scope;

#[derive(Default)]
struct BuildGate(Vec<(Caveats, String)>);
impl PermissionGate for BuildGate {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        panic!("build request must retain its fence")
    }
    fn ask_with_caveats(
        &mut self,
        baseline: &Caveats,
        requests: &[PermissionRequest],
    ) -> PermissionDecision {
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].kind, DenialKind::Build);
        self.0.push((baseline.clone(), requests[0].target.clone()));
        PermissionDecision::Deny
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        panic!("unexpected question")
    }
}

pub(super) async fn dispatch(
    args: serde_json::Value,
    original: &Path,
    caveats: &Caveats,
    session: &WorktreeSession,
    gate: Option<&mut dyn PermissionGate>,
) -> (String, Option<crate::ExecOutcome>) {
    let outcome = std::sync::OnceLock::new();
    let mut display = crate::agentic::display::ToolDisplay::new(Vec::new(), false, 80, 20, false);
    let text = execute(
        &mut display,
        "run_command",
        &args,
        original.to_str().unwrap(),
        false,
        20,
        caveats,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(session),
            permission_gate: gate,
            execution: Some(&outcome),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    (text, outcome.get().copied())
}

/// #2733 P1: path operands cannot masquerade as the Git creation verb.
#[test]
fn worktree_adoption_round2_git_clean_operands_are_not_creation() {
    assert!(!creation_batch_is_read_only_after_add(
        &serde_json::json!({
            "command":"git worktree add ../task -b task; git clean -fd -- victim worktree add"
        }),
        crate::ShellEngine::Brush
    ));
}

/// #2733 P1: ground the literal classifier in real Git dispatch. The exact
/// counterexample must preserve an untracked sentinel before any adoption;
/// the legitimate display wrapper must still create and adopt the worktree.
#[cfg(unix)]
// macOS production counterpart lives in the harness=false native contract.
#[cfg(not(target_os = "macos"))]
#[tokio::test]
#[ignore = "requires native kernel confinement and git"]
async fn worktree_adoption_round2_git_clean_preserves_sentinel() {
    assert!(crate::confined_exec::kernel_fs_fence_available());
    let temp = tempfile::tempdir().unwrap();
    // Match the canonical destination spelling used by admission on macOS.
    let temp_root = dunce::canonicalize(temp.path()).unwrap();
    let root = temp_root.as_path().join("main");
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
        let out =
            crate::agentic::tools::tests::git_shell_grant::hermetic_git(&root, temp_root.as_path())
                .args(args)
                .output()
                .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    std::fs::write(root.join("victim"), "sentinel").unwrap();
    let session = WorktreeSession::default();
    let mut c = Caveats::top();
    c.net = Scope::none();
    c.fs_write = Scope::only([temp_root.as_path().to_string_lossy().into_owned()]);
    let (text, outcome) = dispatch(serde_json::json!({"command":"git worktree add ../task -b task; git clean -fd -- victim worktree add"}), &root, &c, &session, None).await;
    assert_eq!(
        std::fs::read_to_string(root.join("victim")).ok().as_deref(),
        Some("sentinel"),
        "{text}"
    );
    assert_eq!(outcome, Some(crate::ExecOutcome::Denied), "{text}");
    assert!(session.snapshot().is_none());
    assert!(!temp_root.as_path().join("task").exists());
    let (text, outcome) = dispatch(serde_json::json!({"command":"git worktree add ../task -b task 2>&1; echo ready; pwd; git branch --show-current"}), &root, &c, &session, None).await;
    assert_eq!(outcome, Some(crate::ExecOutcome::Passed), "{text}");
    assert_eq!(
        session.snapshot().expect(&text).worktree,
        temp_root.as_path().join("task").canonicalize().unwrap()
    );
}

/// #2733 P2: actual guarded dispatch must reach build admission for sibling
/// AND nested adopted roots, while original-checkout builds never reach it.
/// The recording gate stops before spawning, inspecting the real build fence.
async fn adopted_build_dispatch(source: &str) {
    for nested in [false, true] {
        let (temp, policy, _) = fixture(nested);
        link(&policy);
        let original = temp.path().join("main");
        let crate_dir = policy.worktree.join("crate");
        std::fs::create_dir(&crate_dir).unwrap();
        let session = WorktreeSession::default();
        session.adopt(policy.clone());
        let mut c = Caveats::top();
        c.fs_write = Scope::only([temp.path().to_string_lossy().into_owned()]);
        c.exec = Scope::none(); // Require explicit Build admission.
        for cwd in [&policy.worktree, &crate_dir] {
            let mut gate = BuildGate::default();
            let (text, outcome) = dispatch(
                serde_json::json!({"command":source, "cwd":cwd}),
                &original,
                &c,
                &session,
                Some(&mut gate),
            )
            .await;
            if cfg!(unix) {
                assert_eq!(gate.0.len(), 1, "nested={nested}: {text}");
                let (fence, target) = &gate.0[0];
                assert_eq!(target, policy.worktree.to_str().unwrap());
                assert!(crate::caveats::permits_path(
                    &fence.fs_write,
                    policy.worktree.to_str().unwrap()
                ));
                assert!(!crate::caveats::permits_path(
                    &fence.fs_write,
                    original.to_str().unwrap()
                ));
            } else {
                // No verified policy exists on Windows: reject before Build approval.
                assert!(gate.0.is_empty(), "{text}");
                assert!(text.contains("capability denied"), "{text}");
            }
            assert_eq!(outcome, Some(crate::ExecOutcome::Denied));
        }
        let mut gate = BuildGate::default();
        let (text, outcome) = dispatch(
            serde_json::json!({"command":source, "cwd":original}),
            &original,
            &c,
            &session,
            Some(&mut gate),
        )
        .await;
        assert!(gate.0.is_empty(), "{text}");
        assert_eq!(outcome, Some(crate::ExecOutcome::Denied), "{text}");
    }
}

/// #2733: the ordinary Cargo command also passes through lifecycle routing.
#[tokio::test]
async fn worktree_adoption_round2_argv_build_uses_adopted_root() {
    adopted_build_dispatch("cargo check").await;
}

/// #2733 P2: compound Cargo source must retain the adopted root in build_shell.
#[tokio::test]
async fn worktree_adoption_round2_shell_build_uses_adopted_root() {
    adopted_build_dispatch("cargo check; echo done").await;
}

/// #2733 P2: ground the recording-gate tests in actual confined Cargo builds
/// in sibling and nested adopted directories; original builds remain denied.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires native kernel confinement, cargo and rustc"]
async fn worktree_adoption_round2_native_builds_keep_original_read_only() {
    struct ApproveBuild;
    impl PermissionGate for ApproveBuild {
        fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
            panic!("expected scoped build approval")
        }
        fn ask_with_caveats(
            &mut self,
            c: &Caveats,
            requests: &[PermissionRequest],
        ) -> PermissionDecision {
            assert!(requests.iter().all(|r| r.kind == DenialKind::Build));
            PermissionDecision::Allow(c.clone())
        }
        fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
            panic!("unexpected question")
        }
    }
    assert!(crate::confined_exec::kernel_fs_fence_available());
    for nested in [false, true] {
        let (temp, policy, _) = fixture(nested);
        link(&policy);
        let original = temp.path().join("main");
        for root in [&original, &policy.worktree] {
            std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"adoption-build-proof\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n[lib]\npath = \"lib.rs\"\n").unwrap();
            std::fs::write(root.join("lib.rs"), "pub fn value() -> u8 { 42 }\n").unwrap();
        }
        let session = WorktreeSession::default();
        session.adopt(policy.clone());
        let mut c = Caveats::top();
        c.net = Scope::none();
        c.exec = Scope::none();
        c.fs_write = Scope::only([temp.path().to_string_lossy().into_owned()]);
        for source in ["cargo check", "cargo check && echo checked"] {
            let (text, outcome) = dispatch(
                serde_json::json!({"command":source, "cwd":policy.worktree}),
                &original,
                &c,
                &session,
                Some(&mut ApproveBuild),
            )
            .await;
            assert_eq!(
                outcome,
                Some(crate::ExecOutcome::Passed),
                "nested={nested}, source={source}: {text}"
            );
            assert!(policy.worktree.join("target").exists(), "{text}");
            let (text, outcome) = dispatch(
                serde_json::json!({"command":source, "cwd":original}),
                &original,
                &c,
                &session,
                Some(&mut ApproveBuild),
            )
            .await;
            assert_eq!(outcome, Some(crate::ExecOutcome::Denied), "{text}");
            assert!(!original.join("target").exists(), "{text}");
            assert!(!original.join("Cargo.lock").exists(), "{text}");
        }
    }
}

/// #2733 P1: a later command cannot hide an already recognized creation and
/// thereby skip the batch check (even when it changes cwd or is dynamic).
#[test]
fn worktree_adoption_round2_siblings_cannot_hide_creation() {
    let (temp, _, _) = fixture(false);
    let root = temp.path().join("main");
    for suffix in [
        "git clean -fd -- victim worktree add",
        "cd .; git clean -fd -- victim",
        "git clean -fd -- $VICTIM",
        "git worktree add ../other -b other",
    ] {
        let args =
            serde_json::json!({"command":format!("git worktree add ../task -b task; {suffix}")});
        assert_eq!(
            creation(
                "run_command",
                &args,
                root.to_str().unwrap(),
                &Caveats::top()
            )
            .is_some(),
            cfg!(unix),
            "{suffix}: {:?}",
            agent_bridle::inspect_shell(args["command"].as_str().unwrap())
        );
        assert!(
            !creation_batch_is_read_only_after_add(&args, crate::ShellEngine::Brush),
            "{suffix}"
        );
    }
}
