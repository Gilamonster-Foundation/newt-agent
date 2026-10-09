use super::*;

/// #2831: once-per-branch and governed PR facts are independent of content evidence.
#[test]
fn publish_early_once_per_branch_requires_observed_facts() {
    let mut state = State::default();
    assert!(state.observe("task", false, false).is_none());
    assert_eq!(state.observe("task", false, true), Some(HINT));
    assert!(state.observe("task", false, true).is_none());
    assert_eq!(state.observe("other", false, true), Some(HINT));
    assert!(state.observe("published", true, true).is_none());
    assert!(state.observe("published", false, true).is_none());
}

/// The measured 6096-line copy gets advice, not refusal; ordinary extractions do not.
#[test]
fn bounded_move_threshold_is_advisory() {
    assert_eq!(bounded_move_hint(1500), "");
    assert!(bounded_move_hint(1501).contains("split"));
    assert!(bounded_move_hint(6096).contains("split"));
    assert!(!bounded_move_hint(6096).contains("denied"));
}

#[test]
fn cargo_evidence_excludes_echoes_compounds_and_other_commands() {
    let args = serde_json::json!({"cwd":"task"});
    for command in [
        "echo cargo check",
        "cargo check || true",
        "cargo check; echo ok",
        "cargo test",
        "cargo check --manifest-path elsewhere/Cargo.toml",
        "cargo check && false",
    ] {
        assert!(
            super::super::self_verify::cargo_check_directory(command, &args, "/repo").is_none(),
            "{command}"
        );
    }
    assert_eq!(
        super::super::self_verify::cargo_check_directory(
            "cargo check -p core --lib",
            &args,
            "/repo"
        ),
        Some(std::path::PathBuf::from("/repo/task"))
    );
}

#[test]
fn inherited_head_and_checkout_reflogs_are_not_task_commits() {
    let fields = format!(
        "{} {} Fixture <fixture@example.invalid> 1 +0000",
        "0".repeat(40),
        "a".repeat(40)
    );
    for message in [
        "reset: moving to HEAD",
        "checkout: moving from main to task",
        "branch: Created from HEAD",
        "echo commit: fabricated",
    ] {
        assert!(!commit_in_reflog(&format!("{fields}\t{message}\n")));
    }
    assert!(commit_in_reflog(&format!("{fields}\tcommit: extraction\n")));
    assert!(!commit_in_reflog("malformed\tcommit: extraction"));
    assert!(refactor_publication("Refactor in a WORKTREE and open a PR"));
    assert!(refactor_publication("refactor worktree pull request"));
    assert!(!refactor_publication("refactor worktree print results"));
    assert!(!refactor_publication("refactor in place and open PR"));
}

/// #2831: actual writes (including copy_from expansion) keep their bytes and succeed.
#[tokio::test]
async fn publish_early_large_write_advice_never_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let content = "line\n".repeat(1501);
    std::fs::write(temp.path().join("source"), &content).unwrap();
    let caveats = crate::Caveats {
        fs_write: crate::Scope::only([temp.path().to_string_lossy().into_owned()]),
        ..crate::Caveats::top()
    };
    for (name, args) in [
        (
            "plain",
            serde_json::json!({"path":"plain", "content":content}),
        ),
        (
            "copied",
            serde_json::json!({"path":"copied", "content":"", "copy_from":{"path":"source","start_line":1,"end_line":1501}}),
        ),
    ] {
        let out = crate::execute_tool(
            "write_file",
            &args,
            temp.path().to_str().unwrap(),
            false,
            3,
            &caveats,
            &mut crate::NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(out.contains("Refactor hint:"), "{out}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join(name))
                .unwrap()
                .lines()
                .count(),
            1501
        );
    }
}

