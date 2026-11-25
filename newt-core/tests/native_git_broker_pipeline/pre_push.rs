//! Grounds the mocked pre-push contract and refusal tests with the real broker,
//! Git hook, and kernel fence. Run through `--confined-contract --pre-push`.

use super::{dispatch, fixture, real_git, real_git_output, Scope, WorktreeSession};
use std::os::unix::fs::PermissionsExt;

pub(super) async fn run() {
    let (_temp, main, _, mut caveats) = fixture();
    real_git(&main, &["checkout", "-qb", "task"]);
    real_git(
        &main,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    caveats.net = Scope::only(["github.com".to_owned()]);
    let tracking = real_git_output(&main, &["rev-parse", "HEAD"]);
    real_git(
        &main,
        &["commit", "-q", "--allow-empty", "-m", "local change"],
    );
    let oid = real_git_output(&main, &["rev-parse", "HEAD"]);
    let hooks = main.join(".githooks");
    std::fs::create_dir(&hooks).unwrap();
    real_git(&main, &["config", "core.hooksPath", ".githooks"]);
    let hook = hooks.join("pre-push");
    let session = WorktreeSession::default();

    // A successful hook may reach staging, but cannot reach the network: the
    // configured staging parent is a regular file in this isolated fixture.
    let state = main.join("blocked-state");
    std::fs::write(&state, "not a directory").unwrap();
    newt_core::process_env::set_var("NEWT_CONFIG_DIR", state.to_str().unwrap());
    for tracked in [false, true] {
        if tracked {
            real_git(
                &main,
                &["update-ref", "refs/remotes/origin/task", &tracking],
            );
        }
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf '%s\\n' \"$1\" \"$2\" > hook-args\ncat > hook-stdin\nprintf 'PRE_PUSH_REJECTED\\n' >&2\nexit 1\n",
        ).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = dispatch(&session, &main, &caveats, "git push origin task").await;
        assert!(out.contains("PRE_PUSH_REJECTED"), "{out}");
        assert!(out.contains("pre-push"), "{out}");
        assert_eq!(
            std::fs::read_to_string(main.join("hook-args")).unwrap(),
            "origin\nhttps://github.com/o/r.git\n"
        );
        let remote = if tracked {
            tracking.clone()
        } else {
            "0".repeat(oid.len())
        };
        assert_eq!(
            std::fs::read_to_string(main.join("hook-stdin")).unwrap(),
            format!("refs/heads/task {oid} refs/heads/task {remote}\n")
        );
    }

    std::fs::write(&hook, "#!/bin/sh\nprintf accepted > hook-accepted\n").unwrap();
    let out = dispatch(&session, &main, &caveats, "git push --dry-run").await;
    assert_eq!(
        std::fs::read_to_string(main.join("hook-accepted")).unwrap(),
        "accepted"
    );
    assert!(!out.contains("pre-push hook refused"), "{out}");
    assert!(out.contains("trusted staging checks failed"), "{out}");

    // Native Git skips absent and non-executable hooks. Both paths still
    // require the confined broker and then reach the blocked staging parent.
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(main.join("hook-accepted")).unwrap();
    let out = dispatch(&session, &main, &caveats, "git push").await;
    assert!(out.contains("trusted staging checks failed"), "{out}");
    assert!(!main.join("hook-accepted").exists());
    std::fs::remove_file(&hook).unwrap();
    let out = dispatch(&session, &main, &caveats, "git push").await;
    assert!(out.contains("trusted staging checks failed"), "{out}");

    // No host fallback when the hook attempts a write outside its grant.
    let forbidden = tempfile::tempdir().unwrap();
    let marker = forbidden.path().join("escaped");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf escaped > {}\n",
            newt_core::mcp::shell_quote_arg(&marker.to_string_lossy())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = dispatch(&session, &main, &caveats, "git push").await;
    assert!(out.contains("pre-push"), "{out}");
    assert!(!marker.exists(), "hook escaped its filesystem fence");

    // A linked worktree's default hooks live in the common repository, while
    // the hook runs in that worktree's root even when invoked in a subdirectory.
    real_git(&main, &["config", "--unset", "core.hooksPath"]);
    let common_hooks = main.join(".git/hooks");
    std::fs::create_dir_all(&common_hooks).unwrap();
    let common_hook = common_hooks.join("pre-push");
    std::fs::write(
        &common_hook,
        "#!/bin/sh\nprintf linked > linked-hook\nprintf 'LINKED_HOOK_REJECTED\\n' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&common_hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let linked = main.parent().unwrap().join("linked");
    real_git(
        &main,
        &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
    );
    let sub = linked.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let out = dispatch(&session, &sub, &caveats, "git push").await;
    assert!(out.contains("LINKED_HOOK_REJECTED"), "{out}");
    assert_eq!(
        std::fs::read_to_string(linked.join("linked-hook")).unwrap(),
        "linked"
    );

    // The ordinary per-call timeout owns the broker and all hook descendants.
    std::fs::write(&common_hook, "#!/bin/sh\nexec sleep 30\n").unwrap();
    let out = newt_core::execute_tool(
        "run_command",
        &serde_json::json!({"command": "git push", "timeout_secs": 1}),
        &main.to_string_lossy(),
        false,
        200,
        &caveats,
        &mut newt_core::NoMcp,
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
    assert!(out.contains("pre-push hook refused"), "{out}");
    assert!(out.contains("deadline expired"), "{out}");
    println!("GOVERNED_PRE_PUSH_CONTRACT_CONFIRMED");
}
