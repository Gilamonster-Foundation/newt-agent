//! #2733: admission tests must not depend on host Git or shell-engine selection.
use super::*;

/// #2733 round 6: injected trust and engine decisions drive the classifier,
/// including the exact CI wrapper. Rejection is an error, never “no creation.”
#[test]
fn worktree_adoption_round6_injected_trust_and_engine_matrix() {
    let (temp, _, _) = crate::worktree_adoption::tests::fixture(false);
    let root = temp.path().join("main");
    for engine in [
        crate::ShellEngine::Brush,
        crate::ShellEngine::Host,
        crate::ShellEngine::SafeSubset,
    ] {
        for trusted in [false, true] {
            for wrapped in [false, true] {
                let source = if wrapped {
                    "pwd; git worktree add ../task -b task 2>&1; echo ready; pwd; git branch --show-current"
                } else {
                    "git worktree add ../task -b task; git branch --show-current"
                };
                let admission = creation_admission(
                    false,
                    "run_command",
                    &serde_json::json!({"command":source}),
                    root.to_str().unwrap(),
                    &Caveats::top(),
                    engine,
                    |_| {
                        if trusted {
                            Ok(PathBuf::from("/fixture/git"))
                        } else {
                            Err(())
                        }
                    },
                );
                if cfg!(unix) && trusted && (!wrapped || engine != crate::ShellEngine::SafeSubset) {
                    let (_, pinned) = admission.ok().flatten().expect(source);
                    assert!(pinned.contains("/fixture/git"), "{pinned}");
                } else {
                    assert!(
                        admission.is_err(),
                        "must refuse, not erase creation: {source}"
                    );
                }
            }
        }
        let no_creation = creation_admission(
            false,
            "run_command",
            &serde_json::json!({"command":"git branch --show-current"}),
            root.to_str().unwrap(),
            &Caveats::top(),
            engine,
            |_| panic!("noncreation must not resolve Git for adoption"),
        );
        assert!(no_creation.is_ok_and(|candidate| candidate.is_none()));
    }
}

/// #2733 round 6: ground injected rejection in the real resolver + dispatch.
/// A controlled writable Git image must cause the standalone refusal before
/// any execution, even though bare branch --show-current is a valid sibling.
#[cfg(unix)]
#[tokio::test]
async fn worktree_adoption_round6_untrusted_host_git_refuses_dispatch() {
    use crate::agentic::tools::disable_ocap_tests::EnvVar;
    use std::os::unix::fs::PermissionsExt;
    let _env = crate::process_env::lock();
    let (temp, _, _) = crate::worktree_adoption::tests::fixture(false);
    let root = temp.path().join("main");
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(&git, "must never execute").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o777)).unwrap();
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual_env = EnvVar::unset("VIRTUAL_ENV");
    let _paths = EnvVar::set("NEWT_EXEC_PATHS", bin.to_str().unwrap());
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    let caveats = Caveats::top();
    assert!(git_identity::resolve(&caveats).is_err());
    let args = serde_json::json!({"command":"git worktree add ../fresh -b fresh; git branch --show-current"});
    assert!(creation("run_command", &args, root.to_str().unwrap(), &caveats).is_some());
    let session = crate::worktree_adoption::WorktreeSession::default();
    std::fs::write(root.join("sentinel"), "unchanged").unwrap();
    let (text, outcome) =
        super::round2_tests::dispatch(args, &root, &caveats, &session, None).await;
    assert_eq!(outcome, Some(crate::ExecOutcome::Denied), "{text}");
    assert!(text.contains("standalone"), "{text}");
    assert!(session.snapshot().is_none());
    assert!(!temp.path().join("fresh").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("sentinel")).unwrap(),
        "unchanged"
    );
}

/// #2810: a literal bounded stdin display can accompany creation; operands,
/// dynamic evaluation, writes and follow mode cannot certify a read-only batch.
#[test]
fn worktree_2810_tail_shape_is_bounded_and_literal() {
    for tail in ["tail -5", "tail -n 5"] {
        assert!(
            creation_batch_is_read_only_after_add(
                &serde_json::json!({"command":format!("git worktree add -b task ../task 2>&1 | {tail} && git status --short")}),
                crate::ShellEngine::Brush,
            ),
            "{tail}"
        );
    }
    for tail in [
        "tail -f",
        "tail -5 victim",
        "tail -n +5",
        "tail -0",
        "tail -$N",
        "tail -5 > victim",
        "./tail -5",
        "tail $(echo -5)",
        "tail -5 && git clean -fd",
        "tail -5 && git worktree add -b other ../other",
    ] {
        assert!(
            !creation_batch_is_read_only_after_add(
                &serde_json::json!({"command":format!("git worktree add -b task ../task 2>&1 | {tail}")}),
                crate::ShellEngine::Brush,
            ),
            "{tail}"
        );
    }
}

/// #2810: the real resolver must reject a writable PATH replacement, even
/// when its literal argv would be a valid display. No creation may start.
#[cfg(unix)]
#[test]
fn worktree_2810_writable_tail_is_not_a_display_identity() {
    use crate::agentic::tools::disable_ocap_tests::EnvVar;
    use std::os::unix::fs::PermissionsExt;
    let _env = crate::process_env::lock();
    let (temp, _, _) = crate::worktree_adoption::tests::fixture(false);
    let root = temp.path().join("main");
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let tail = bin.join("tail");
    std::fs::write(&tail, "must never execute").unwrap();
    std::fs::set_permissions(&tail, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual_env = EnvVar::unset("VIRTUAL_ENV");
    let _paths = EnvVar::set("NEWT_EXEC_PATHS", bin.to_str().unwrap());
    let caveats = Caveats {
        fs_write: crate::Scope::only([temp.path().to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    let admission = creation_admission(
        false,
        "run_command",
        &serde_json::json!({"command":"git worktree add -b task ../fresh 2>&1 | tail -5 && git status --short"}),
        root.to_str().unwrap(),
        &caveats,
        crate::ShellEngine::Brush,
        |_| Ok(PathBuf::from("/fixture/git")),
    );
    assert!(admission.is_err());
    assert!(!temp.path().join("fresh").exists());
}
