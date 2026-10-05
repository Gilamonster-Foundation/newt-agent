/// The fixture git, by absolute path: an inherited `PATH` cannot substitute
/// another binary, and it is the same root-owned `/usr/bin/git` the #2630
/// exec-path alias is proven against. The kernel-fence tests skip without
/// it; the rest fail loudly ([`hermetic_git`]).
const FIXTURE_GIT: &str = "/usr/bin/git";

/// The explicit, minimal environment every fixture git runs under, for BOTH
/// the unconfined setup ([`hermetic_git`]) and the confined dispatch (the
/// `"env"` seam of `dispatch_bridled_shell`): one definition, so "isolated
/// enough not to touch a real repository" and "isolated enough to be a fair
/// confinement proof" cannot drift apart. A private `HOME` plus
/// `GIT_CONFIG_NOSYSTEM`/`GIT_CONFIG_GLOBAL`/`GIT_TEMPLATE_DIR` close the
/// config and template sources that live outside the process environment
/// (`/etc/gitconfig`, the operator's `~/.gitconfig`, `~/.config/git`).
pub(in crate::agentic::tools) fn hermetic_git_env(
    home: &std::path::Path,
) -> std::collections::BTreeMap<String, String> {
    let home = home.to_string_lossy();
    [
        ("HOME", home.as_ref()),
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@example.invalid"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@example.invalid"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TEMPLATE_DIR", "/dev/null"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// Mirror of the vendored core's test-private `hermetic_git_command`
/// (`vendor/agent-bridle-core/src/sandbox.rs`, bridle PR #407): `env_clear()`
/// rather than a denylist, so an inherited `GIT_DIR`, `GIT_WORK_TREE`,
/// `GIT_CEILING_DIRECTORIES`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`,
/// `GIT_OBJECT_DIRECTORY` or any other ambient git knob cannot leak in,
/// because nothing is inherited. Proven by
/// [`hostile_inherited_git_env_cannot_redirect_the_worktree_add_fixture`].
/// Widened to `pub(in crate::agentic::tools)` (#2681 round 3) so the
/// `execute_tool_branch_tests::permissions` git-broker fixture reuses it
/// too, rather than a second ad hoc git fixture that inherits the ambient
/// `GIT_DIR`/`HOME`/hooks.
pub(in crate::agentic::tools) fn hermetic_git(
    dir: &std::path::Path,
    home: &std::path::Path,
) -> std::process::Command {
    assert!(
        std::path::Path::new(FIXTURE_GIT).exists(),
        "the fixture git {FIXTURE_GIT} is absent"
    );
    let mut cmd = std::process::Command::new(FIXTURE_GIT);
    cmd.current_dir(dir)
        .env_clear()
        .envs(hermetic_git_env(home));
    cmd
}

/// The real-kernel-fence tests in this file need Landlock AND the
/// root-owned fixture git; both are present in CI. A bare `return` on
/// either being missing would make a quietly-green local run look identical
/// to one that measured nothing — print an explicit line so a skip is
/// visible in test output instead (#2686 review round 2).
fn skip_without_real_kernel_fence() -> bool {
    let available = crate::confined_exec::kernel_fs_fence_available()
        && std::path::Path::new(FIXTURE_GIT).exists();
    if !available {
        eprintln!(
            "skip: real-kernel-fence test requires Landlock + {FIXTURE_GIT}, neither of \
             which is available on this host"
        );
    }
    !available
}

/// Unconfined fixture setup (`init`/`add`/`commit`/`worktree add`) under a
/// throwaway private `HOME` that lives only for the one command.
fn real_git(dir: &std::path::Path, args: &[&str]) {
    let home = tempfile::tempdir().unwrap();
    let ok = hermetic_git(dir, home.path())
        .args(args)
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
    if skip_without_real_kernel_fence() {
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

    let home = tempfile::tempdir().unwrap();
    let status = hermetic_git(&wt, home.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("A  f.txt"),
        "f.txt must be staged: {}",
        String::from_utf8_lossy(&status.stdout)
    );
}

/// #2630 — a `git`-only exec grant must let the confined shell run git
/// porcelain that re-executes git under an internal alias: `git worktree
/// add` spawns `<exec-path>/git branch` / `… update-ref`, which the kernel
/// `Execute` allow-list denied because only the granted `git` binary itself
/// was on it. Fixed upstream in agent-bridle#407 (`landlock_impl::
/// resolve_exec_paths` folds in `<exec-path>/git` when it is the same image
/// as a root-owned, trusted-dir `git`), backported into the vendored core.
///
/// Confirmed red on the pre-backport vendored copy: exit 255,
/// `fatal: cannot exec 'branch': Permission denied`.
///
/// Real-resource proof (real `/usr/bin/git`, real Landlock), placed beside
/// the existing kernel-confined `git add` proof above rather than in the
/// weekly tier: like it, this skips (never fails) without Landlock or
/// `/usr/bin/git`, and it is the ground truth for the vendored
/// `git_exec_path_tests`, which only check the in-process allow-list and
/// never ask the kernel. No `/etc/gitconfig` read grant is needed: the
/// confined dispatch carries [`hermetic_git_env`], whose
/// `GIT_CONFIG_NOSYSTEM=1` means git never opens the system config.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn confined_shell_git_worktree_add_succeeds_under_a_git_only_exec_grant() {
    if skip_without_real_kernel_fence() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    worktree_add_under_git_only_exec_grant(root.path()).await;
}

/// The #2630 fixture proper, built under `root`: an unconfined `init`/`add`/
/// `commit` of `<root>/main`, then the Landlock-confined `git worktree add`
/// under a `git`-only exec grant. Both halves run under [`hermetic_git_env`]
/// with a private `HOME` inside `root`: the setup through [`hermetic_git`],
/// the confined command through the dispatch's `"env"` seam (the engine
/// `env_clear`s the child and applies only that map, so nothing of this
/// process's environment reaches either git). Shared by the test above and
/// by the hostile-environment subprocess of
/// [`hostile_inherited_git_env_cannot_redirect_the_worktree_add_fixture`],
/// so the isolation claim is made about exactly the fixture that runs.
#[cfg(target_os = "linux")]
async fn worktree_add_under_git_only_exec_grant(root: &std::path::Path) {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let main = root.join("main");
    std::fs::create_dir(&main).unwrap();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    let setup = |args: &[&str]| {
        let ok = hermetic_git(&main, &home)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    };
    setup(&["init", "-q"]);
    std::fs::write(main.join("seed"), "x").unwrap();
    setup(&["add", "seed"]);
    setup(&["commit", "-q", "-m", "init"]);
    let wt = root.join("wt");

    let root_s = root.to_string_lossy().into_owned();
    let session = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([root_s.clone()]),
        fs_write: crate::caveats::Scope::only([root_s]),
        exec: crate::caveats::Scope::only(["git".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let cmd = format!("git worktree add -q {} -b task", wt.display());
    let envelope = super::shell::dispatch_bridled_shell(
        serde_json::json!({
            "cmd": cmd,
            "cwd": main.to_string_lossy(),
            "env": hermetic_git_env(&home),
        }),
        &session,
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
        "git worktree add must succeed under a git-only exec grant: {envelope}"
    );
    assert!(
        wt.join("seed").exists(),
        "the new worktree must actually be checked out"
    );
}

/// Env var whose presence makes a re-execution of this test binary run as
/// the dedicated hostile-environment subprocess of
/// [`hostile_inherited_git_env_cannot_redirect_the_worktree_add_fixture`]
/// (its value is the fixture root). Set ONLY on the `Command` that spawns
/// that one child, never on this process: a process-global `set_var` races
/// every other test in the binary.
#[cfg(target_os = "linux")]
const HOSTILE_ENV_SUBPROCESS_ROOT: &str = "NEWT_2630_HOSTILE_ENV_SUBPROCESS_ROOT";

/// PR #2670 fix-first, P1: the fixture's unconfined `git init`/`add`/`commit`
/// must not be redirectable by the environment the test process inherited.
/// A developer shell that exports `GIT_DIR`/`GIT_WORK_TREE` (the dotfiles
/// idiom: `GIT_DIR=~/.dotfiles GIT_WORK_TREE=~`) or carries a `~/.gitconfig`
/// naming `core.hooksPath` would otherwise have the setup commit INTO that
/// external repository and run its hooks, which is code execution outside
/// the fixture.
///
/// The adversary is real, not sentinels nothing points at: this test
/// re-executes its own binary with `--exact` against only itself, and that
/// child's `Command` environment (this process's own is never mutated)
/// carries `GIT_DIR` = an external sentinel repository's gitdir,
/// `GIT_WORK_TREE` = the fixture root (so the fixture's files lie INSIDE the
/// hostile work tree, exactly as a tempdir under `~` does under the dotfiles
/// idiom), and `HOME` = a hostile home whose `.gitconfig` sets
/// `core.hooksPath` to a `pre-commit` hook that leaves a mark. The child
/// runs the real fixture through [`worktree_add_under_git_only_exec_grant`];
/// this parent then checks the sentinels are byte-identical, the hook never
/// ran, and the fixture actually completed (`<root>/wt/seed` exists, so a
/// child that matched no test cannot pass vacuously).
///
/// Confirmed red with the pre-fix `real_git` (bare `git`, inherited
/// environment): the setup's `add`/`commit` landed in the sentinel
/// repository and its hook ran. The sentinel repository is this test's own
/// disposable fixture, never an operator repository.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn hostile_inherited_git_env_cannot_redirect_the_worktree_add_fixture() {
    // Re-entry: the same test, running as the dedicated hostile subprocess.
    if let Ok(root) = std::env::var(HOSTILE_ENV_SUBPROCESS_ROOT) {
        assert!(
            crate::confined_exec::kernel_fs_fence_available(),
            "the parent re-executes only on a Landlock host"
        );
        worktree_add_under_git_only_exec_grant(std::path::Path::new(&root)).await;
        return;
    }
    if skip_without_real_kernel_fence() {
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let hostile = tempfile::tempdir().unwrap();

    // The external sentinel repository: a real repo with one commit, whose
    // gitdir the hostile `GIT_DIR` names.
    let sentinel = hostile.path().join("sentinel");
    std::fs::create_dir(&sentinel).unwrap();
    real_git(&sentinel, &["init", "-q", "-b", "main"]);
    std::fs::write(sentinel.join("seed"), "sentinel").unwrap();
    real_git(&sentinel, &["add", "seed"]);
    real_git(&sentinel, &["commit", "-q", "-m", "sentinel"]);
    let sentinel_gitdir = sentinel.join(".git");
    let index_before = std::fs::read(sentinel_gitdir.join("index")).unwrap();
    let head_before = std::fs::read(sentinel_gitdir.join("refs/heads/main")).unwrap();

    // A hostile HOME whose global config routes hooks to a directory whose
    // `pre-commit` leaves a mark.
    let hostile_home = hostile.path().join("home");
    let hooks = hostile_home.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook_mark = hostile.path().join("hook-ran");
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, format!("#!/bin/sh\n: > '{}'\n", hook_mark.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        hostile_home.join(".gitconfig"),
        format!("[core]\n\thooksPath = {}\n", hooks.display()),
    )
    .unwrap();

    // libtest names a test by its module path without the crate prefix.
    let this_test = format!(
        "{}::hostile_inherited_git_env_cannot_redirect_the_worktree_add_fixture",
        module_path!().split_once("::").map_or("", |(_, rest)| rest)
    );
    let exe = std::env::current_exe().expect("current_exe must resolve for the re-exec proof");
    let output = std::process::Command::new(&exe)
        .args(["--exact", &this_test, "--nocapture", "--test-threads=1"])
        // Explicit, minimal: this Command's env is what the child inherits.
        // PATH is for the re-executed test binary's own machinery.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env(HOSTILE_ENV_SUBPROCESS_ROOT, root.path())
        .env("HOME", &hostile_home)
        .env("GIT_DIR", &sentinel_gitdir)
        .env("GIT_WORK_TREE", root.path())
        .output()
        .expect("spawn hostile-env subprocess");

    // Sentinels first, so a red names the mutation rather than its fallout.
    assert!(
        !hook_mark.exists(),
        "a hook from the inherited HOME's config must never run"
    );
    assert_eq!(
        std::fs::read(sentinel_gitdir.join("index")).unwrap(),
        index_before,
        "an inherited GIT_DIR/GIT_WORK_TREE must never redirect the fixture's `git add`"
    );
    assert_eq!(
        std::fs::read(sentinel_gitdir.join("refs/heads/main")).unwrap(),
        head_before,
        "an inherited GIT_DIR/GIT_WORK_TREE must never redirect the fixture's `git commit`"
    );
    assert!(
        output.status.success(),
        "hostile-env subprocess fixture failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        root.path().join("wt").join("seed").exists(),
        "the subprocess must have run the fixture to completion, not matched no test"
    );
}

/// #2682: when the session's OWN workspace IS the linked worktree (the
/// production shape `apply_cli_fs_grants` builds — `fs_write` scoped to the
/// worktree only, with the common gitdir reached only through the per-
/// dispatch widening below), a `git commit` must succeed there, not only
/// `git add`.
///
/// Round 2 (#2686 review round 2): a confined `git commit` no longer gets
/// write on the common dir's `refs/heads/`/`logs/refs/heads/` AT ALL — round
/// 5 (`8c537f17`) granted those two directories so `git commit` could
/// advance the checked-out branch's own ref, but a directory-wide grant
/// there covers EVERY branch's ref/reflog, and a compound command's second,
/// non-`git` segment can reach them (`confined_shell_cannot_redirect_…`
/// below, red under that grant). This test now drives the REPLACEMENT
/// mechanism end to end: `HEAD` detached at the branch's current tip
/// ([`crate::git_hardening::detach_own_head`]) so the confined commit only
/// ever touches `objects/` + the worktree's own admin dir, then the
/// host-side, bounded `update-ref`
/// ([`crate::git_hardening::advance_own_branch_ref`]) that publishes it,
/// then reattachment ([`crate::git_hardening::reattach_own_head`]) — the
/// exact sequence `agentic::tools`'s `run_command` arm performs around this
/// same confined dispatch.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn confined_shell_git_commit_succeeds_in_a_linked_worktree_on_a_non_default_branch() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if skip_without_real_kernel_fence() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
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

    // The same wiring `apply_cli_fs_grants` produces in production: `fs_write`
    // scoped to the worktree ONLY — the common dir is reached only through
    // the per-dispatch widening under test.
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
    let cmd = "git add f.txt && git commit -q -m update";
    let widened =
        super::shell::dispatch_caveats_for_git_shell(cmd, &wt.to_string_lossy(), &session);

    // What `agentic::tools`'s `run_command` arm does before the confined
    // dispatch: resolve the branch/tip and detach HEAD to it.
    let crate::git_hardening::OwnBranchRefMove {
        branch,
        old_tip,
        identity,
    } = crate::git_hardening::own_branch_for_commit_ref_move(&wt)
        .expect("task is a non-default, born branch");
    assert_eq!(branch, "task");
    crate::git_hardening::detach_own_head(&identity, &old_tip).unwrap();

    let envelope = super::shell::dispatch_bridled_shell(
        serde_json::json!({
            "cmd": cmd,
            "cwd": wt.to_string_lossy(),
            "env": hermetic_git_env(&home),
        }),
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
        "git commit must succeed under the (narrowed) fence with HEAD detached: {envelope}"
    );

    // What the arm does after a successful confined dispatch: publish the
    // detached commit onto the branch, host-side, then reattach.
    crate::git_hardening::advance_own_branch_ref(&identity, &branch, &old_tip).unwrap();
    crate::git_hardening::reattach_own_head(&identity, &branch).unwrap();

    let log = hermetic_git(&wt, &home)
        .args(["log", "--oneline", "-1", "refs/heads/task"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("update"),
        "the commit must actually land on refs/heads/task: {}",
        String::from_utf8_lossy(&log.stdout)
    );
    let branch_ref = hermetic_git(&wt, &home)
        .args(["symbolic-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&branch_ref.stdout).trim(),
        "refs/heads/task",
        "HEAD must be reattached, not left detached"
    );
}

/// #2686 review round 2, P1: a compound confined-shell command whose
/// LEADING program is `git` shares its (possibly widened) fence with every
/// OTHER program in the same dispatch — `dispatch_caveats_for_git_shell`'s
/// own doc comment names this as an accepted trade-off for the common
/// `objects/` directory, but round 5's (`8c537f17`) `refs/heads/`/
/// `logs/refs/heads/` widening extended the SAME trade-off to every
/// branch's ref and reflog, not just the checked-out one. A plain shell
/// redirect is not a `git` VERB, so `inspect_commands`'s unconditional
/// `update-ref`/`push`/`branch -f` refusal
/// (`newt-core::agentic::tools::native_git`) never inspects it — the
/// directory-wide Landlock grant was the only thing standing in the way.
///
/// Measured red against `8c537f17` (confirmed by temporarily restoring
/// round 5's two extra directories to `own_gitdir_grants`/
/// `own_gitdir_shell_write_grant`): every sub-case below SUCCEEDED
/// (`exit_code == 0`, sentinel bytes CHANGED) — the compound command
/// reached past the checked-out branch into a sibling branch's ref/reflog
/// and into `main`'s own ref/reflog. Green once those two directories are
/// never granted to a confined child at all.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn confined_shell_cannot_redirect_into_a_sibling_or_default_branch_ref_or_reflog() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if skip_without_real_kernel_fence() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    real_git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("seed"), "x").unwrap();
    real_git(&main, &["add", "seed"]);
    real_git(&main, &["commit", "-q", "-m", "init"]);
    // A sibling branch, so its ref/reflog exist to be targeted.
    real_git(&main, &["branch", "other"]);
    let wt = root.path().join("wt");
    real_git(
        &main,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
    );
    std::fs::write(wt.join("f.txt"), "hi\n").unwrap();
    let own_git = crate::git_hardening::own_gitdir_grants(&wt);

    let common = main.join(".git");
    // `fs_read` includes the own-gitdir read grant (common dir + worktree
    // gitdir), same as production (`apply_cli_fs_grants`) and the positive
    // test above — WITHOUT it, `git add` itself fails before the `&&`
    // reaches the redirect, which would make every sub-case below pass for
    // the wrong reason (nothing ran) rather than because the redirect was
    // denied.
    let mut read_roots = vec![wt.to_string_lossy().into_owned()];
    read_roots.extend(own_git.read);
    // `exec` includes `echo` (not just `git`): the realistic shape this
    // proves is a session that already has ordinary shell exec authority
    // (the common case), where the FS grant is the only thing that can
    // stop a redirect — not an artificially git-only exec scope that would
    // deny `echo` before the FS fence is ever reached.
    let session = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only(read_roots),
        fs_write: crate::caveats::Scope::only([wt.to_string_lossy().into_owned()]),
        exec: crate::caveats::Scope::only(["git".to_string(), "echo".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };

    for (label, target) in [
        ("sibling branch's ref", common.join("refs/heads/other")),
        (
            "sibling branch's reflog",
            common.join("logs/refs/heads/other"),
        ),
        ("default branch's ref", common.join("refs/heads/main")),
        (
            "default branch's reflog",
            common.join("logs/refs/heads/main"),
        ),
    ] {
        let before = std::fs::read(&target).unwrap_or_default();
        let cmd = format!("git add f.txt && echo payload >> {}", target.display());
        let widened =
            super::shell::dispatch_caveats_for_git_shell(&cmd, &wt.to_string_lossy(), &session);
        let envelope = super::shell::dispatch_bridled_shell(
            serde_json::json!({
                "cmd": cmd,
                "cwd": wt.to_string_lossy(),
                "env": hermetic_git_env(&home),
            }),
            &widened,
            None,
        )
        .await
        .expect("dispatch");
        let after = std::fs::read(&target).unwrap_or_default();
        assert_eq!(
            before,
            after,
            "{label} ({}) must be byte-identical after a confined-shell commit attempt: {envelope}",
            target.display()
        );
        assert_ne!(
            envelope["exit_code"], 0,
            "{label}: the redirect into it must be denied by the kernel fence: {envelope}"
        );
    }
}

/// #2686 review round 2: files OUTSIDE the (now two) granted directories
/// must stay byte-identical across a confined-shell commit attempt — the
/// common dir's `config`, its top-level `HEAD` (the MAIN checkout's own
/// HEAD, never this worktree's), `packed-refs`, and ANOTHER linked
/// worktree's own admin `HEAD`. None of these were ever in
/// `own_gitdir_grants`'s write set; this pins that a compound redirect
/// cannot reach them either, so it can't regress silently later.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn confined_shell_commit_cannot_reach_config_packed_refs_common_head_or_a_foreign_worktree_admin_dir(
) {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if skip_without_real_kernel_fence() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    real_git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("seed"), "x").unwrap();
    real_git(&main, &["add", "seed"]);
    real_git(&main, &["commit", "-q", "-m", "init"]);
    real_git(&main, &["pack-refs", "--all"]);
    let wt = root.path().join("wt");
    real_git(
        &main,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
    );
    let wt2 = root.path().join("wt2");
    real_git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            wt2.to_str().unwrap(),
            "-b",
            "other-task",
        ],
    );
    std::fs::write(wt.join("f.txt"), "hi\n").unwrap();
    let own_git = crate::git_hardening::own_gitdir_grants(&wt);

    let common = main.join(".git");
    let mut read_roots = vec![wt.to_string_lossy().into_owned()];
    read_roots.extend(own_git.read);
    // `exec` includes `echo` for the same reason as the sibling test above.
    let session = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only(read_roots),
        fs_write: crate::caveats::Scope::only([wt.to_string_lossy().into_owned()]),
        exec: crate::caveats::Scope::only(["git".to_string(), "echo".to_string()]),
        net: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };

    for (label, target) in [
        ("the common dir's config", common.join("config")),
        ("packed-refs", common.join("packed-refs")),
        ("the common dir's own top-level HEAD", common.join("HEAD")),
        (
            "a foreign worktree's admin HEAD",
            common.join("worktrees/wt2/HEAD"),
        ),
    ] {
        assert!(
            target.exists(),
            "fixture sentinel {label} must exist: {}",
            target.display()
        );
        let before = std::fs::read(&target).unwrap();
        let cmd = format!("git add f.txt && echo payload >> {}", target.display());
        let widened =
            super::shell::dispatch_caveats_for_git_shell(&cmd, &wt.to_string_lossy(), &session);
        let envelope = super::shell::dispatch_bridled_shell(
            serde_json::json!({
                "cmd": cmd,
                "cwd": wt.to_string_lossy(),
                "env": hermetic_git_env(&home),
            }),
            &widened,
            None,
        )
        .await
        .expect("dispatch");
        let after = std::fs::read(&target).unwrap();
        assert_eq!(
            before,
            after,
            "{label} ({}) must be byte-identical after a confined-shell commit attempt: {envelope}",
            target.display()
        );
    }
}

