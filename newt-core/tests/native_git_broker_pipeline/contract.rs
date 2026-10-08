//! Production Brush counterpart of the libtest command-shape/adoption rows.
//! Grounds session binding, kernel ref protection, and governed publication.
//! `--require-2818` runs the positive commit/ref publication row pending PR #2818.
#[path = "contract_chat.rs"]
mod chat;

use super::native::{real_git, real_git_output, TrailerGitTool};
use newt_core::{worktree_adoption::WorktreeSession, Caveats, Scope};
use std::path::{Path, PathBuf};

pub fn run() {
    let home = tempfile::tempdir().unwrap();
    // Set environment before starting any worker/runtime; never use operator config.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_NAMESPACE",
        "NEWT_VENV",
        "VIRTUAL_ENV",
    ] {
        newt_core::process_env::remove_var(key);
    }
    for (key, value) in [
        ("NEWT_SHELL_ENGINE", "brush"),
        ("NEWT_DISABLE_OCAP", "0"),
        ("NEWT_FULL_ACCESS", "0"),
        ("NEWT_NO_ROUTE", "0"),
        ("NEWT_EXEC_PATHS", "/usr/bin:/bin"),
        ("PATH", "/usr/bin:/bin"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TEMPLATE_DIR", "/dev/null"),
        ("GIT_AUTHOR_NAME", "fixture"),
        ("GIT_AUTHOR_EMAIL", "fixture@example.invalid"),
        ("GIT_COMMITTER_NAME", "fixture"),
        ("GIT_COMMITTER_EMAIL", "fixture@example.invalid"),
    ] {
        newt_core::process_env::set_var(key, value);
    }
    newt_core::process_env::set_var("HOME", home.path().to_str().unwrap());
    newt_core::process_env::set_var(
        "NEWT_EVENT_JOURNAL",
        home.path().join("events.jsonl").to_str().unwrap(),
    );
    let bin = home.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", bin.join("gh")).unwrap();
    let paths = format!("{}:/usr/bin:/bin", bin.display());
    newt_core::process_env::set_var("NEWT_EXEC_PATHS", &paths);
    newt_core::process_env::set_var("PATH", &paths);
    assert!(
        newt_core::confined_exec::kernel_fs_fence_available(),
        "native contract requires a kernel fence"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            if std::env::args().any(|arg| arg == "--require-2818") {
                governed_publication().await;
            } else {
                for wrapper in [false, true] {
                    creation_and_security(wrapper).await;
                }
                println!("NATIVE_CONFINED_CONTRACT_CONFIRMED");
            }
        });
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Caveats) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let main = root.join("main");
    let task = root.join("task");
    std::fs::create_dir(&main).unwrap();
    real_git(&main, &["init", "-q", "-b", "main"]);
    real_git(&main, &["commit", "-q", "--allow-empty", "-m", "seed"]);
    std::fs::write(main.join("sentinel"), "keep me").unwrap();
    let caveats = Caveats {
        fs_write: Scope::only([root.to_string_lossy().into_owned()]),
        net: Scope::none(),
        ..Caveats::top()
    };
    (temp, main, task, caveats)
}

async fn dispatch(
    session: &WorktreeSession,
    main: &Path,
    caveats: &Caveats,
    command: &str,
) -> String {
    chat::dispatch(
        session,
        main,
        caveats,
        command,
        Some(&TrailerGitTool(false)),
    )
    .await
}

