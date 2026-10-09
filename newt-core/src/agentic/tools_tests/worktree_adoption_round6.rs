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
                &serde_json::json!({"command":format!("git worktree add -b task ../task 2>&1 | {tail} && git branch --show-current")}),
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
        &serde_json::json!({"command":"git worktree add -b task ../fresh 2>&1 | tail -5 && git branch --show-current"}),
        root.to_str().unwrap(),
        &caveats,
        crate::ShellEngine::Brush,
        |_| Ok(PathBuf::from("/fixture/git")),
    );
    assert!(admission.is_err());
    assert!(!temp.path().join("fresh").exists());
}

/// #2810 round 2: admission must keep tail's stdin attached to the pipeline,
/// not a file or replacement descriptor. Git's own 2>&1 remains supported.
#[cfg(unix)]
#[test]
fn worktree_2810_admission_refuses_tail_input_redirects() {
    use crate::agentic::tools::disable_ocap_tests::EnvVar;
    let _env = crate::process_env::lock();
    let (temp, _, _) = crate::worktree_adoption::tests::fixture(false);
    let root = temp.path().join("main");
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual_env = EnvVar::unset("VIRTUAL_ENV");
    let _paths = EnvVar::set("NEWT_EXEC_PATHS", "/usr/bin:/bin");
    let caveats = Caveats {
        fs_write: crate::Scope::only([temp.path().to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    let admit = |redirect: &str| {
        creation_admission(
            false,
            "run_command",
            &serde_json::json!({"command":format!("git worktree add -b task ../fresh 2>&1 | tail -5 {redirect} && git branch --show-current")}),
            root.to_str().unwrap(),
            &caveats,
            crate::ShellEngine::Brush,
            |_| Ok(PathBuf::from("/fixture/git")),
        )
    };
    assert!(admit("").is_ok_and(|candidate| candidate.is_some()));
    let accepted: Vec<_> = ["< victim", "<&3", "0<&3"]
        .into_iter()
        .filter(|redirect| admit(redirect).is_ok())
        .collect();
    assert!(accepted.is_empty(), "admitted tail redirects: {accepted:?}");
}

/// #2810/#2812 follow-up: admit only fixed read-only Git sibling forms.
#[test]
fn readonly_git_sibling_shapes() {
    for sibling in [
        "git branch --show-current",
        "git rev-parse HEAD",
        "git rev-parse --abbrev-ref HEAD",
        "git rev-parse --show-toplevel",
        "git worktree list",
    ] {
        assert!(
            creation_batch_is_read_only_after_add(
                &serde_json::json!({"command":format!("git worktree add -b task ../task HEAD 2>&1 | tail -5 && {sibling}")}),
                crate::ShellEngine::Brush
            ),
            "{sibling}"
        );
    }
}

/// #2810/#2812: no config/alias/pager/env surface or sibling redirection may
/// run before the original-checkout fence is armed.
#[test]
fn readonly_git_sibling_negatives() {
    for sibling in [
        "git status",
        "git status --short",
        "git status -s",
        "git status --porcelain",
        "git log --oneline -n 5",
        "git log --oneline -5",
        "git commit -m bad",
        "git checkout -b bad",
        "git branch bad",
        "git st",
        "git -c core.pager=cat branch --show-current",
        "git --exec-path=/tmp branch --show-current",
        "git --paginate log --oneline -5",
        "GIT_PAGER=cat git branch --show-current",
        "env GIT_DIR=/tmp git branch --show-current",
        "git branch --show-current > marker",
        "git branch --show-current < marker",
        "git branch --show-current 2>&1",
        "git branch --show-current <&3",
        "git rev-parse --git-path config",
        "git log --oneline -0",
        "git log --oneline -n +5",
        "git log --oneline -n $N",
        "git log --oneline -5 --output=marker",
        "git log --oneline -5 --format=%x00",
        "git worktree list --porcelain",
    ] {
        assert!(
            !creation_batch_is_read_only_after_add(
                &serde_json::json!({"command":format!("git worktree add -b task ../task && {sibling}")}),
                crate::ShellEngine::Brush
            ),
            "{sibling}"
        );
    }
}

/// #2810/#2812: dots in cwd/destination/prose are not the start operand. Every
/// admitted sibling must use the injected trusted Git, with pagers/locks off.
#[test]
fn readonly_git_pins_siblings_and_only_normalizes_dot_start() {
    for start in [
        ".",
        "'.'",
        "\".\"",
        "HEAD",
        "main",
        "0123456789012345678901234567890123456789abcd",
    ] {
        let source = format!(
            "git -C . worktree add -b task ../task {start} 2>&1 && git branch --show-current"
        );
        let pinned =
            git_identity::pin(&source, Path::new("/trusted tool/git"), &Caveats::top()).unwrap();
        let start = if [".", "'.'", "\".\""].contains(&start) {
            "HEAD"
        } else {
            start
        };
        assert_eq!(pinned, format!("'/trusted tool/git' -C . worktree add -b task ../task {start} 2>&1 && '/trusted tool/git' --no-pager --no-optional-locks branch --show-current"));
    }
    for source in [
        "git worktree add -b task .",
        "git worktree add -b task ../task && echo '.'",
    ] {
        let pinned =
            git_identity::pin(source, Path::new("/trusted tool/git"), &Caveats::top()).unwrap();
        assert!(!pinned.contains("HEAD"), "{pinned}");
    }
}

/// #2810/#2812: only creation's stderr-to-stdout duplication is admitted.
#[test]
fn readonly_git_redirect_boundary() {
    for redirect in ["< marker", "<&3", "1>&2", "2>&3", "> marker", "<<< text"] {
        let args = serde_json::json!({"command":format!("git worktree add -b task ../task {redirect} && git branch --show-current")});
        assert!(
            !creation_batch_is_read_only_after_add(&args, crate::ShellEngine::Brush),
            "{redirect}"
        );
    }
}

#[cfg(target_os = "linux")]
#[path = "worktree_readonly_config.rs"]
mod hostile_config;
