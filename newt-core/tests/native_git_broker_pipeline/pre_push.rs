//! Grounds the mocked pre-push contract and refusal tests with the real broker,
//! Git hook, and kernel fence. Run through `--confined-contract --pre-push`.

use super::{
    dispatch, dispatch_with_operator, fixture, real_git, real_git_output, Operator, Scope,
    WorktreeSession,
};
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
    // #3294 grounds the mocked Build admission and pre-push staging contract:
    // a real build tool needs its toolchain read, target and private scratch.
    let tools = tempfile::tempdir().unwrap();
    let bin = tools.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let tool = bin.join("just");
    std::fs::write(
        &tool,
        "#!/bin/sh\nexec /usr/bin/make --no-print-directory\n",
    )
    .unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    newt_core::process_env::set_var("XDG_BIN_HOME", bin.to_str().unwrap());
    std::fs::write(
        main.join("Makefile"),
        concat!(
            "check:\n",
            "\ttest -n \"$$TMPDIR\" && test -d \"$$TMPDIR\"\n",
            "\ttest \"$$CARGO_TARGET_DIR\" = \"$$PWD/target\"\n",
            "\tprintf scratch > \"$$TMPDIR/probe\"\n",
            "\tmkdir -p \"$$CARGO_TARGET_DIR\"\n",
            "\tprintf built > \"$$CARGO_TARGET_DIR/hook-built\"\n",
            "\tprintf '%s' \"$$TMPDIR\" > hook-scratch\n",
        ),
    )
    .unwrap();
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\nexec {}\n",
            newt_core::mcp::shell_quote_arg(&tool.to_string_lossy())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut build = newt_core::confined_exec::build_tool_request(
        &main,
        &main,
        "just",
        ["check"],
        &Scope::none(),
    )
    .caveats()
    .clone();
    build.net = caveats.net.clone();
    let out = dispatch(&session, &main, &build, "git push origin task").await;
    assert!(
        out.contains("trusted staging checks failed"),
        "build hook: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(main.join("target/hook-built")).unwrap(),
        "built"
    );
    let scratch = std::fs::read_to_string(main.join("hook-scratch")).unwrap();
    assert!(
        !std::path::Path::new(&scratch).exists(),
        "scratch lease leaked"
    );
    println!("GOVERNED_PRE_PUSH_BUILD_CONFIRMED");

    // Missing or declined Build authority cannot execute even the hook's first tool.
    std::fs::remove_file(main.join("target/hook-built")).unwrap();
    let out = dispatch(&session, &main, &caveats, "git push").await;
    assert!(
        out.contains("requires explicit confined build authority"),
        "{out}"
    );
    assert!(!main.join("target/hook-built").exists());
    for allow in [false, true] {
        let mut operator = Operator::new(allow);
        let out =
            dispatch_with_operator(&session, &main, &caveats, "git push", &mut operator).await;
        assert_eq!(operator.asked, [main.to_string_lossy().into_owned()]);
        assert_eq!(main.join("target/hook-built").exists(), allow, "{out}");
        assert!(
            out.contains(if allow {
                "trusted staging checks failed"
            } else {
                "requires explicit confined build authority"
            }),
            "{out}"
        );
    }
    if let Scope::Only(writes) = &mut caveats.fs_write {
        writes.insert(main.to_string_lossy().into_owned());
        writes.insert(
            newt_core::confined_exec::build_scratch_root(&main)
                .to_string_lossy()
                .into_owned(),
        );
    }
    println!("GOVERNED_PRE_PUSH_BUILD_AUTHORITY_CONFIRMED");

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

    // Even an ambient network grant for publication stays unavailable to hook code.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    std::fs::write(main.join("network-probe.py"), format!(
        "import socket\ntry:\n    socket.create_connection(('127.0.0.1', {}), timeout=1)\nexcept PermissionError:\n    pass\nelse:\n    raise AssertionError('hook acquired network')\n", listener.local_addr().unwrap().port()
    )).unwrap();
    std::fs::write(&hook, "#!/bin/sh\nexec python3 network-probe.py\n").unwrap();
    let mut network = caveats.clone();
    network.net = Scope::All;
    let out = dispatch(&session, &main, &network, "git push").await;
    assert!(
        out.contains("trusted staging checks failed"),
        "network fence: {out}"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    println!("GOVERNED_PRE_PUSH_NETWORK_DENIED_CONFIRMED");

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
    // Session bootstrap binds the external metadata reads used by every build.
    newt_core::git_hardening::own_gitdir_grants(&linked);
    if let Scope::Only(writes) = &mut caveats.fs_write {
        writes.insert(linked.to_string_lossy().into_owned());
        writes.insert(
            newt_core::confined_exec::build_scratch_root(&linked)
                .to_string_lossy()
                .into_owned(),
        );
    }
    let sub = linked.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let out = dispatch(&session, &sub, &caveats, "git push").await;
    assert!(out.contains("LINKED_HOOK_REJECTED"), "{out}");
    assert_eq!(
        std::fs::read_to_string(linked.join("linked-hook")).unwrap(),
        "linked"
    );

    // The common Git directory is readable to the hook, but cannot be mutated.
    let config = main.join(".git/config");
    let original = std::fs::read(&config).unwrap();
    std::fs::write(
        &common_hook,
        format!(
            "#!/bin/sh\nprintf poisoned > {}\n",
            newt_core::mcp::shell_quote_arg(&config.to_string_lossy())
        ),
    )
    .unwrap();
    let out = dispatch(&session, &sub, &caveats, "git push").await;
    assert!(out.contains("pre-push hook refused"), "{out}");
    assert_eq!(std::fs::read(&config).unwrap(), original);
    println!("GOVERNED_PRE_PUSH_GIT_PROTECTION_CONFIRMED");

    // An explicit per-call hang guard still owns the broker and hook descendants.
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