async fn adopt(
    session: &WorktreeSession,
    main: &Path,
    task: &Path,
    caveats: &Caveats,
    wrapper: bool,
) {
    let command = if wrapper {
        "git worktree add -b task ../task 2>&1 | tail -5 && git status --short"
    } else {
        "git worktree add -b task ../task"
    };
    let out = dispatch(session, main, caveats, command).await;
    assert!(
        out.contains("Adopted task worktree:"),
        "creation/adoption: {out}"
    );
    assert!(task.join(".git").is_file());
    assert_eq!(
        real_git_output(task, &["symbolic-ref", "HEAD"]),
        "refs/heads/task"
    );
    // Prove routing and an armed fence behaviorally, without exposing internals.
    let cwd = dispatch(session, main, caveats, "pwd").await;
    assert!(cwd.contains(task.to_str().unwrap()), "task routing: {cwd}");
    let command = format!("echo changed > '{}/sentinel'", main.display());
    let refused = dispatch(session, main, caveats, &command).await;
    assert!(
        refused.contains("read-only") || refused.contains("denied"),
        "original write: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(main.join("sentinel")).unwrap(),
        "keep me"
    );
    println!("PASS creation_adoption_original_fence wrapper={wrapper}");
}

async fn creation_and_security(wrapper: bool) {
    let (_temp, main, task, caveats) = fixture();
    let session = WorktreeSession::default();
    let unsafe_batch = dispatch(
        &session,
        &main,
        &caveats,
        "git worktree add -b task ../task; git clean -fd -- sentinel",
    )
    .await;
    assert!(unsafe_batch.contains("capability denied"), "{unsafe_batch}");
    assert!(!task.exists());
    assert_eq!(
        std::fs::read_to_string(main.join("sentinel")).unwrap(),
        "keep me"
    );
    println!("PASS unsafe_batch_preserves_sentinel");
    let missing = dispatch(&session, &main, &caveats, "git worktree add ../task absent").await;
    assert!(missing.contains("-b"), "missing branch guidance: {missing}");
    assert!(!task.join(".git").exists());
    if task.exists() {
        std::fs::remove_dir(&task).expect("failed creation leaves only an empty leaf");
    }
    println!("PASS missing_branch_guidance");
    let discovery = dispatch(&session, &main, &caveats, "which git").await;
    assert!(discovery.contains("/usr/bin/git"), "discovery: {discovery}");
    println!("PASS confined_git_discovery");
    let gh = dispatch(&session, &main, &caveats, "which gh").await;
    let expected = PathBuf::from(std::env::var_os("HOME").unwrap()).join("bin/gh");
    assert!(
        gh.contains(expected.to_str().unwrap()),
        "inert gh discovery: {gh}"
    );
    println!("PASS confined_fixture_gh_discovery");
    adopt(&session, &main, &task, &caveats, wrapper).await;
    raw_refs_are_protected(&main, &task).await;
    // No policy: refusal must not fund an unmanaged commit.
    let before = real_git_output(&task, &["rev-parse", "HEAD"]);
    let out = chat::dispatch(
        &session,
        &main,
        &caveats,
        "git commit --allow-empty -m unmanaged",
        None,
    )
    .await;
    assert!(
        out.contains("error:") || out.contains("refused"),
        "no broker: {out}"
    );
    assert_eq!(real_git_output(&task, &["rev-parse", "HEAD"]), before);
    println!("PASS missing_commit_broker_refused");
    let branch = dispatch(&session, &main, &caveats, "git switch -c followup").await;
    assert_eq!(
        real_git_output(&task, &["symbolic-ref", "HEAD"]),
        "refs/heads/followup",
        "bounded branch: {branch}"
    );
    assert_eq!(
        real_git_output(&main, &["rev-parse", "refs/heads/followup"]),
        before
    );
    assert_eq!(
        real_git_output(&main, &["rev-parse", "refs/heads/task"]),
        before
    );
    assert_eq!(real_git_output(&main, &["rev-parse", "HEAD"]), before);
    println!("PASS bounded_branch_broker_publication");
}

