//! #2733: recognition must not disappear behind an ambiguous earlier command.
#[cfg(unix)]
use super::round2_tests::dispatch;
use super::*;
#[cfg(unix)]
use crate::{worktree_adoption::WorktreeSession, Scope};

/// Ground fail-closed creation recognition in actual Git dispatch: neither
/// prefix may let git clean delete the original sentinel before adoption.
#[cfg(unix)]
async fn prefix_preserves_sentinel(prefix: &str) {
    assert!(crate::confined_exec::kernel_fs_fence_available());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
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
    std::fs::write(root.join("victim"), "sentinel").unwrap();
    let session = WorktreeSession::default();
    let mut c = Caveats::top();
    c.net = Scope::none();
    c.fs_write = Scope::only([temp.path().to_string_lossy().into_owned()]);
    let args = serde_json::json!({"command":format!("{prefix}; git worktree add ../task -b task; git clean -fd -- victim")});
    let (text, outcome) = dispatch(args, &root, &c, &session, None).await;
    assert_eq!(
        std::fs::read_to_string(root.join("victim")).ok().as_deref(),
        Some("sentinel"),
        "{text}"
    );
    assert_eq!(outcome, Some(crate::ExecOutcome::Denied), "{text}");
    assert!(text.contains("standalone"), "{text}");
    assert!(session.snapshot().is_none());
    assert!(!temp.path().join("task").exists());
    // The refusal must not poison the later, ordinary creation/adoption path.
    let (text, outcome) = dispatch(
        serde_json::json!({"command":"git worktree add ../task -b task"}),
        &root,
        &c,
        &session,
        None,
    )
    .await;
    assert_eq!(outcome, Some(crate::ExecOutcome::Passed), "{text}");
    assert_eq!(
        session.snapshot().expect(&text).worktree,
        temp.path().join("task").canonicalize().unwrap()
    );
}

/// #2733 round 3: a preceding cwd command must not erase creation recognition.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires native kernel confinement and git"]
async fn worktree_adoption_round3_cd_prefix_preserves_sentinel() {
    prefix_preserves_sentinel("pwd; cd .").await;
}

/// #2733 round 3: a nonliteral earlier Git operand must not erase recognition.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires native kernel confinement and git"]
async fn worktree_adoption_round3_dynamic_git_prefix_preserves_sentinel() {
    prefix_preserves_sentinel("git status -- $UNSET").await;
}

/// #2733: the same nonliteral prefix with an already-allowed shell variable
/// demonstrates the boundary independently of the shell's $UNSET allowlist.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires native kernel confinement and git"]
async fn worktree_adoption_round3_allowed_variable_preserves_sentinel() {
    prefix_preserves_sentinel("git status -- $PWD").await;
}

/// #2733 round 3: ambiguity must be an error, not absence. These deterministic
/// controls run routinely and stop before any shell or Git execution.
#[test]
fn worktree_adoption_round3_ambiguous_attempts_fail_closed() {
    let (temp, _, _) = crate::worktree_adoption::tests::fixture(false);
    let root = temp.path().join("main");
    for source in [
        "pwd; cd .; git worktree add ../task -b task; git clean -fd -- victim",
        "git status -- $UNSET; git worktree add ../task -b task; git clean -fd -- victim",
        "git status -- $PWD; git worktree add ../task -b task",
        "pwd; cd .; git worktree add ../task -b task",
        "git worktree add $DEST -b task",
        "git $OPTIONS worktree add ../task -b task",
        "git -c core.hooksPath=hooks worktree add ../task -b task",
        "python3 mutate.py; git worktree add ../task -b task",
        "python3 mutate.py; g'it' work'tree' a'dd' ../task -b task",
        "git worktree add ../task -b 'unterminated",
        "echo $(git worktree add ../task -b task)",
        "git worktree add ../task -b task; head < <(git clean -fd -- victim)",
    ] {
        assert!(possible_creation(source), "not recognized: {source}");
        assert!(
            creation_admission(
                "run_command",
                &serde_json::json!({"command":source}),
                root.to_str().unwrap(),
                &Caveats::top()
            )
            .is_err(),
            "fell open: {source}"
        );
    }
}

/// #2733 round 3: standalone creation and known display wrappers still admit;
/// unrelated commands and merely quoting creation text do not become attempts.
#[test]
fn worktree_adoption_round3_exact_creation_and_noncreation_controls() {
    let (temp, _, _) = crate::worktree_adoption::tests::fixture(false);
    let root = temp.path().join("main");
    for source in [
        "git worktree add ../task -b task".to_owned(),
        format!("git -C '{}' worktree add ../task -b task", root.display()),
        "pwd; git worktree add ../task -b task 2>&1 | tail -5; git branch --show-current"
            .to_owned(),
    ] {
        let admission = creation_admission(
            "run_command",
            &serde_json::json!({"command":source}),
            root.to_str().unwrap(),
            &Caveats::top(),
        );
        if cfg!(unix) {
            assert!(
                admission.is_ok_and(|candidate| candidate.is_some()),
                "{source}"
            );
        } else {
            assert!(
                admission.is_err(),
                "unsupported metadata reads must refuse: {source}"
            );
        }
    }
    for source in [
        "git status -- $UNSET",
        "pwd; cd .",
        "echo 'git worktree add ../task'",
        "git worktree list",
    ] {
        assert!(!possible_creation(source), "{source}");
        assert!(
            creation_admission(
                "run_command",
                &serde_json::json!({"command":source}),
                root.to_str().unwrap(),
                &Caveats::top()
            )
            .is_ok_and(|candidate| candidate.is_none()),
            "{source}"
        );
    }
}
