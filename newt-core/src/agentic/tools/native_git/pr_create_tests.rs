// #2740 regression tests, included in the existing isolated broker fixture.

/// #2740: literal base/head assertions and output wrappers reach approval.
#[test]
fn issue_2740_literal_pr_create_reaches_operator_approval() {
    let _env = BrokerEnv::new();
    let repo = repo_on_feature_branch();
    git(
        repo.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    for source in [
        "gh pr create --base main --head task --title 'A title' --body 'A body'",
        "gh pr create --base=main --head=task --title='A title' --body='A body' 2>&1",
        "gh pr create --title='A title' --body='A body' --head=task --base=main 2>&1 | head -5",
    ] {
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let plan = plan_pr_create_test(
            source,
            repo.path(),
            &with_net(Scope::only([])),
            &mut Some(&mut gate),
        )
        .unwrap();
        assert_eq!(
            (
                plan.base.as_str(),
                plan.head.as_str(),
                plan.title.as_str(),
                plan.body.as_str()
            ),
            ("main", "task", "A title", "A body")
        );
        assert_eq!(gate.requests.len(), 1);
        assert_eq!(gate.requests[0].target, "github.com");
    }
}

/// #2740: every refusal names a safe reason and exact supported command,
/// without reflecting model-controlled flags or repository values.
#[test]
fn issue_2740_pr_create_refusals_are_actionable_and_safe() {
    if !fence() {
        return;
    }
    let _env = BrokerEnv::new();
    let repo = repo_on_feature_branch();
    git(
        repo.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    for (source, reason) in [
        (
            "gh pr create --base private-base --head task --title t --body b",
            "--base must match the repository default branch",
        ),
        (
            "gh pr create --base main --head private-head --title t --body b",
            "--head must match the checked-out branch",
        ),
        ("gh pr create --body b", "--title is required"),
        ("gh pr create --title t", "--body is required"),
        (
            "gh pr create --title t --body b --private-flag",
            "unsupported gh pr create flag",
        ),
        (
            "gh pr create --title t --body b > private-output",
            "redirect",
        ),
    ] {
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let out = execute_governed_pr_create(
            source,
            repo.path(),
            &with_net(Scope::only([])),
            &mut Some(&mut gate),
        );
        assert!(out.contains(reason), "{source}: {out}");
        assert!(out.contains("gh pr create --base <default-branch> --head <current-branch> --title '<title>' --body '<body>'"), "{out}");
        assert!(!out.contains("private-"), "untrusted bytes leaked: {out}");
        assert!(gate.requests.is_empty(), "invalid request reached approval");
    }
}

/// #2740: approval is followed by the existing staged gh executor. The fake
/// gh records argv; no request is sent to GitHub.
#[test]
fn issue_2740_approved_pr_create_uses_verified_branches() {
    if !fence() {
        return;
    }
    let env = BrokerEnv::new();
    let repo = repo_on_feature_branch();
    git(
        repo.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    let argv = env.home.path().join("pr-argv");
    std::fs::write(
        env.gh(),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\necho 'https://github.com/o/r/pull/7'\n",
            argv.display()
        ),
    )
    .unwrap();
    let mut gate = Gate {
        allow: true,
        requests: vec![],
    };
    let (out, _) = dispatch_run_command_with_gate(
        repo.path(),
        "gh pr create --base=main --head=task --title='A title' --body='A body' 2>&1 | head -1",
        &with_net(Scope::only([])),
        Some(&mut gate),
    );
    assert!(
        out.contains("pr_created https://github.com/o/r/pull/7"),
        "{out}"
    );
    assert_eq!(gate.requests.len(), 1);
    assert_eq!(std::fs::read_to_string(argv).unwrap(), "pr\ncreate\n--repo\ngithub.com/o/r\n--base\nmain\n--head\ntask\n--title\nA title\n--body\nA body\n");
}

/// #2740: use the linked worktree's HEAD and the recorded non-main default,
/// not the primary checkout or a hard-coded fallback.
#[test]
fn issue_2740_linked_worktree_and_non_main_default() {
    let _env = BrokerEnv::new();
    let repo = repo_on_feature_branch();
    git(repo.path(), &["branch", "-m", "main", "trunk"]);
    git(
        repo.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    let linked = tempdir();
    let worktree = linked.path().join("linked");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-b",
            "linked-task",
            worktree.to_str().unwrap(),
        ],
    );
    let plan = plan_pr_create_test(
        "gh pr create --base=trunk --head=linked-task --title=t --body=b",
        &worktree,
        &scoped_caveats(),
        &mut None,
    )
    .unwrap();
    assert_eq!(
        (plan.base.as_str(), plan.head.as_str()),
        ("trunk", "linked-task")
    );
    let error = plan_pr_create_test(
        "gh pr create --base=trunk --head=task --title=t --body=b",
        &worktree,
        &scoped_caveats(),
        &mut None,
    )
    .unwrap_err();
    assert!(error.contains("--head must match"), "{error}");
}

/// #2740: unknown branch authority and declined approval explain the next
/// action without running gh or leaking the rejected command's content.
#[test]
fn issue_2740_unknown_default_and_denied_approval_explain_refusal() {
    if !fence() {
        return;
    }
    let _env = BrokerEnv::new();
    let repo = repo_on_feature_branch();
    let source = "gh pr create --title=t --body=b 2>&1 | head -1";
    let mut gate = Gate {
        allow: false,
        requests: vec![],
    };
    let out = execute_governed_pr_create(
        source,
        repo.path(),
        &with_net(Scope::only([])),
        &mut Some(&mut gate),
    );
    assert!(
        out.contains("an operator must approve PR creation"),
        "{out}"
    );
    assert!(out.contains(PR_CREATE_RETRY), "{out}");
    assert_eq!(gate.requests.len(), 1);
    gate.requests.clear();
    git(
        repo.path(),
        &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
    );
    let out = execute_governed_pr_create(
        source,
        repo.path(),
        &with_net(Scope::only([])),
        &mut Some(&mut gate),
    );
    assert!(
        out.contains("repository default branch is unknown"),
        "{out}"
    );
    assert!(out.contains(PR_CREATE_RETRY), "{out}");
    assert!(gate.requests.is_empty());
}

/// #2740: ambiguous flags and executable shell shapes stay outside approval.
#[test]
fn issue_2740_ambiguous_or_dynamic_commands_are_refused() {
    if !fence() {
        return;
    }
    let _env = BrokerEnv::new();
    let repo = repo_on_feature_branch();
    for source in [
        "gh pr create --title t --body b --head task --head other",
        "gh pr create --title --body b",
        "gh pr create --title=t --body",
        "gh pr create --title='' --body=b",
        "gh pr create --title=t --body=b --base",
        "gh pr create --title=t --body=b --head",
        "gh pr create --title=$SECRET --body=b",
        "gh pr create --title=t --body=$(id)",
        "gh pr create --title=t --body=b && head -1",
        "gh pr create --title=t --body=b | head private-file",
        "TOKEN=private-secret gh pr create --title=t --body=b",
    ] {
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let out = execute_governed_pr_create(
            source,
            repo.path(),
            &with_net(Scope::only([])),
            &mut Some(&mut gate),
        );
        assert!(out.contains("refused:"), "{source}: {out}");
        assert!(out.contains(PR_CREATE_RETRY), "{source}: {out}");
        assert!(!out.contains("private-"), "{out}");
        assert!(gate.requests.is_empty(), "{source}");
    }
}

/// #2740: parsing flags must preserve ordinary dash-prefixed Markdown data.
#[test]
fn issue_2740_literal_title_and_body_content_is_preserved() {
    let _env = BrokerEnv::new();
    let repo = repo_on_feature_branch();
    let plan = plan_pr_create_test(
        "gh pr create --title '- title' --body '- body'",
        repo.path(),
        &scoped_caveats(),
        &mut None,
    )
    .unwrap();
    assert_eq!(plan.title, "- title");
    assert_eq!(plan.body, "- body");
}
