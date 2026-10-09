//! Explicit Git worktree administration is caught by the narrow accident guard.
//! Runs with production Brush on both Linux and macOS (not SafeSubset).
use super::{adopt, dispatch, fixture, real_git, real_git_output, WorktreeSession};

pub async fn run() {
    aliases().await;
    let (_temp, main, task, caveats) = fixture();
    let session = WorktreeSession::default();
    // A session without adoption retains ordinary worktree administration.
    real_git(&main, &["worktree", "add", "-b", "task", "../task"]);
    let command = format!(
        "git -C '{}' worktree remove '{}' --force",
        main.display(),
        task.display()
    );
    let out = dispatch(&session, &main, &caveats, &command).await;
    assert!(!task.exists(), "unadopted removal: {out}");
    real_git(&main, &["branch", "-D", "task"]);
    println!("PASS unadopted_worktree_removal_unchanged");

    adopt(&session, &main, &task, &caveats, false).await;
    std::fs::write(task.join("uncommitted"), "preserve this work").unwrap();
    let admin = main.join(".git/worktrees/task");
    let head = real_git_output(&task, &["rev-parse", "HEAD"]);
    for command in [
        command,
        format!("git worktree remove --force '{}'", task.display()),
        format!("git worktree remove -ff '{}'", admin.display()),
        "git worktree prune --expire now".into(),
        "git worktree move ../task ../moved".into(),
        "echo before; git worktree prune".into(),
    ] {
        let out = dispatch(&session, &main, &caveats, &command).await;
        assert_eq!(
            std::fs::read_to_string(task.join("uncommitted"))
                .ok()
                .as_deref(),
            Some("preserve this work"),
            "uncommitted data lost: {command}: {out}"
        );
        assert!(
            out.contains("adopted worktree administration"),
            "{command}: {out}"
        );
        assert!(task.join(".git").is_file());
        assert!(admin.join("HEAD").is_file());
        assert_eq!(real_git_output(&task, &["rev-parse", "HEAD"]), head);
    }
    let out = dispatch(&session, &main, &caveats, "git worktree list").await;
    assert!(out.contains(task.to_str().unwrap()), "list: {out}");
    let out = dispatch(&session, &main, &caveats, "echo still-working > continued").await;
    assert_eq!(
        std::fs::read_to_string(task.join("continued")).unwrap(),
        "still-working\n",
        "{out}"
    );
    println!("PASS adopted_worktree_removal_refused_session_usable");
}

/// #2836: aliases carry the same destructive operation without a literal verb.
async fn aliases() {
    for source in ["command-line", "repository", "worktree"] {
        let (_temp, main, task, caveats) = fixture();
        let session = WorktreeSession::default();
        adopt(&session, &main, &task, &caveats, false).await;
        std::fs::write(task.join("uncommitted"), "alias sentinel").unwrap();
        match source {
            "repository" => real_git(&main, &["config", "alias.wipe", "worktree remove --force"]),
            "worktree" => {
                real_git(&main, &["config", "extensions.worktreeConfig", "true"]);
                real_git(
                    &task,
                    &[
                        "config",
                        "--worktree",
                        "alias.wipe",
                        "worktree remove --force",
                    ],
                );
            }
            _ => {}
        }
        let command = if source == "command-line" {
            format!(
                "git -c 'alias.wipe=worktree remove --force' wipe '{}'",
                task.display()
            )
        } else {
            format!("git wipe '{}'", task.display())
        };
        let out = dispatch(&session, &main, &caveats, &command).await;
        assert_eq!(
            std::fs::read_to_string(task.join("uncommitted"))
                .ok()
                .as_deref(),
            Some("alias sentinel"),
            "{source}: {out}"
        );
        assert!(
            out.contains("adopted worktree administration"),
            "{source}: {out}"
        );
        assert!(task.join(".git").is_file());
        assert!(main.join(".git/worktrees/task/HEAD").is_file());
        // Git built-ins take precedence over aliases; preserve ordinary reads.
        let out = dispatch(
            &session,
            &main,
            &caveats,
            "git -c 'alias.status=worktree prune' status --short",
        )
        .await;
        assert!(out.contains("uncommitted"), "built-in status: {out}");
        println!("PASS adopted_alias_refused source={source}");
    }
}
