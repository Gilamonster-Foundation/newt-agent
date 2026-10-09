//! #2733 round 5: every Git authorization requires an authenticated identity.
#[cfg(unix)]
use super::round2_tests::dispatch;
use super::*;
#[cfg(unix)]
use crate::{worktree_adoption::WorktreeSession, Scope};

/// #2733: no path-qualified or basename-equivalent Git can authorize a batch.
#[test]
fn worktree_adoption_round5_git_identity_sweep() {
    for program in [
        "/authorized/bin/git",
        "./git",
        "bin/git",
        "git.exe",
        "GIT",
        r"'C:\authorized\git.exe'",
    ] {
        for suffix in [
            "status",
            "status --short",
            "worktree list",
            "branch --show-current",
        ] {
            let source = format!("git worktree add ../task -b task; {program} {suffix}");
            assert!(
                !creation_batch_is_read_only_after_add(
                    &serde_json::json!({"command": source}),
                    crate::ShellEngine::Brush
                ),
                "{source}"
            );
        }
        let source = format!("{program} worktree add ../task -b task");
        assert!(
            possible_creation(&source),
            "recognition must fail closed: {source}"
        );
        assert!(
            !creation_batch_is_read_only_after_add(
                &serde_json::json!({"command": source}),
                crate::ShellEngine::Brush
            ),
            "{source}"
        );
    }
    assert!(creation_batch_is_read_only_after_add(
        &serde_json::json!({"command": "git worktree add ../task -b task; git status"}),
        crate::ShellEngine::Brush
    ));
}

/// #2733 round 5: ground the Git identity classifier in real dispatch. A native rm
/// image named git must not delete the sentinel before the adoption boundary.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires native kernel confinement and git"]
async fn worktree_adoption_round5_replacement_preserves_sentinel() {
    assert!(crate::confined_exec::kernel_fs_fence_available());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    let bin = temp.path().join("authorized/bin");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy("/bin/rm", bin.join("git")).unwrap();
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
        let out = crate::agentic::tools::tests::git_shell_grant::hermetic_git(&root, temp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    std::fs::write(root.join("status"), "sentinel").unwrap();
    let session = WorktreeSession::default();
    let mut c = Caveats::top();
    c.net = Scope::none();
    c.fs_write = Scope::only([temp.path().to_string_lossy().into_owned()]);
    let (text, outcome) = dispatch(
        serde_json::json!({"command": format!("git worktree add ../task -b task; '{}' status", bin.join("git").display())}),
        &root, &c, &session, None,
    ).await;
    assert_eq!(
        std::fs::read_to_string(root.join("status")).ok().as_deref(),
        Some("sentinel"),
        "{text}"
    );
    assert_eq!(outcome, Some(crate::ExecOutcome::Denied), "{text}");
    assert!(text.contains("standalone"), "{text}");
    assert!(!temp.path().join("task").exists());
    assert!(session.snapshot().is_none());
    let (text, outcome) = dispatch(
        serde_json::json!({"command": "pwd; git worktree add ../task -b task; git status; echo ready"}),
        &root,
        &c,
        &session,
        None,
    )
    .await;
    assert_eq!(outcome, Some(crate::ExecOutcome::Passed), "{text}");
    assert!(session.snapshot().is_some(), "{text}");
}

/// #2733: bind executable tokens, not argument text, while keeping shell order.
#[test]
fn worktree_adoption_round5_pins_every_git_word_without_changing_operands() {
    let source =
        "pwd; git worktree add ../task -b task 2>&1 && echo 'git status; café' ; 'git' status";
    let pinned =
        git_identity::pin(source, Path::new("/trusted tool/bin/git"), &Caveats::top()).unwrap();
    assert_eq!(pinned, "pwd; '/trusted tool/bin/git' worktree add ../task -b task 2>&1 && echo 'git status; café' ; '/trusted tool/bin/git' --no-pager --no-optional-locks status");
    for source in [
        "# git status\ngit status",
        "PATH=/authorized/bin git status",
        "git worktree add ../task & git status",
        "for x in git; do git status; done",
        "echo $(git status)",
    ] {
        assert!(
            git_identity::pin(source, Path::new("/trusted tool/bin/git"), &Caveats::top()).is_err(),
            "{source}"
        );
    }
}

/// #2733: reuse staging's filesystem trust; an executable in a model-writable
/// PATH directory cannot become Git merely by having that filename.
#[cfg(unix)]
#[test]
fn worktree_adoption_round5_lookup_rejects_untrusted_and_relative_paths() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let fake = temp.path().join("git");
    std::fs::write(&fake, "not executed").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut c = Caveats::top();
    c.fs_write = Scope::only([temp.path().to_string_lossy().into_owned()]);
    assert!(git_identity::authenticate(temp.path().as_os_str(), &c).is_err());
    for path in ["", ".:/usr/bin", "/usr/bin:.", "/missing-program-directory"] {
        assert!(
            git_identity::authenticate(std::ffi::OsStr::new(path), &c).is_err(),
            "{path}"
        );
    }
    // Without the write-root exclusion, unsafe fixture permissions still deny.
    c.fs_write = Scope::none();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(git_identity::authenticate(temp.path().as_os_str(), &c).is_err());
}

/// #2733: direct candidate extraction cannot independently admit an impostor.
#[test]
fn worktree_adoption_round5_candidate_rejects_path_qualified_git() {
    let (temp, _, _) = crate::worktree_adoption::tests::fixture(false);
    let root = temp.path().join("main");
    for program in ["/authorized/bin/git", "./git", "git.exe", "GIT"] {
        assert!(creation(
            "run_command",
            &serde_json::json!({"command": format!("{program} worktree add ../task -b task")}),
            root.to_str().unwrap(),
            &Caveats::top()
        )
        .is_none());
    }
}
