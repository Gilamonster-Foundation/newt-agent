//! Real native Git publication through Newt's ordinary command interface.
//!
//! Grounds the broker policy tests in Brush parsing, native Git's own argument
//! handling, actual signatures, and observed ref publication. No installed Newt,
//! inference service, operator Git configuration, or live repository is used.

fn main() {
    if let Some(code) = newt_core::maybe_dispatch() {
        std::process::exit(code);
    }
    if !std::env::args().any(|arg| arg == "--ignored") {
        eprintln!("native_git_commit ... ignored (real native Git and confinement)");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    // A dedicated harness=false process owns this isolated environment before
    // any worker threads start. Every worker re-exec dispatches above first.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("NEWT_SHELL_ENGINE", "brush");
        std::env::remove_var("NEWT_DISABLE_OCAP");
        std::env::remove_var("NEWT_FULL_ACCESS");
    }
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(composed_commit_is_native_attributed_and_signed(&root));
}

fn git(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env(
            "GIT_CONFIG_SYSTEM",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture Operator")
        .env("GIT_AUTHOR_EMAIL", "operator@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture Operator")
        .env("GIT_COMMITTER_EMAIL", "operator@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

async fn composed_commit_is_native_attributed_and_signed(root: &std::path::Path) {
    use newt_core::commit_signing::{generate_harness_key, HarnessSshSigner};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    let repo = root.join("repository");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "task"]);
    std::fs::write(repo.join("README.md"), "fixture\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    std::fs::write(repo.join(".gitignore"), "*.pyc\n").unwrap();

    let key = root.join("signing-key.pem");
    let public = generate_harness_key(&key).unwrap();
    let identity = newt_core::AgentIdentity {
        name: "Fixture Agent".into(),
        email: "agent@example.invalid".into(),
        ..Default::default()
    };
    let tool = newt_git::LocalGitTool {
        root: repo.clone(),
        author: newt_git::Author {
            name: "Fixture Agent".into(),
            email: "agent@example.invalid".into(),
        },
        attribution: Some(newt_core::attribution::CommitAttribution::from_identity(
            "fixture-model",
            &identity,
        )),
        commit_succeeded: Arc::new(AtomicUsize::new(0)),
        contributors_consumed: Arc::new(AtomicUsize::new(0)),
        signer: Some(Arc::new(HarnessSshSigner::load(&key).unwrap())),
    };
    let before = git(&repo, &["rev-parse", "HEAD"]).stdout;
    let authority = newt_core::Caveats {
        fs_read: newt_core::Scope::only([repo.to_string_lossy().into_owned()]),
        fs_write: newt_core::Scope::only([repo.to_string_lossy().into_owned()]),
        ..newt_core::Caveats::top()
    };
    let output = execute(
        &repo,
        &tool,
        &authority,
        "git add .gitignore && git commit -m 'Ignore Python bytecode'",
    )
    .await;
    assert_succeeded(&output);
    let after = git(&repo, &["rev-parse", "HEAD"]).stdout;
    assert_ne!(
        before, after,
        "ordinary authorized compound commit must publish: {output}"
    );
    let message = String::from_utf8(git(&repo, &["log", "-1", "--format=%B"]).stdout).unwrap();
    assert!(message.contains("Ignore Python bytecode"), "{message}");
    assert!(message.contains("fixture-model"), "{message}");
    assert!(message.contains("Co-authored-by:"), "{message}");
    assert_eq!(git(&repo, &["show", "HEAD:.gitignore"]).stdout, b"*.pyc\n");
    let status = git(&repo, &["status", "--porcelain"]).stdout;
    assert!(status.is_empty(), "{}", String::from_utf8_lossy(&status));
    assert_eq!(tool.drain_commit_success(), 1);
    assert_eq!(tool.drain_commit_success(), 0);
    let allowed = root.join("allowed-signers");
    std::fs::write(&allowed, format!("agent@example.invalid {public}\n")).unwrap();
    git(
        &repo,
        &[
            "-c",
            &format!("gpg.ssh.allowedSignersFile={}", allowed.display()),
            "verify-commit",
            "HEAD",
        ],
    );
    println!("test composed_commit_is_native_attributed_and_signed ... ok");

    // A direct command's broker does not cover a native dispatcher's child.
    // Preflight must reject the whole source before even its first commit.
    for source in [
        "git commit --allow-empty -m direct; find . -maxdepth 0 -exec git commit --allow-empty -m delegated \\;",
        "git commit --allow-empty -m direct; timeout 30 git commit --allow-empty -m delegated",
        "git commit --allow-empty -m direct; find . -maxdepth 0 -exec timeout 30 git commit --allow-empty -m delegated \\;",
        "git commit --allow-empty -m direct; timeout 30 find . -maxdepth 0 -exec timeout 20 git commit --allow-empty -m delegated \\;",
    ] {
        let before = git(&repo, &["rev-parse", "HEAD"]).stdout;
        let output = execute(&repo, &tool, &authority, source).await;
        assert_eq!(git(&repo, &["rev-parse", "HEAD"]).stdout, before, "{source}: {output}");
        assert!(
            output.contains("refused:") || output.contains("refusing Git commit publication"),
            "{source}: {output}"
        );
        assert_eq!(tool.drain_commit_success(), 0, "{source}: {output}");
    }
    println!("test native_descendant_cannot_borrow_direct_commit_broker ... ok");

    // Grounds static heredoc inspection in the exact ordinary shell shape
    // that previously missed the runtime broker. A successful tail stage is
    // insufficient: verify the actual tree, message, signature, and event.
    std::fs::create_dir(repo.join("newt-core")).unwrap();
    std::fs::write(repo.join("README.md"), "committed through a heredoc\n").unwrap();
    git(&repo, &["add", "README.md"]);
    let body = "Refactor fixture\n\nPreserve multiline message semantics.\nLiteral `backticks` and $(printf literal) remain message text.";
    let command = format!(
        "cd newt-core && git branch --show-current && git commit -m \"$(cat <<'EOF'\n{body}\nEOF\n)\" 2>&1 | tail -5"
    );
    let before = git(&repo, &["rev-parse", "HEAD"]).stdout;
    let output = execute(&repo, &tool, &authority, &command).await;
    assert_succeeded(&output);
    assert_ne!(
        git(&repo, &["rev-parse", "HEAD"]).stdout,
        before,
        "{output}"
    );
    let message = String::from_utf8(git(&repo, &["log", "-1", "--format=%B"]).stdout).unwrap();
    assert!(message.starts_with(&format!("{body}\n\n")), "{message}");
    assert!(message.contains("fixture-model"), "{message}");
    assert!(message.contains("Co-authored-by:"), "{message}");
    assert_eq!(
        git(&repo, &["show", "HEAD:README.md"]).stdout,
        b"committed through a heredoc\n"
    );
    assert!(git(&repo, &["status", "--porcelain"]).stdout.is_empty());
    assert_eq!(tool.drain_commit_success(), 1);
    assert_eq!(tool.drain_commit_success(), 0);
    git(
        &repo,
        &[
            "-c",
            &format!("gpg.ssh.allowedSignersFile={}", allowed.display()),
            "verify-commit",
            "HEAD",
        ],
    );
    println!("test heredoc_compound_commit_is_native_attributed_and_signed ... ok");

    let before = git(&repo, &["rev-parse", "HEAD"]).stdout;
    let output = execute(
        &repo,
        &tool,
        &authority,
        "git commit --allow-empty -m \"$(cat <(printf unsupported))\"",
    )
    .await;
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]).stdout,
        before,
        "{output}"
    );
    assert_eq!(tool.drain_commit_success(), 0);
    assert!(output.contains("process substitution"), "{output}");
    assert!(!output.contains("does not yet integrate"), "{output}");
    println!("test unsupported_commit_form_reports_actual_inspection_limit ... ok");

    let output = execute(&repo, &tool, &authority, "git commit --allow-empty -m --").await;
    assert_succeeded(&output);
    assert_eq!(
        String::from_utf8(git(&repo, &["log", "-1", "--format=%s"]).stdout)
            .unwrap()
            .trim_end(),
        "--",
        "{output}"
    );
    assert_eq!(tool.drain_commit_success(), 1);
    println!("test literal_double_dash_message_keeps_native_option_parsing ... ok");

    let before = git(&repo, &["rev-parse", "HEAD"]).stdout;
    let output = execute(
        &repo,
        &tool,
        &authority,
        "git commit --allow-empty --no-gpg-sign -m 'must refuse unsigned'",
    )
    .await;
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]).stdout,
        before,
        "{output}"
    );
    assert_eq!(tool.drain_commit_success(), 0);
    assert!(
        output.contains("signature") || output.contains("signing"),
        "{output}"
    );
    println!("test explicit_no_sign_cannot_publish_unsigned_commit ... ok");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hooks = repo.join(".git/operator-hooks");
        std::fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nset -eu\nif (printf leaked >&198) 2>/dev/null; then echo 'private broker fd leaked' >&2; exit 70; fi\nhooks=$(git config --get core.hooksPath)\nif rm \"$hooks/reference-transaction\" 2>/dev/null; then echo 'protected helper was writable' >&2; exit 71; fi\ntest -L \"$hooks/reference-transaction\"\nprintf 'protected; no delegated fd\\n' > .git/hook-audit\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let command = "git -c core.hooksPath=.git/operator-hooks commit --allow-empty --author='Requested Author <requested@example.invalid>' -m 'hook and author preserved'";
        let output = execute(&repo, &tool, &authority, command).await;
        assert_succeeded(&output);
        assert_eq!(
            std::fs::read_to_string(repo.join(".git/hook-audit")).unwrap_or_default(),
            "protected; no delegated fd\n",
            "{output}"
        );
        assert_eq!(
            String::from_utf8(git(&repo, &["log", "-1", "--format=%an <%ae>"]).stdout)
                .unwrap()
                .trim_end(),
            "Requested Author <requested@example.invalid>",
            "{output}"
        );
        assert_eq!(tool.drain_commit_success(), 1);
        git(
            &repo,
            &[
                "-c",
                &format!("gpg.ssh.allowedSignersFile={}", allowed.display()),
                "verify-commit",
                "HEAD",
            ],
        );
        println!("test repository_hook_cannot_replace_helpers_or_inherit_broker_fd ... ok");
    }

    #[cfg(target_os = "macos")]
    {
        let before = git(&repo, &["rev-parse", "HEAD"]).stdout;
        let output = execute(
            &repo,
            &tool,
            &authority,
            "/usr/bin/git commit --allow-empty -m 'system Git shim'",
        )
        .await;
        assert_succeeded(&output);
        assert_ne!(
            git(&repo, &["rev-parse", "HEAD"]).stdout,
            before,
            "{output}"
        );
        assert_eq!(tool.drain_commit_success(), 1);
        println!("test apple_system_git_exec_transition_keeps_native_command ... ok");
    }

    let before = git(&repo, &["rev-parse", "HEAD"]).stdout;
    let output = execute(
        &repo,
        &tool,
        &newt_core::Caveats::top(),
        "git commit --allow-empty -m 'unprotectable grant'",
    )
    .await;
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]).stdout,
        before,
        "{output}"
    );
    assert_eq!(tool.drain_commit_success(), 0);
    assert!(
        output.contains("protect") || output.contains("write"),
        "{output}"
    );
    println!("test unrestricted_write_cannot_claim_protected_helpers ... ok");

    // Ordinary native commands retain the same ref policy as the embedded
    // engine, including a nonstandard origin default and an unborn default
    // beside an existing feature branch.
    let mut regressions = Vec::new();
    for (branch, remote_default) in [("main", false), ("release", true)] {
        git(&repo, &["switch", "-q", "-c", branch]);
        if remote_default {
            git(
                &repo,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/release",
                ],
            );
        }
        let before = git(&repo, &["rev-parse", "HEAD"]).stdout;
        let output = execute(
            &repo,
            &tool,
            &authority,
            "git commit --allow-empty -m 'protected default'",
        )
        .await;
        let published = git(&repo, &["rev-parse", "HEAD"]).stdout != before;
        let confirmed = tool.drain_commit_success();
        if published || confirmed != 0 || !output.contains("default branch") {
            regressions.push(format!(
                "protected {branch}: published={published}, confirmed={confirmed}: {output}"
            ));
        }
        git(&repo, &["switch", "-q", "task"]);
    }
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/master"]);
    let output = execute(
        &repo,
        &tool,
        &authority,
        "git commit --allow-empty -m 'unborn protected default'",
    )
    .await;
    let confirmed = tool.drain_commit_success();
    if !output.contains("default branch")
        || confirmed != 0
        || repo.join(".git/refs/heads/master").exists()
    {
        regressions.push(format!(
            "unborn default with surviving branch: confirmed={confirmed}: {output}"
        ));
    }
    git(&repo, &["symbolic-ref", "HEAD", "refs/heads/task"]);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let subdir = repo.join("nested");
        std::fs::create_dir(&subdir).unwrap();
        let index_hook = repo.join(".git/operator-hooks/post-index-change");
        std::fs::write(
            &index_hook,
            "#!/bin/sh\nprintf 'index hook ran\\n' > .git/index-hook-audit\n",
        )
        .unwrap();
        std::fs::set_permissions(index_hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(repo.join(".git/hook-audit")).unwrap();
        std::fs::write(repo.join("README.md"), "changed by native commit -a\n").unwrap();
        let output = execute(
            &subdir,
            &tool,
            &authority,
            "git -c core.hooksPath=.git/operator-hooks commit -am 'relative hooks preserved'",
        )
        .await;
        assert_succeeded(&output);
        if std::fs::read_to_string(repo.join(".git/hook-audit")).unwrap_or_default()
            != "protected; no delegated fd\n"
        {
            regressions.push(format!("relative pre-commit hook did not run: {output}"));
        }
        if std::fs::read_to_string(repo.join(".git/index-hook-audit")).unwrap_or_default()
            != "index hook ran\n"
        {
            regressions.push(format!("post-index-change hook did not run: {output}"));
        }
        assert_eq!(tool.drain_commit_success(), 1);
    }
    assert!(regressions.is_empty(), "{}", regressions.join("\n\n"));
    println!("test native_commit_preserves_default_branch_policy ... ok");
    println!("test relative_hooks_and_post_index_change_keep_native_semantics ... ok");
}

async fn execute(
    repo: &std::path::Path,
    tool: &newt_git::LocalGitTool,
    authority: &newt_core::Caveats,
    command: &str,
) -> String {
    let mut mcp = newt_core::NoMcp;
    newt_core::execute_tool(
        "run_command",
        &serde_json::json!({"command": command}),
        &repo.to_string_lossy(),
        false,
        200,
        authority,
        &mut mcp,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(tool),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

fn assert_succeeded(output: &str) {
    assert!(
        !output.starts_with("error:") && !output.contains("fatal:"),
        "native command must finish successfully: {output}"
    );
}