/// Item 2(b): the widening is scoped to the ONE dispatch — the SESSION
/// `fs_write` scope itself still does not permit the common dir's
/// `objects/`, so a file tool (`write_file`/`delete_file`) stays refused.
#[test]
fn session_fs_write_still_excludes_the_common_objects_directory() {
    let root = tempfile::tempdir().unwrap();
    // Git resolves macOS's /var alias; compare the same physical paths.
    let root_path = root.path().canonicalize().unwrap();
    let main = root_path.join("main");
    std::fs::create_dir(&main).unwrap();
    real_git(&main, &["init", "-q"]);
    std::fs::write(main.join("seed"), "x").unwrap();
    real_git(&main, &["add", "seed"]);
    real_git(&main, &["commit", "-q", "-m", "init"]);
    let wt = root_path.join("wt");
    real_git(
        &main,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "task"],
    );
    // Round 4: the per-dispatch write grant is bound to the identity cached
    // at session start (`apply_cli_fs_grants` → `own_gitdir_grants`) — prime
    // it the same way production does, or the widening below finds no cached
    // identity and correctly grants nothing.
    crate::git_hardening::own_gitdir_grants(&wt);

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

/// A cached repository identity is not a write grant. In particular, a
/// read-only session or a grant for an unrelated path must not gain .git writes
/// merely by running a native Git command.
#[test]
fn git_shell_widening_requires_write_authority_over_the_workspace() {
    use crate::caveats::{Caveats, Scope};

    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    real_git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("seed"), "x").unwrap();
    real_git(&main, &["add", "seed"]);
    real_git(&main, &["commit", "-q", "-m", "init"]);
    let repo = root.path().join("repo");
    real_git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            repo.to_str().unwrap(),
            "-b",
            "task",
        ],
    );
    let own_git = crate::git_hardening::own_gitdir_grants(&repo);
    assert!(
        !own_git.write.is_empty(),
        "prime a real writable Git identity"
    );
    let workspace = repo.to_string_lossy();

    for write_scope in [
        Scope::none(),
        Scope::only([root.path().join("unrelated").to_string_lossy().into_owned()]),
        Scope::only([repo.join("seed").to_string_lossy().into_owned()]),
    ] {
        let session = Caveats {
            fs_read: Scope::only([workspace.to_string()]),
            fs_write: write_scope,
            ..Caveats::top()
        };
        let dispatched = super::shell::dispatch_caveats_for_git_shell(
            "git config --local test.key value",
            &workspace,
            &session,
        );
        assert_eq!(
            dispatched, session,
            "cached Git metadata must not add authority without a workspace write grant"
        );
    }

    let session = Caveats {
        fs_write: Scope::only([workspace.to_string()]),
        ..Caveats::top()
    };
    assert!(own_git
        .write
        .iter()
        .all(|path| !crate::caveats::permits_path(&session.fs_write, path)));
    let dispatched =
        super::shell::dispatch_caveats_for_git_shell("git add seed", &workspace, &session);
    for path in own_git.write {
        assert!(
            crate::caveats::permits_path(&dispatched.fs_write, &path),
            "a genuine workspace write grant retains access to its Git metadata: {path}"
        );
    }
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
    crate::git_hardening::own_gitdir_grants(root.path());

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