async fn raw_refs_are_protected(main: &Path, task: &Path) {
    use newt_core::confined_exec::{ConstrainedExecutor, ExecOrigin, ExecRequest};
    let remote = task.parent().unwrap().join("remote.git");
    real_git(main, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    real_git(main, &["remote", "add", "origin", remote.to_str().unwrap()]);
    let old = real_git_output(task, &["rev-parse", "HEAD"]);
    let mut roots = newt_core::git_hardening::own_gitdir_grants(task).write;
    roots.extend([
        task.to_string_lossy().into_owned(),
        remote.to_string_lossy().into_owned(),
    ]);
    let caveats = Caveats {
        fs_write: Scope::only(roots),
        net: Scope::none(),
        ..Caveats::top()
    };
    for (name, args) in [
        ("raw_branch", vec!["branch", "followup"]),
        ("raw_tracking", vec!["push", "origin", "task:task"]),
    ] {
        let out = ConstrainedExecutor::run_async(
            ExecRequest::new(
                ExecOrigin::AgentInfluenced,
                "/usr/bin/git",
                args,
                task,
                caveats.clone(),
            )
            .env("PATH", "/usr/bin:/bin"),
        )
        .await
        .expect("raw Git kernel admission");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.sandbox_kind,
            if cfg!(target_os = "macos") {
                agent_bridle::SandboxKind::Seatbelt
            } else {
                agent_bridle::SandboxKind::Landlock
            }
        );
        if name == "raw_branch" {
            assert!(!out.success, "raw branch write succeeded");
            assert!(
                stderr.contains("Permission denied") || stderr.contains("Operation not permitted"),
                "raw branch refusal: {stderr}"
            );
        } else {
            // Git transport can succeed even when its tracking update fails.
            assert!(out.success, "local transport: {stderr}");
            assert_eq!(
                real_git_output(&remote, &["rev-parse", "refs/heads/task"]),
                old
            );
            assert!(
                stderr.contains("update_ref failed"),
                "tracking write not refused: {stderr}"
            );
        }
        assert!(!main.join(".git/refs/heads/followup").exists());
        assert!(!main.join(".git/refs/remotes/origin/task").exists());
        assert_eq!(real_git_output(task, &["rev-parse", "HEAD"]), old);
        println!("PASS {name}_refused_shared_refs_unchanged");
    }
}

/// Same production worker/hook pattern as --regression-2813 in PR #2818.
/// Successful governed publication means the verified candidate advances ONLY
/// the task branch; local transport and broker refusal are separate controls.
async fn governed_publication() {
    let (_temp, main, task, mut caveats) = fixture();
    let session = WorktreeSession::default();
    adopt(&session, &main, &task, &caveats, false).await;
    caveats.fs_write = Scope::only([task.to_string_lossy().into_owned()]);
    let old = real_git_output(&main, &["rev-parse", "HEAD"]);
    std::fs::write(task.join("payload"), "governed\n").unwrap();
    let out = dispatch(
        &session,
        &main,
        &caveats,
        "git add payload && git commit -m contract",
    )
    .await;
    let new = real_git_output(&task, &["rev-parse", "HEAD"]);
    assert_ne!(
        old, new,
        "NEEDS PR #2818: governed commit/publication failed: {out}"
    );
    assert!(
        real_git_output(&task, &["log", "-1", "--format=%B"])
            .contains("native-git-broker-pipeline-fixture"),
        "policy trailer missing: {out}"
    );
    assert_eq!(
        real_git_output(&task, &["show", "HEAD:payload"]),
        "governed"
    );
    assert_eq!(
        real_git_output(&main, &["rev-parse", "refs/heads/task"]),
        new
    );
    assert_eq!(real_git_output(&main, &["rev-parse", "HEAD"]), old);
    assert_eq!(
        real_git_output(&task, &["symbolic-ref", "HEAD"]),
        "refs/heads/task"
    );
    // #2813: ref movement alone is insufficient: Git can publish the commit
    // and still fail its AUTO_MERGE cleanup against shared packed refs.
    assert!(
        !out.contains("packed-refs.lock")
            && !out.contains("Operation not permitted")
            && !out.contains("Permission denied")
            && !out.contains("error:"),
        "NEEDS PR #2818: governed commit cleanup failed after publication: {out}"
    );
    println!("PASS governed_commit_and_task_ref_publication");
}
