fn real_git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

/// F32/#2537, PR #2577 round 3 item 2(a) — the proof round 1's tests lacked:
/// a REAL confined-shell `git add` in a linked worktree, on a non-default
/// branch, succeeds under the actual KERNEL fence (Landlock), not a plain
/// unconfined subprocess. `session.fs_write` is scoped to the worktree ONLY —
/// `dispatch_caveats_for_git_shell` is what widens it (per-dispatch) with
/// write on the worktree's own gitdir + the common `objects/` directory,
/// which is what lets `index.lock` create+rename and object insertion
/// succeed where a bare file-level rule could not.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn confined_shell_git_add_succeeds_in_a_linked_worktree_on_a_non_default_branch() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if !crate::confined_exec::kernel_fs_fence_available()
        || !std::path::Path::new("/usr/bin/git").exists()
    {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    real_git(&main, &["init", "-q"]);
    std::fs::write(main.join("seed"), "x").unwrap();
    real_git(&main, &["add", "seed"]);
    real_git(&main, &["commit", "-q", "-m", "init"]);
    let wt = root.path().join("wt");
    real_git(
        &main,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
    );
    std::fs::write(wt.join("f.txt"), "hi\n").unwrap();

    // The same read wiring `apply_cli_fs_grants` produces in production: the
    // worktree plus the own-gitdir READ grant (config/HEAD/index/objects
    // live outside `wt` for a linked worktree).
    let own_git = crate::git_hardening::own_gitdir_grants(&wt);
    let mut read_roots = vec![wt.to_string_lossy().into_owned()];
    read_roots.extend(own_git.read);
    let session = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only(read_roots),
        fs_write: crate::caveats::Scope::only([wt.to_string_lossy().into_owned()]),
        exec: crate::caveats::Scope::only(["git".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let widened = super::shell::dispatch_caveats_for_git_shell(
        "git add f.txt",
        &wt.to_string_lossy(),
        &session,
    );

    let envelope = super::shell::dispatch_bridled_shell(
        serde_json::json!({"cmd": "git add f.txt", "cwd": wt.to_string_lossy()}),
        &widened,
        None,
    )
    .await
    .expect("dispatch");
    assert_eq!(
        envelope["sandbox_kind"], "landlock",
        "must be kernel-confined: {envelope}"
    );
    assert_eq!(
        envelope["exit_code"], 0,
        "git add must succeed under the widened fence: {envelope}"
    );

    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&wt)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("A  f.txt"),
        "f.txt must be staged: {}",
        String::from_utf8_lossy(&status.stdout)
    );
}

/// Item 2(b): the widening is scoped to the ONE dispatch — the SESSION
/// `fs_write` scope itself still does not permit the common dir's
/// `objects/`, so a file tool (`write_file`/`delete_file`) stays refused.
#[test]
fn session_fs_write_still_excludes_the_common_objects_directory() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    real_git(&main, &["init", "-q"]);
    std::fs::write(main.join("seed"), "x").unwrap();
    real_git(&main, &["add", "seed"]);
    real_git(&main, &["commit", "-q", "-m", "init"]);
    let wt = root.path().join("wt");
    real_git(
        &main,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
    );

    let session = crate::caveats::Caveats {
        fs_write: crate::caveats::Scope::only([wt.to_string_lossy().into_owned()]),
        ..crate::caveats::Caveats::top()
    };
    // Unwidened session caveats (what a file tool dispatch actually sees —
    // `dispatch_caveats_for_git_shell` is consulted only on the shell path).
    let objects = main.join(".git/objects/pack/x.pack");
    assert!(
        !crate::caveats::permits_path(&session.fs_write, &objects.to_string_lossy()),
        "the common objects/ dir must stay outside the SESSION fs_write scope"
    );
    // But the per-dispatch widening for a `git` shell command DOES cover it —
    // this is the mechanism, proven directly (no kernel involved here).
    let widened = super::shell::dispatch_caveats_for_git_shell(
        "git add f.txt",
        &wt.to_string_lossy(),
        &session,
    );
    assert!(
        crate::caveats::permits_path(&widened.fs_write, &objects.to_string_lossy()),
        "the per-dispatch widening must cover objects/ for a git shell command"
    );
    // A non-`git` command gets no widening at all.
    let unwidened =
        super::shell::dispatch_caveats_for_git_shell("rm f.txt", &wt.to_string_lossy(), &session);
    assert_eq!(unwidened.fs_write, session.fs_write);
}

/// Item 2(c): on the default branch, no shell write widening — matches
/// `own_gitdir_grants`'s own default-branch carve-out.
#[test]
fn default_branch_gets_no_shell_write_widening() {
    let root = tempfile::tempdir().unwrap();
    real_git(root.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(root.path().join("seed"), "x").unwrap();
    real_git(root.path(), &["add", "seed"]);
    real_git(root.path(), &["commit", "-q", "-m", "init"]);

    let session = crate::caveats::Caveats {
        fs_write: crate::caveats::Scope::only([root.path().to_string_lossy().into_owned()]),
        ..crate::caveats::Caveats::top()
    };
    let widened = super::shell::dispatch_caveats_for_git_shell(
        "git add f.txt",
        &root.path().to_string_lossy(),
        &session,
    );
    assert_eq!(
        widened.fs_write, session.fs_write,
        "the default branch must get no shell-lane write widening"
    );
}