/// PR #2577 round 4, Blocker 1(a): rewriting the workspace's `.git` gitlink
/// after session start must NOT redirect the next `git` dispatch's write
/// grant onto a different repository — that repo's `hooks/`/`config` become
/// kernel-writable otherwise (hooks mean code execution the next time
/// anyone runs git there). Confirmed red by temporarily reverting
/// `own_gitdir_shell_write_grant` to call `own_gitdir_grants(workspace).write`
/// directly (round 3's behavior): it followed the rewritten gitlink and
/// returned the foreign repo's directories.
#[test]
fn rewriting_the_gitlink_after_session_start_grants_nothing() {
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
    // Session start: prime the cache to the REAL identity.
    crate::git_hardening::own_gitdir_grants(&wt);
    assert!(
        !crate::git_hardening::own_gitdir_shell_write_grant(&wt).is_empty(),
        "sanity: the real identity must grant write before any tampering"
    );

    // A completely separate repository, also on a non-default branch.
    let other = tempfile::tempdir().unwrap();
    real_git(other.path(), &["init", "-q", "-b", "task"]);
    std::fs::write(other.path().join("seed"), "y").unwrap();
    real_git(other.path(), &["add", "seed"]);
    real_git(other.path(), &["commit", "-q", "-m", "init"]);

    // The model rewrites the gitlink (an ordinary file `write_file` can
    // reach) to point at the OTHER repo's `.git`.
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", other.path().join(".git").display()),
    )
    .unwrap();

    assert!(
        crate::git_hardening::own_gitdir_shell_write_grant(&wt).is_empty(),
        "a rewritten gitlink must grant nothing, not the foreign repo's directories"
    );
}

