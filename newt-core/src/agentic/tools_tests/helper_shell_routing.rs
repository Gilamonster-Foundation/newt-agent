use super::*;

/// #898/#1022: `run_command_redirect` bounces embedded-tool-served LOCAL git
/// ops (and other direct tools), but lets git passthrough ops fall through
/// to the shell — otherwise a model can never `git push` or `git rm`.
#[test]
fn resolve_exec_cwd_confines_to_workspace() {
    // #1159: relative cwd joins under the workspace; absolute passes through
    // (the fs fence rejects escapes); empty/None → workspace root.
    assert_eq!(resolve_exec_cwd("/ws", None), "/ws");
    assert_eq!(resolve_exec_cwd("/ws", Some("")), "/ws");
    assert_eq!(resolve_exec_cwd("/ws", Some("  ")), "/ws");
    // Relative join uses the platform separator (correct — the cwd feeds
    // the confined shell on this OS); compare against the same join.
    let joined = std::path::Path::new("/ws")
        .join("crates/foo")
        .to_string_lossy()
        .into_owned();
    assert_eq!(resolve_exec_cwd("/ws", Some("crates/foo")), joined);
    // An absolute path passes through verbatim (platform-appropriate).
    let abs = if cfg!(windows) {
        "C:\\ws\\sub"
    } else {
        "/ws/sub"
    };
    assert_eq!(resolve_exec_cwd("/ws", Some(abs)), abs);
    // The dispatch args carry it verbatim.
    let a = confined_dispatch_args("ls", &joined);
    assert_eq!(a["cwd"], joined);
}

#[test]
fn split_leading_cd_folds_the_habitual_cd_prefix() {
    // The reported failure: `cd <workspace> && git checkout -b …` tried to
    // exec the `cd` builtin. Fold it: cwd = the path, run the real command.
    let (path, rest) = split_leading_cd("cd /ws/newt-agent && git checkout -b x");
    assert_eq!(path.as_deref(), Some("/ws/newt-agent"));
    assert_eq!(rest, "git checkout -b x");

    // `;` connective folds the same way.
    let (path, rest) = split_leading_cd("cd sub ; ls -la");
    assert_eq!(path.as_deref(), Some("sub"));
    assert_eq!(rest, "ls -la");

    // A quoted path with spaces is returned unquoted; the remainder is kept.
    let (path, rest) = split_leading_cd("cd \"/a b/c\" && cargo test");
    assert_eq!(path.as_deref(), Some("/a b/c"));
    assert_eq!(rest, "cargo test");

    // Only the FIRST cd is folded — a second cd stays in the remainder for
    // the shell engine (we don't chase chdir chains).
    let (path, rest) = split_leading_cd("cd a && cd b && ls");
    assert_eq!(path.as_deref(), Some("a"));
    assert_eq!(rest, "cd b && ls");

    // A bare `cd <path>` folds to an empty remainder (the caller turns this
    // into a guidance note — nothing to exec).
    let (path, rest) = split_leading_cd("cd /somewhere");
    assert_eq!(path.as_deref(), Some("/somewhere"));
    assert!(rest.is_empty());
}

#[test]
fn split_leading_cd_leaves_non_cd_and_ambiguous_commands_whole() {
    // Not a cd at all → unchanged.
    assert_eq!(split_leading_cd("git status"), (None, "git status".into()));
    // `cd` as a substring of another word is not a match.
    assert_eq!(split_leading_cd("cding foo"), (None, "cding foo".into()));
    // `cd <path>` followed by something other than a sequential connective
    // (a pipe here) is left whole — we only fold the safe `&&`/`;` shapes.
    assert_eq!(
        split_leading_cd("cd x | grep y"),
        (None, "cd x | grep y".into())
    );
}