/// #2831 round 2: real Git grounds content freshness in the shared observer.
#[tokio::test]
async fn publish_early_checked_content_controls() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    let empty = tempfile::NamedTempFile::new().unwrap();
    let _global = EnvVar::set("GIT_CONFIG_GLOBAL", empty.path().to_str().unwrap());
    let _system = EnvVar::set("GIT_CONFIG_NOSYSTEM", "1");
    let _count = EnvVar::set("GIT_CONFIG_COUNT", "0");
    let _parameters = EnvVar::set("GIT_CONFIG_PARAMETERS", "");
    for scenario in [
        "staged",
        "original-checkout",
        "shell-edit",
        "ambiguous",
        "opaque-check",
        "switch-return",
        "failed-check",
        "unknown-check",
        "pr-created",
        "unstaged",
        "untracked",
        "assume-unchanged",
        "filters",
        "different-commit",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let task = temp.path().join("task");
        std::fs::create_dir(&original).unwrap();
        let git = |root: &std::path::Path, args: &[&str]| {
            let out = crate::git_hardening::metadata_git(root, args, &crate::Scope::All)
                .unwrap()
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&original, &["init", "-q", "-b", "main"]);
        std::fs::write(original.join("source"), "base").unwrap();
        git(&original, &["add", "."]);
        git(
            &original,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "base"],
        );
        git(
            &original,
            &["worktree", "add", "-qb", "task", task.to_str().unwrap()],
        );
        std::fs::write(task.join("source"), "checked extraction").unwrap();
        git(&task, &["add", "."]);
        let session = crate::worktree_adoption::WorktreeSession::default();
        session.record_task_worktree(&task, "task");
        let observe = |command: &str, outcome| {
            after_tool(
                &session,
                "Refactor in a worktree and open a PR",
                "run_command",
                &serde_json::json!({"command":command,"cwd":task}),
                original.to_str().unwrap(),
                outcome,
                None,
                &crate::Scope::All,
                Some(&task),
            )
        };
        match scenario {
            "unstaged" => std::fs::write(task.join("source"), "unstaged extraction").unwrap(),
            "untracked" => std::fs::write(task.join("extra"), "untracked input").unwrap(),
            "assume-unchanged" => git(&task, &["update-index", "--assume-unchanged", "source"]),
            "filters" => git(&task, &["config", "filter.fixture.clean", "false"]),
            _ => {}
        }
        if scenario == "original-checkout" {
            assert!(after_tool(
                &session,
                "Refactor in a worktree and open a PR",
                "run_command",
                &serde_json::json!({"command":"cargo check","cwd":original}),
                original.to_str().unwrap(),
                Some(ExecOutcome::Passed),
                None,
                &crate::Scope::All,
                Some(&original)
            )
            .is_none());
        } else {
            assert!(observe("cargo check", Some(ExecOutcome::Passed)).is_none());
        }
        match scenario {
            "shell-edit" => {
                std::fs::write(task.join("source"), "unchecked broken code").unwrap();
                assert!(observe("printf broken > source", Some(ExecOutcome::Passed)).is_none());
                git(&task, &["add", "."]);
            }
            "failed-check" => {
                assert!(observe("cargo check", Some(ExecOutcome::Failed)).is_none());
            }
            "unknown-check" => {
                assert!(observe("cargo check", None).is_none());
            }
            "opaque-check" => {
                assert!(observe("./check-wrapper", Some(ExecOutcome::Passed)).is_none());
            }
            "pr-created" => {
                let receipt = crate::git_staging::Outcome::PrCreated {
                    url: "https://github.com/example/project/pull/1".into(),
                };
                assert!(after_tool(
                    &session,
                    "continue",
                    "run_command",
                    &serde_json::json!({"command":"gh pr create","cwd":task}),
                    original.to_str().unwrap(),
                    Some(ExecOutcome::Passed),
                    Some(&receipt),
                    &crate::Scope::All,
                    Some(&task)
                )
                .is_none());
            }
            "different-commit" => {
                std::fs::write(task.join("other"), "different tree").unwrap();
                git(&task, &["add", "other"]);
                git(
                    &task,
                    &[
                        "-c",
                        "commit.gpgsign=false",
                        "commit",
                        "--only",
                        "-qm",
                        "other",
                        "other",
                    ],
                );
                assert!(!checked_tree::index_is_head(&task, &crate::Scope::All));
                assert!(observe("git commit --only other", Some(ExecOutcome::Passed)).is_none());
            }
            "ambiguous" => {
                assert!(observe("cargo check || true", Some(ExecOutcome::Passed)).is_none());
            }
            "switch-return" => {
                git(&task, &["switch", "-qc", "other"]);
                git(&task, &["switch", "-q", "task"]);
                assert!(observe(
                    "git switch -c other && git switch task",
                    Some(ExecOutcome::Passed)
                )
                .is_none());
            }
            _ => {}
        }
        git(&task, &["add", "."]);
        git(
            &task,
            &["-c", "commit.gpgsign=false", "commit", "-qm", "extraction"],
        );
        assert_eq!(
            after_tool(
                &session,
                "continue",
                "run_command",
                &serde_json::json!({"command":"git commit -qm extraction","cwd":task}),
                original.to_str().unwrap(),
                Some(ExecOutcome::Passed),
                None,
                &crate::Scope::All,
                Some(&task)
            ),
            (scenario == "staged").then_some(HINT),
            "{scenario}"
        );
        assert!(observe("git status", Some(ExecOutcome::Passed)).is_none());
        assert!(checked_tree::index_tree(&task, &crate::Scope::none()).is_none());
    }
}