/// PR #2577 round 4, Blocker 1(b): rewriting the worktree gitdir's
/// `commondir` file after session start must NOT redirect the next `git`
/// dispatch's write grant onto a different repository's `objects/`.
#[test]
fn rewriting_commondir_after_session_start_grants_nothing() {
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
    crate::git_hardening::own_gitdir_grants(&wt);
    assert!(!crate::git_hardening::own_gitdir_shell_write_grant(&wt).is_empty());

    let other = tempfile::tempdir().unwrap();
    real_git(other.path(), &["init", "-q", "-b", "task"]);
    std::fs::write(other.path().join("seed"), "y").unwrap();
    real_git(other.path(), &["add", "seed"]);
    real_git(other.path(), &["commit", "-q", "-m", "init"]);

    // `<worktree admin dir>/commondir` is INSIDE the granted write directory
    // itself — a compound `git status; echo … > …/commondir` (leading
    // program `git`) can reach it under the widened fence.
    let admin_dir = main.join(".git/worktrees/wt");
    std::fs::write(
        admin_dir.join("commondir"),
        format!("{}\n", other.path().join(".git").display()),
    )
    .unwrap();

    let write = crate::git_hardening::own_gitdir_shell_write_grant(&wt);
    assert!(
        write.is_empty(),
        "a rewritten commondir must grant nothing, not the other repo's objects/: {write:?}"
    );
}