#[test]
fn run_command_redirect_lets_git_network_ops_through() {
    // Ops the embedded git tool cannot do faithfully → fall through (None).
    for cmd in [
        "git push origin fix/foo",
        "git push",
        "git fetch origin",
        "git pull",
        "git clone https://example.com/r.git",
        "git rm src/cockpit.rs",
    ] {
        assert_eq!(run_command_redirect(cmd), None, "{cmd} must fall through");
    }
    // Local ops the embedded git tool handles → still redirect.
    for cmd in [
        "git status",
        "git log --oneline",
        "git add .",
        "git commit -m x",
    ] {
        assert_eq!(
            run_command_redirect(cmd),
            Some("git"),
            "{cmd} must redirect"
        );
    }
    // Other direct tools still redirect; plain shell commands run as-is.
    assert_eq!(run_command_redirect("read_file foo.txt"), Some("read_file"));
    assert_eq!(run_command_redirect("list_dir ."), Some("list_dir"));
    assert_eq!(run_command_redirect("cargo test"), None);
    assert_eq!(run_command_redirect("gh pr create --fill"), None);
    assert_eq!(run_command_redirect(""), None);
}

/// #1262: a command with shell COMPOSITION is a real shell program the
/// embedded tools cannot serve — it must never be redirected (the diagnosed
/// session's legitimate pipeline was bounced + miscounted as a corrected
/// hallucination). Bare servable forms keep redirecting.
#[test]
fn run_command_redirect_passes_composed_commands_through() {
    // The exact diagnosed pipeline.
    assert_eq!(
        run_command_redirect(
            "find . -name \"*.rs\" -type f -print0 | xargs -0 du -k | sort -rn | head 20"
        ),
        None,
        "a pipeline leading with `find` is not a misdirected find call"
    );
    // Redirects and sequencing are composition too.
    assert_eq!(run_command_redirect("find . -name '*.log' > out.txt"), None);
    assert_eq!(run_command_redirect("git status && git diff"), None);
    assert_eq!(run_command_redirect("list_dir . ; echo done"), None);
    assert_eq!(run_command_redirect("read_file $(pick_file)"), None);
    assert_eq!(run_command_redirect("read_file `pick_file`"), None);
    // Bare servable forms still redirect (the true positives hold).
    assert_eq!(run_command_redirect("find . -name \"*.rs\""), Some("find"));
    assert_eq!(run_command_redirect("list_dir src"), Some("list_dir"));
    assert_eq!(run_command_redirect("git status"), Some("git"));
}

/// #1709 family: a COMPOSED `run_command` that creates a git commit bypasses
/// `LocalGitTool::finalize_commit_message` and would land an unattributed
/// commit. The guard `run_command_creates_shell_git_commit` detects it so the
/// run_command arm can refuse predictably and direct the model to the
/// first-class `git` tool. A bare `git commit` is already bounced by
/// `run_command_redirect` (covered above); these are the composed forms that
/// fall through.
#[test]
fn run_command_creates_shell_git_commit_detects_composed_commit_forms() {
    // Sequencing + commit.
    assert!(run_command_creates_shell_git_commit(
        "git add . && git commit -m \"fix the parser\""
    ));
    assert!(run_command_creates_shell_git_commit(
        "git add . ; git commit -m x"
    ));
    // Pipeline commit (e.g. message from stdin).
    assert!(run_command_creates_shell_git_commit(
        "echo \"msg\" | git commit -F -"
    ));
    // Redirect is composition.
    assert!(run_command_creates_shell_git_commit(
        "git commit -m x > commit.log"
    ));
    // Global option with a value before the subcommand.
    assert!(run_command_creates_shell_git_commit(
        "git -c user.email=evil@example.com commit -m x"
    ));
    assert!(run_command_creates_shell_git_commit(
        "git -C /repo commit -m x"
    ));
    // `--git-dir=<path>` carries its value in-token, so the next token IS
    // the subcommand.
    assert!(run_command_creates_shell_git_commit(
        "git --git-dir=/repo/.git commit -m x"
    ));
    // A bare flag global option before the subcommand.
    assert!(run_command_creates_shell_git_commit(
        "git --no-pager commit -m x"
    ));
    // `--amend` is still the `commit` subcommand.
    assert!(run_command_creates_shell_git_commit(
        "git add . && git commit --amend -m x"
    ));
    // Qualified binary path (the model often uses /usr/bin/git).
    assert!(run_command_creates_shell_git_commit(
        "/usr/bin/git -C /repo commit -m x"
    ));
    // Env-assignment prefix forging commit identity.
    assert!(run_command_creates_shell_git_commit(
        "GIT_AUTHOR_NAME=evil GIT_AUTHOR_EMAIL=evil@example.com git commit -m x"
    ));
    // Command substitution / backtick wrapping.
    assert!(run_command_creates_shell_git_commit("$(git commit -m x)"));
    assert!(run_command_creates_shell_git_commit("`git commit -m x`"));
}

