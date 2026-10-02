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
fn hermetic_git_env(home: &std::path::Path) -> std::collections::BTreeMap<String, String> {
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
fn hermetic_git(dir: &std::path::Path, home: &std::path::Path) -> std::process::Command {
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
    if !crate::confined_exec::kernel_fs_fence_available()
        || !std::path::Path::new(FIXTURE_GIT).exists()
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
    if !crate::confined_exec::kernel_fs_fence_available()
        || !std::path::Path::new(FIXTURE_GIT).exists()
    {
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
    if !crate::confined_exec::kernel_fs_fence_available()
        || !std::path::Path::new(FIXTURE_GIT).exists()
    {
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