/// Discovered from the actual CI run of this PR: a Landlock-confined `git
/// add` on a runner that ships `/etc/gitconfig` (this dev sandbox does not,
/// which is why the kernel-confined test above passed here but failed in
/// CI) fails with "unknown error occurred while reading the configuration
/// files", exit 128 — real git always tries to read system config,
/// regardless of repo/branch, and Landlock's base read allowlist does not
/// include it. Deterministic (no dependency on whether `/etc/gitconfig`
/// exists on the machine running this test): the widened `fs_read` must
/// include it.
#[test]
fn git_shell_widening_grants_read_on_etc_gitconfig() {
    let root = tempfile::tempdir().unwrap();
    real_git(root.path(), &["init", "-q", "-b", "task"]);
    std::fs::write(root.path().join("seed"), "x").unwrap();
    real_git(root.path(), &["add", "seed"]);
    real_git(root.path(), &["commit", "-q", "-m", "init"]);
    crate::git_hardening::own_gitdir_grants(root.path());

    let session = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([root.path().to_string_lossy().into_owned()]),
        fs_write: crate::caveats::Scope::only([root.path().to_string_lossy().into_owned()]),
        ..crate::caveats::Caveats::top()
    };
    let widened = super::shell::dispatch_caveats_for_git_shell(
        "git add f.txt",
        &root.path().to_string_lossy(),
        &session,
    );
    assert!(
        crate::caveats::permits_path(&widened.fs_read, "/etc/gitconfig"),
        "the widened dispatch must grant read on /etc/gitconfig"
    );
}