/// #1709 family: the guard must NOT fire on legitimate read-only git, git
/// network ops, unrelated commands, or the abort/quit forms of the
/// commit-producing subcommands — the bypass closure is narrow to commit
/// creation only. (`git merge`/`cherry-pick`/`revert`/`rebase` themselves
/// ARE blocked now — see `run_command_creates_shell_git_commit_detects_other_commit_forms`.)
#[test]
fn run_command_creates_shell_git_commit_preserves_readonly_and_unrelated() {
    // Read-only composed git.
    assert!(!run_command_creates_shell_git_commit(
        "git status && git diff"
    ));
    assert!(!run_command_creates_shell_git_commit(
        "git log | grep commit"
    ));
    assert!(!run_command_creates_shell_git_commit(
        "git log --grep=commit --oneline"
    ));
    // Git network passthrough ops.
    assert!(!run_command_creates_shell_git_commit(
        "git add . && git push origin fix/foo"
    ));
    assert!(!run_command_creates_shell_git_commit("git fetch"));
    // `commit` appearing as an ARGUMENT, not a subcommand.
    assert!(!run_command_creates_shell_git_commit(
        "echo git commit > notes.txt"
    ));
    assert!(!run_command_creates_shell_git_commit("cat commit_log.txt"));
    // Non-git commands.
    assert!(!run_command_creates_shell_git_commit("cargo test"));
    assert!(!run_command_creates_shell_git_commit("just check"));
    assert!(!run_command_creates_shell_git_commit(""));
    // Abort/quit forms create NO commit — preserved (fall through). These
    // back out of an in-progress op without creating a commit.
    assert!(!run_command_creates_shell_git_commit("git rebase --abort"));
    assert!(!run_command_creates_shell_git_commit("git rebase --quit"));
    assert!(!run_command_creates_shell_git_commit(
        "git cherry-pick --abort"
    ));
    assert!(!run_command_creates_shell_git_commit("git revert --quit"));
    assert!(!run_command_creates_shell_git_commit("git merge --abort"));
    assert!(!run_command_creates_shell_git_commit("git merge --quit"));
    // `--skip`/`--continue` DO create commits (they advance the operation),
    // so they are NOT abort forms — covered as blocked below.
}

