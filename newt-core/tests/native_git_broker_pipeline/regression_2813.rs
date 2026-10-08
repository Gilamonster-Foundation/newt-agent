//! #2813: the transcript command shapes through the real worker and hook image.
//! Real resources ground the unit fixture (which substitutes safe-subset for Brush).
use super::native::{init_worktree, real_git, real_git_output, TrailerGitTool};
use newt_core::{Caveats, NoMcp, Scope};
use std::path::Path;

pub fn run() {
    let journal = tempfile::tempdir().unwrap();
    newt_core::process_env::set_var(
        "NEWT_EVENT_JOURNAL",
        journal.path().join("events.jsonl").to_str().unwrap(),
    );
    newt_core::process_env::set_var("NEWT_SHELL_ENGINE", "brush");
    assert!(newt_core::confined_exec::kernel_fs_fence_available());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        nested_creation_without_session_is_refused().await;
        for packed in [false, true] {
            commit_cleanup_preserves_shared_refs(true, packed).await;
            commit_cleanup_preserves_shared_refs(false, packed).await;
        }
    });
    println!("REGRESSION_2813_CONFIRMED");
}

async fn dispatch(cwd: &Path, command: &str, caveats: &Caveats, signing: bool) -> String {
    newt_core::execute_tool(
        "run_command",
        &serde_json::json!({"command":command}),
        cwd.to_str().unwrap(),
        false,
        200,
        caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&TrailerGitTool(signing)),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

/// #2813 reg2-A: missing session collaborators cannot bypass creation admission.
async fn nested_creation_without_session_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let (main, _) = init_worktree(&temp.path().canonicalize().unwrap());
    let caveats = Caveats {
        fs_write: Scope::only([main.to_string_lossy().into_owned()]),
        ..Caveats::top()
    };
    let command = format!("cd '{}' && git worktree add -b agentic-refactor-wt agentic-refactor-wt HEAD 2>&1 | tail -5", main.display());
    let out = dispatch(&main, &command, &caveats, false).await;
    assert!(
        out.contains("capability denied") && out.contains("standalone literal command"),
        "creation bypassed admission: {out}"
    );
    assert!(!main.join("agentic-refactor-wt").exists());
    assert!(!main.join(".git/refs/heads/agentic-refactor-wt").exists());
}

/// #2813 reg3-A: native AUTO_MERGE cleanup must not try to lock shared refs
/// after a successful detached commit. Grounds the broker reference-event policy.
async fn commit_cleanup_preserves_shared_refs(heredoc: bool, packed: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (main, wt) = init_worktree(&root);
    if packed {
        real_git(&main, &["pack-refs", "--all"]);
    }
    let packed_before = std::fs::read(main.join(".git/packed-refs")).ok();
    let original_head = real_git_output(&main, &["rev-parse", "HEAD"]);
    std::fs::write(wt.join("seed"), "changed by regression").unwrap();
    if heredoc {
        real_git(&wt, &["add", "seed"]);
    }
    let grant = newt_core::git_hardening::own_gitdir_grants(&wt);
    let caveats = Caveats {
        fs_read: Scope::All,
        fs_write: Scope::only([wt.to_string_lossy().into_owned()]),
        net: Scope::none(),
        ..Caveats::top()
    };
    for relative in ["refs", "logs", "packed-refs", "packed-refs.lock"] {
        assert!(!grant
            .write
            .iter()
            .any(|p| main.join(".git").join(relative).starts_with(p)));
    }
    let command = if heredoc {
        "git commit -q -F - <<'EOF'\nfixture heredoc\nEOF\ngit log --oneline -1"
    } else {
        "git add seed && git commit -q -m 'fixture compound' && git log --oneline -2"
    };
    let command = format!("cd '{}' && {command}", wt.display());
    let out = dispatch(&wt, &command, &caveats, false).await;
    let message = real_git_output(&wt, &["log", "-1", "--format=%B"]);
    assert!(
        message.contains("native-git-broker-pipeline-fixture"),
        "broker did not publish: {out}"
    );
    assert!(
        !out.contains("packed-refs.lock")
            && !out.contains("Operation not permitted")
            && !out.contains("Permission denied"),
        "commit cleanup tried to lock shared refs: {out}"
    );
    assert_eq!(
        std::fs::read(main.join(".git/packed-refs")).ok(),
        packed_before
    );
    assert_eq!(
        real_git_output(&main, &["rev-parse", "HEAD"]),
        original_head
    );
    assert_eq!(
        real_git_output(&wt, &["show", "HEAD:seed"]),
        "changed by regression"
    );
    assert_eq!(
        real_git_output(&wt, &["symbolic-ref", "HEAD"]),
        "refs/heads/task"
    );
    let raw = real_git_output(&wt, &["cat-file", "commit", "HEAD"]);
    assert!(!raw.contains("gpgsig "));
    // The fixture signer deliberately refuses: private views must not turn a
    // required signature failure into an unsigned publication (#2813/#2720).
    if packed {
        let before = real_git_output(&wt, &["rev-parse", "HEAD"]);
        let refused = dispatch(
            &wt,
            "git commit -q --allow-empty -m signing-refusal; git log --oneline -1",
            &caveats,
            true,
        )
        .await;
        assert!(
            refused.contains("signing not exercised by this fixture"),
            "{refused}"
        );
        assert_eq!(real_git_output(&wt, &["rev-parse", "HEAD"]), before);
    }
    if packed && !heredoc {
        let before = real_git_output(&wt, &["rev-parse", "HEAD"]);
        let tree = real_git_output(&wt, &["rev-parse", "HEAD^{tree}"]);
        real_git(&wt, &["update-ref", "AUTO_MERGE", &tree]);
        let refused = dispatch(
            &wt,
            "git commit -q --allow-empty -m unfinished",
            &caveats,
            false,
        )
        .await;
        assert!(refused.contains("standalone git commit"), "{refused}");
        assert_eq!(real_git_output(&wt, &["rev-parse", "HEAD"]), before);
    }
    let admin = main.join(".git/worktrees/wt");
    assert!(!std::fs::read_dir(admin).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".newt-commit-view-")));
}