/// Supplies a REAL native commit policy, as production always does for a
/// commit-creating command (`run_command`'s arm refuses the command outright
/// when `git_tool.native_commit_policy()` is `None` — see
/// `run_command_creates_shell_git_commit(cmd) && commit_broker.is_none()`),
/// without exercising `NativeGitBroker`'s hook-protocol signing/attribution
/// machinery, which only round-trips correctly from a `maybe_dispatch`-aware
/// binary (`newt`/`brush_build_pipeline`'s `harness = false` tests) — never
/// from the ordinary `cargo test` harness this file runs under.
struct QuietCommitPolicy;
impl agent_toolchain::native_git::CommitPolicy for QuietCommitPolicy {
    fn finalize_message(&self, message: &str) -> Result<String, String> {
        Ok(message.to_string())
    }
    fn signing_required(&self) -> bool {
        false
    }
    fn sign_commit(&self, _payload: &[u8]) -> Result<String, String> {
        Err("signing not exercised by this fixture".into())
    }
    fn committed(&self) {}
}

pub(in crate::agentic::tools) struct FixtureGitTool;
impl crate::agentic::git_tool::GitTool for FixtureGitTool {
    fn native_commit_policy(
        &self,
    ) -> Option<std::sync::Arc<dyn agent_toolchain::native_git::CommitPolicy>> {
        Some(std::sync::Arc::new(QuietCommitPolicy))
    }
    fn dispatch(
        &self,
        _op: &str,
        _args: &serde_json::Value,
        _caveats: &agent_toolchain::git_caveats::GitCaveats,
        _session: &crate::caveats::Caveats,
    ) -> Result<String, String> {
        Err("the embedded git tool is not exercised by this fixture".into())
    }
}