/// #1709 family (audit req 7/8): the other audit-identified commit-producing
/// shell forms — `git merge`, `git cherry-pick`, `git revert`, `git rebase`
/// — are now BLOCKED (route or deny), bare AND composed, while their
/// `--abort`/`--quit` forms pass through (above). `--skip`/`--continue`
/// create commits and stay blocked.
#[test]
fn run_command_creates_shell_git_commit_detects_other_commit_forms() {
    // Bare commit-producing subcommands.
    assert!(run_command_creates_shell_git_commit("git merge feature/x"));
    assert!(run_command_creates_shell_git_commit(
        "git cherry-pick abc123"
    ));
    assert!(run_command_creates_shell_git_commit("git revert abc123"));
    assert!(run_command_creates_shell_git_commit("git rebase main"));
    // Composed with a preceding `git add`.
    assert!(run_command_creates_shell_git_commit(
        "git add . && git merge feature/x"
    ));
    // Qualified binary path.
    assert!(run_command_creates_shell_git_commit(
        "/usr/bin/git -C /repo cherry-pick abc123"
    ));
    // Env-assignment prefix.
    assert!(run_command_creates_shell_git_commit(
        "GIT_AUTHOR_NAME=evil git revert abc123"
    ));
    // `--skip` and `--continue` advance the operation and CREATE commits —
    // they are NOT abort forms, so they stay blocked.
    assert!(run_command_creates_shell_git_commit("git rebase --skip"));
    assert!(run_command_creates_shell_git_commit(
        "git cherry-pick --continue"
    ));
    assert!(run_command_creates_shell_git_commit(
        "git rebase --continue"
    ));
    // An abort flag ANYWHERE in the args exempts a non-commit subcommand
    // (a real abort never creates a commit).
    assert!(!run_command_creates_shell_git_commit(
        "git rebase --abort --continue"
    ));
}

/// #1709 family (req 13): a model cannot escape harness-managed attribution
/// by shelling out `git commit` with its OWN hand-written trailer or forged
/// identity — the guard blocks the commit attempt ENTIRELY, so the model is
/// forced onto the `git` tool, whose finalizer owns attribution. The model's
/// text never reaches a real `git commit`.
#[test]
fn run_command_creates_shell_git_commit_blocks_model_forged_attribution() {
    // Model hand-writes a trailer to impersonate harness attribution.
    assert!(run_command_creates_shell_git_commit(
        "git commit -m \"fix the parser\n\nCo-authored-by: fake (newt-agent v9.9) <x@y>\""
    ));
    // Model forges the author identity via -c.
    assert!(run_command_creates_shell_git_commit(
        "git -c user.name='newt-agent' -c user.email='x@y' commit -m x"
    ));
    // Model forges identity via env prefix.
    assert!(run_command_creates_shell_git_commit(
        "GIT_AUTHOR_NAME=newt-agent GIT_AUTHOR_EMAIL=x@y git commit -m x"
    ));
    // Model tries to suppress attribution by emptying the message via a file.
    assert!(run_command_creates_shell_git_commit(
        "printf '' | git commit -F -"
    ));
}

/// #1709 family: a bare `git commit` is already caught by
/// `run_command_redirect` (the existing bounce to the `git` tool); the new
/// guard is the composed-shell fallback, not a duplicate of the bare case.
#[test]
fn bare_git_commit_is_caught_by_redirect_not_the_shell_guard() {
    assert_eq!(run_command_redirect("git commit"), Some("git"));
    assert_eq!(run_command_redirect("git commit --amend -m x"), Some("git"));
    // The shell guard also reports it (defense in depth), but the redirect
    // fires first in the run_command arm.
    assert!(run_command_creates_shell_git_commit("git commit"));
}

/// #1262: the loop's `hallucination_count` increments exactly on
/// `is_hallucination` (mod.rs call classification), so this pure pin IS the
/// turn-metrics behavior: the diagnosed pipeline counts ZERO hallucinations;
/// a bare misdirected call still counts one.
#[test]
fn pipeline_is_never_counted_as_a_hallucination() {
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command":
                "find . -name \"*.rs\" -type f -print0 | xargs -0 du -k | sort -rn | head 20"})
    ));
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git status && git diff"})
    ));
    // The true positive holds: a bare misdirected call still counts.
    assert!(is_hallucination(
        "run_command",
        &serde_json::json!({"command": "list_dir ."})
    ));
}

/// #898 regression: a real `git push` at run_command must NOT be counted as
/// a hallucination (it now runs), while a local `git status` still is.
#[test]
fn is_hallucination_allows_git_network_ops() {
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git push origin fix/foo"})
    ));
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git fetch"})
    ));
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git rm src/cockpit.rs"})
    ));
    assert!(is_hallucination(
        "run_command",
        &serde_json::json!({"command": "git status"})
    ));
}