/// #2686 review round 3, P2: through the COMPLETE `run_command` dispatch
/// (`execute_tool_with_collaborators`, the same entry point the TUI/headless
/// driver calls — not the lower-level `advance_own_branch_ref` directly), a
/// detached-HEAD commit that is NOT a descendant of `old_tip` (here, an
/// orphan commit the model's own compound command creates) must be refused
/// publication AND reported as a typed FAILURE — never `ExecOutcome::Passed`
/// with merely different text. Deterministic (no timing race): the orphan
/// shape is produced by the dispatched command itself, not by an external
/// mover, so this is reproducible every run rather than a timing-dependent
/// race.
#[tokio::test]
async fn an_orphan_detached_commit_through_the_full_dispatch_reports_a_typed_failure_not_passed() {
    let _env = super::disable_ocap_tests::env_lock().await;
    let _engine = super::disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if skip_without_real_kernel_fence() {
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

    let before = real_git_output(&main, &["rev-parse", "refs/heads/task"]);
    // An unrelated root commit (no parent), already in the shared object
    // store (same tree as `main`'s own init commit, just no parent) — so
    // the confined child can detach straight to it without a fence write on
    // any ref: `checkout --detach` only touches the worktree-local HEAD.
    let tree = real_git_output(&main, &["rev-parse", "HEAD^{tree}"]);
    let unrelated_oid = real_git_output(&main, &["commit-tree", "-m", "unrelated", &tree]);

    let git_tool = FixtureGitTool;
    let execution = std::sync::OnceLock::new();
    let out = super::execute_tool_with_collaborators(
        "run_command",
        &serde_json::json!({
            "command": format!(
                "git checkout -q --detach {unrelated_oid} && git commit -q --allow-empty -m forged"
            )
        }),
        &wt.to_string_lossy(),
        false,
        200,
        &session,
        &mut crate::agentic::NoMcp,
        super::ToolCollaborators {
            git_tool: Some(&git_tool),
            execution: Some(&execution),
            ..Default::default()
        },
        false,
        super::PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(
        execution.get().copied(),
        Some(crate::ExecOutcome::Failed),
        "a refused publication must be a typed failure, not Passed with different text: {out}"
    );
    assert_eq!(
        real_git_output(&main, &["rev-parse", "refs/heads/task"]),
        before,
        "a refused advance must not move refs/heads/task at all"
    );
    assert!(
        out.contains("refused publication"),
        "the text must explain the refusal: {out}"
    );
}

/// Like [`real_git`], but returns trimmed stdout.
fn real_git_output(dir: &std::path::Path, args: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let output = hermetic_git(dir, home.path()).args(args).output().unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
