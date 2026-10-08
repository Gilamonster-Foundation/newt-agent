//! #2801: live command shapes through the real tool dispatcher, with isolated Git.
//! Add one line to the `rows!` table to add a case. Each row is an ordinary
//! test on every OS unless explicitly cfg-gated. Outcomes are typed execution classes (the public
//! dispatch seam does not expose raw exit codes). Permission probes deliberately
//! deny before Cargo executes; the requested kind/root proves routing. Publication
//! uses a local bare remote or an inert child, never credentials or a forge.
//! Existing detailed regressions retain responsibility for broker internals.
//! Linux/Windows CI and the native macOS confinement job run these. Confined
//! discovery on Windows needs the AppContainer feature.
use super::*;
use crate::caveats::Caveats;
use crate::{worktree_adoption::WorktreeSession, ExecOutcome, Scope};
use disable_ocap_tests::{env_lock, EnvVar};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Confined,
    Plain,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Setup {
    Fresh,
    Bound,
    Restricted,
    Publish,
    // The native confined rows are Unix-only; shared fixture matching still
    // names these variants in the Windows build of the portable table.
    #[cfg_attr(not(unix), allow(dead_code))]
    Adopted,
    #[cfg_attr(not(unix), allow(dead_code))]
    AdoptedPublish,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Root {
    Original,
    Task,
    Neither,
}

#[derive(Default)]
struct Gate(Vec<PermissionRequest>);
impl PermissionGate for Gate {
    fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
        self.0.extend_from_slice(requests);
        PermissionDecision::Deny
    }
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        panic!("contract rows do not ask human questions")
    }
}

fn git(root: &Path, home: &Path, args: &[&str]) {
    let out = git_fixture::hermetic_git(root, home)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fixture {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

struct Row {
    mode: Mode,
    setup: Setup,
    command: &'static str,
    outcome: Option<ExecOutcome>,
    root: Root,
    bound: bool,
    refused: bool,
    contains: &'static str,
    gate: Option<DenialKind>,
    claim: &'static str,
}

fn discovery_names_fixture(out: &str, fixture: &str) -> bool {
    let normalize = |s: &str| {
        s.replace("\\\\", "\\")
            .replace('\\', "/")
            .to_ascii_lowercase()
    };
    // Shells may translate the temp root. Keep the unique directory and both
    // trailing components, with boundaries so another fixture cannot match.
    let fixture = normalize(fixture);
    let mut components: Vec<_> = fixture.rsplit('/').take(3).collect();
    components.reverse();
    let suffix = format!("/{}", components.join("/"));
    let suffix = suffix.strip_suffix(".exe").unwrap_or(&suffix);
    normalize(out)
        .split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`'))
        .any(|path| path.strip_suffix(".exe").unwrap_or(path).ends_with(suffix))
}

/// #2811: MSYS discovery rewrites the temp root and may omit .exe.
#[test]
fn discovery_matches_unique_fixture_across_windows_path_forms() {
    let fixture = r"C:\Temp\.tmpUnique123\bin\gh.exe";
    for found in [
        "/tmp/.tmpUnique123/bin/gh",
        "C:/Temp/.TMPUNIQUE123/bin/GH.EXE",
        r"C:\Temp\.tmpUnique123\bin\gh.exe",
        r"C:\\Temp\\.tmpUnique123\\bin\\gh.exe",
    ] {
        assert!(discovery_names_fixture(found, fixture), "{found}");
    }
    for found in [
        "/usr/bin/gh",
        "/tmp/.tmpOther/bin/gh",
        "/tmp/prefix.tmpUnique123/bin/gh",
        "/tmp/.tmpUnique123/bin/gh-evil",
        "/tmp/.tmpUnique123/bin/gh.exe.bak",
        "/tmp/.tmpUnique123/bin/gh/child",
    ] {
        assert!(!discovery_names_fixture(found, fixture), "{found}");
    }
}

async fn check(row: Row) {
    let _lock = env_lock().await;
    let temp = tempfile::tempdir().unwrap();
    // macOS exposes /var through /private/var; grants must use the same
    // canonical spelling as destination admission.
    let temp_root = dunce::canonicalize(temp.path()).unwrap();
    let root = temp_root.as_path().join("original");
    let task = temp_root.as_path().join("task");
    std::fs::create_dir(&root).unwrap();
    git(&root, temp_root.as_path(), &["init", "-q", "-b", "main"]);
    git(
        &root,
        temp_root.as_path(),
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "seed",
        ],
    );
    std::fs::write(root.join("marker"), "original marker").unwrap();
    let session = WorktreeSession::default();
    if matches!(row.setup, Setup::Bound | Setup::Restricted | Setup::Publish) {
        git(
            &root,
            temp_root.as_path(),
            &["worktree", "add", "-b", "task", "../task"],
        );
        std::fs::write(task.join("marker"), "task marker").unwrap();
        std::fs::write(
            task.join("Cargo.toml"),
            "[package]\nname='contract'\nversion='0.1.0'\n",
        )
        .unwrap();
        session.record_task_worktree(&task.canonicalize().unwrap(), "task");
    }
    if matches!(row.setup, Setup::Publish | Setup::AdoptedPublish) {
        let remote = temp_root.as_path().join("remote.git");
        git(
            &root,
            temp_root.as_path(),
            &["init", "--bare", "-q", remote.to_str().unwrap()],
        );
        git(
            &root,
            temp_root.as_path(),
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(
            &root,
            temp_root.as_path(),
            &["config", "push.default", "current"],
        );
    }
    let _redirects: Vec<_> = [
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
    ]
    .into_iter()
    .map(EnvVar::unset)
    .collect();
    let git_env = git_fixture::hermetic_git_env(temp_root.as_path());
    let _env = [
        "HOME",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_GLOBAL",
        "GIT_TEMPLATE_DIR",
    ]
    .map(|key| EnvVar::set(key, &git_env[key]));
    // Discovery is about lookup, not host gh installation or credentials.
    let bin = temp_root.as_path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    // The test executable rejects gh's --base argument locally. It can never
    // create a PR; its error proves the plain route really spawned the child.
    let gh = bin.join(if cfg!(windows) { "gh.exe" } else { "gh" });
    let executable = std::env::current_exe().unwrap();
    if std::fs::hard_link(&executable, &gh).is_err() {
        std::fs::copy(&executable, &gh).unwrap();
    }
    let mut paths = vec![bin.clone()];
    // Keep dispatch on the same system Git used by hermetic setup, not a
    // runner's group-writable Homebrew prefix. The inert gh fixture stays first.
    #[cfg(unix)]
    paths.push(PathBuf::from("/usr/bin"));
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let paths = std::env::join_paths(paths).unwrap();
    let _paths = EnvVar::set("NEWT_EXEC_PATHS", paths.to_str().unwrap());
    let _ambient_path = EnvVar::set("PATH", paths.to_str().unwrap());
    #[cfg(windows)]
    let _pathext = EnvVar::set("PATHEXT", ".COM;.EXE;.BAT;.CMD");
    // #2811: Windows ambient execution consumes PATH, not NEWT_EXEC_PATHS.
    // Assert before any PR command so a broken fixture never reaches a forge.
    assert_eq!(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).next(),
        Some(bin.clone()),
        "the inert gh fixture must win ambient PATH lookup"
    );
    let _bypass = EnvVar::set(
        "NEWT_DISABLE_OCAP",
        if row.mode == Mode::Plain { "1" } else { "0" },
    );
    let _full = EnvVar::set(
        "NEWT_FULL_ACCESS",
        if row.mode == Mode::Plain { "1" } else { "0" },
    );
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    let _routing = EnvVar::set("NEWT_NO_ROUTE", "0");
    let mut caveats = Caveats {
        net: Scope::none(),
        fs_write: Scope::only([if matches!(
            row.setup,
            Setup::Fresh | Setup::Adopted | Setup::AdoptedPublish
        ) {
            temp_root.as_path()
        } else {
            &task
        }
        .to_string_lossy()
        .into_owned()]),
        ..Caveats::top()
    };
    if row.setup == Setup::Restricted {
        caveats.exec = Scope::none();
    }
    #[cfg(unix)]
    if matches!(row.setup, Setup::Adopted | Setup::AdoptedPublish) {
        confined_setup(
            "git worktree add -b task ../task",
            &root,
            &caveats,
            &session,
        )
        .await;
        assert!(session.snapshot().is_some(), "creation must arm adoption");
        caveats.fs_write = Scope::only([task.to_string_lossy().into_owned()]);
        if row.setup == Setup::AdoptedPublish {
            caveats.fs_write = Scope::only(
                [task.clone(), temp_root.as_path().join("remote.git")]
                    .map(|p| p.to_string_lossy().into_owned()),
            );
        }
        std::fs::write(task.join("marker"), "task commit\n").unwrap();
        confined_setup("git add marker", &root, &caveats, &session).await;
        if row.command != "git commit -m contract" {
            confined_setup("git commit -m contract", &root, &caveats, &session).await;
        }
    }
    let baseline = crate::agentic::claim_check::TurnClaims::capture(
        root.to_str().unwrap(),
        &Scope::All,
        Some(&session),
    );
    if row.mode == Mode::Plain && row.command.starts_with("gh pr ") {
        // A harmless probe must identify libtest before any publication-shaped
        // command runs. Even a broken lookup can only reach host gh --help.
        let route = shell::select_shell_route(
            true,
            false,
            false,
            true,
            cfg!(windows),
            crate::config::windows_cmd_enabled(),
            crate::ambient_brush::installed(),
            crate::ShellEngine::Brush,
        );
        let probe = shell::host_shell_dispatch(
            route,
            "gh --help",
            task.to_str().unwrap(),
            None,
            None,
            Default::default(),
        )
        .await
        .unwrap();
        assert!(
            probe["exit_code"] == 0
                && probe["stdout"]
                    .as_str()
                    .is_some_and(|text| text.contains("--test-threads")),
            "plain gh must resolve to the inert libtest child before PR execution"
        );
    }
    let (name, args) = if row.command == "read_file marker" {
        ("read_file", serde_json::json!({"path":"marker"}))
    } else {
        ("run_command", serde_json::json!({"command":row.command}))
    };
    let execution = OnceLock::new();
    let directory = OnceLock::new();
    let receipt = OnceLock::new();
    let mut gate = Gate::default();
    #[cfg(unix)]
    let fixture_git = crate::agentic::tools::tests::git_shell_grant::FixtureGitTool;
    let out = if row.command == "confined-executor git push" {
        use crate::confined_exec::{ConstrainedExecutor, ExecOrigin, ExecRequest};
        let policy = session.snapshot().expect("adopted task");
        let authority = policy.attenuate(&policy.task_authority(&caveats));
        let program = crate::git_hardening::trusted_git_program(Some(paths.as_os_str())).unwrap();
        let request = ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            program.to_string_lossy(),
            ["push", "origin", "task:task"],
            &task,
            authority,
        )
        .envs(git_env.clone())
        .env("PATH", paths.to_string_lossy())
        .timeout(std::time::Duration::from_secs(30));
        let output = ConstrainedExecutor::run_async(request)
            .await
            .expect("confined local push admission");
        assert!(
            output.success,
            "confined local push: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            output.sandbox_kind,
            if cfg!(target_os = "macos") {
                agent_bridle::SandboxKind::Seatbelt
            } else {
                agent_bridle::SandboxKind::Landlock
            },
            "local push must use the native kernel fence"
        );
        let expected = std::fs::read_to_string(root.join(".git/refs/heads/task")).unwrap();
        assert_eq!(
            std::fs::read_to_string(temp_root.join("remote.git/refs/heads/task")).unwrap(),
            expected,
            "local push must publish the task commit before checking tracking metadata"
        );
        let tracking = root.join(".git/refs/remotes/origin/task");
        assert_eq!(
            std::fs::read_to_string(&tracking).ok().as_deref(),
            Some(expected.as_str()),
            "local push must update shared tracking ref: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        execution.set(ExecOutcome::Passed).unwrap();
        directory.set(task.clone()).unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        execute_tool_with_display_cancellable(
            &mut ToolDisplay::new(Vec::new(), false, 100, 40, false),
            name,
            &args,
            root.to_str().unwrap(),
            false,
            40,
            &caveats,
            &mut crate::agentic::NoMcp,
            ToolCollaborators {
                worktree_session: Some(&session),
                #[cfg(unix)]
                git_tool: matches!(row.setup, Setup::Adopted | Setup::AdoptedPublish)
                    .then_some(&fixture_git as &dyn crate::agentic::git_tool::GitTool),
                execution: Some(&execution),
                command_directory: Some(&directory),
                governed_pr: Some(&receipt),
                permission_gate: Some(&mut gate),
                exec_floor: (row.setup == Setup::Restricted).then_some(&caveats.exec),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap()
    };
    #[cfg(windows)]
    if let Some(cwd) = directory.get() {
        assert!(
            !cwd.to_string_lossy().starts_with(r"\\?\"),
            "child cwd must not be verbatim: {cwd:?}"
        );
    }
    assert_eq!(
        execution.get().copied(),
        row.outcome,
        "{}: {out}",
        row.command
    );
    let expected_root = match row.root {
        Root::Original => Some(&root),
        Root::Task => Some(&task),
        Root::Neither => None,
    };
    assert_eq!(
        directory.get().map(|p: &PathBuf| p.canonicalize().unwrap()),
        expected_root.map(|p| p.canonicalize().unwrap()),
        "{}: {out}",
        row.command
    );
    assert_eq!(
        session.task_root(&root),
        row.bound
            .then(|| if row.command.contains(".worktrees/task") {
                root.join(".worktrees/task")
            } else {
                task.clone()
            }
            .canonicalize()
            .unwrap()),
        "{}: {out}",
        row.command
    );
    // #2810: recording a task path alone is not confined adoption.
    if row.mode == Mode::Confined && row.setup == Setup::Fresh && row.bound {
        assert!(session.snapshot().is_some(), "{}: {out}", row.command);
    }
    if let Some(kind) = row.gate {
        assert!(
            gate.0.iter().any(|r| r.kind == kind),
            "{}: {:?}: {out}",
            row.command,
            gate.0
        );
        if kind == DenialKind::Build {
            assert_eq!(
                Path::new(&gate.0[0].target).canonicalize().unwrap(),
                task.canonicalize().unwrap()
            );
        }
    }
    if matches!(row.setup, Setup::Publish | Setup::AdoptedPublish) {
        let expected =
            crate::agentic::claim_check::git_head(task.to_str().unwrap(), &Scope::All).unwrap();
        let remote = temp_root.as_path().join("remote.git/refs/heads/task");
        assert_eq!(std::fs::read_to_string(remote).unwrap().trim(), expected);
    }
    if row.command == "git commit -m contract" {
        assert_ne!(
            std::fs::read(root.join(".git/refs/heads/task")).unwrap(),
            std::fs::read(root.join(".git/refs/heads/main")).unwrap(),
            "confined commit must advance the task branch"
        );
    }
    if row.command == "git branch followup" {
        assert_eq!(
            std::fs::read(root.join(".git/refs/heads/followup")).unwrap(),
            std::fs::read(root.join(".git/refs/heads/task")).unwrap()
        );
    }
    if row.command == "which gh" {
        // Both plain and confined discovery must name this fixture, not host gh.
        assert!(
            discovery_names_fixture(&out, &gh.to_string_lossy()),
            "gh discovery must identify the inert fixture: {out}"
        );
    }
    if row.claim.is_empty() {
        assert!(out.contains(row.contains), "{}: {out}", row.command);
    } else {
        let mut ledger = crate::agentic::self_verify::VerificationLedger::default();
        ledger
            .observe(
                name,
                &args,
                tool_result_ok(&out),
                execution.get().copied(),
                root.to_str().unwrap(),
            )
            .await;
        if let Some(outcome) = receipt.get() {
            ledger.record_push_outcome(outcome);
        }
        let final_text = crate::agentic::finalize_final_text(
            row.claim.into(),
            root.to_str().unwrap(),
            &Scope::All,
            &crate::agentic::capability_check::Evidence::default(),
            None,
            &baseline,
            &ledger,
        );
        assert!(final_text.contains(row.contains), "{final_text}");
        if row.setup == Setup::Publish {
            assert_eq!(final_text, row.claim);
        }
    }
    let refused = out.contains("capability denied")
        || out.contains("refused:")
        || out.contains("failed(refused_by_harness)");
    assert_eq!(refused, row.refused, "{}: {out}", row.command);
}

/// Real confined creation and commit setup, shared by the #2813 contract rows.
#[cfg(unix)]
async fn confined_setup(command: &str, root: &Path, caveats: &Caveats, session: &WorktreeSession) {
    let execution = OnceLock::new();
    let text = worktree::execute(
        &mut ToolDisplay::new(Vec::new(), false, 100, 40, false),
        "run_command",
        &serde_json::json!({"command":command}),
        root.to_str().unwrap(),
        false,
        40,
        caveats,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(session),
            git_tool: Some(&crate::agentic::tools::tests::git_shell_grant::FixtureGitTool),
            execution: Some(&execution),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    assert_eq!(
        execution.get(),
        Some(&ExecOutcome::Passed),
        "{command}: {text}"
    );
}

macro_rules! rows {
    ($( $(#[$attr:meta])* $id:ident, $mode:ident, $setup:ident, $command:literal, $outcome:expr, $root:ident, $bound:literal, $refused:literal, $contains:literal, $gate:expr, $claim:literal; )*) => {$(
        $(#[$attr])*
        #[tokio::test]
        async fn $id() {
            check(Row { mode: Mode::$mode, setup: Setup::$setup, command: $command, outcome: $outcome, root: Root::$root, bound: $bound, refused: $refused, contains: $contains, gate: $gate, claim: $claim }).await;
        }
    )*};
}
use ExecOutcome::{Denied, Failed, Passed, Unavailable};
#[rustfmt::skip]
rows! {
// name, mode, setup, command, outcome, cwd, bound, refusal, text, gate, claim
// #2813: actual confined creation precedes commit/ref writes. No forge or network.
#[cfg(unix)]
// #905: native creation is refused before the requested task operation.
#[cfg_attr(target_os = "macos", should_panic(expected = "refusing to spawn: backend authority on the Net axis is not decidable against the delegated grant ∪ declared runtime closure (L3 BOUND)"))]
confined_task_commit, Confined, Adopted, "git commit -m contract", Some(Passed), Task, true, false, "", None, "";
#[cfg(unix)]
// #2813 also reproduces under Landlock: task-scoped authority cannot lock shared refs.
#[cfg_attr(target_os = "linux", should_panic(expected = "original/.git/refs/heads/followup.lock': Permission denied"))]
// #905: native creation is refused before the requested task operation.
#[cfg_attr(target_os = "macos", should_panic(expected = "refusing to spawn: backend authority on the Net axis is not decidable against the delegated grant ∪ declared runtime closure (L3 BOUND)"))]
confined_task_ref_update, Confined, Adopted, "git branch followup", Some(Passed), Task, true, false, "", None, "";
// Owned local remote via the kernel-confined executor; the governed broker's
// local-URL refusal is a separate control in the macOS job (#905, #2813).
#[cfg(unix)]
// #2813: successful transport must not hide a failed shared tracking-ref update.
#[cfg_attr(target_os = "linux", should_panic(expected = "error: update_ref failed for ref 'refs/remotes/origin/task': cannot lock ref 'refs/remotes/origin/task': unable to create directory for"))]
// #905: native creation is refused before the requested task operation.
#[cfg_attr(target_os = "macos", should_panic(expected = "refusing to spawn: backend authority on the Net axis is not decidable against the delegated grant ∪ declared runtime closure (L3 BOUND)"))]
confined_local_push, Confined, AdoptedPublish, "confined-executor git push", Some(Passed), Task, true, false, "", None, "";
// TODO row (c), PR #2814: blank plan approval -> Approved once it lands.
// The macOS job also selects plan_mode::tests to inherit that regression.
// Worktree creation exercises automatic binding, not a pre-seeded session (#2791).
standalone, Plain, Fresh, "git worktree add -b task ../task", Some(Passed), Original, true, false, "", None, "";
compound, Plain, Fresh, "git worktree add -b task ../task 2>&1 | tail -5 && git status --short", Some(Passed), Original, true, false, "", None, "";
missing_b, Plain, Fresh, "git worktree add ../task absent", Some(Failed), Original, false, false, "-b", None, "";
#[cfg(unix)]
// #905: native creation is refused before the requested task operation.
#[cfg_attr(target_os = "macos", should_panic(expected = "refusing to spawn: backend authority on the Net axis is not decidable against the delegated grant ∪ declared runtime closure (L3 BOUND)"))]
confined_creation, Confined, Fresh, "git worktree add -b task ../task", Some(Passed), Original, true, false, "", None, "";
default_cwd, Plain, Bound, "git branch --show-current", Some(Passed), Task, true, false, "task", None, "";
relative_read, Plain, Bound, "read_file marker", None, Neither, true, false, "task marker", None, "";
confined_read, Confined, Bound, "read_file marker", None, Neither, true, false, "task marker", None, "";
#[cfg(any(unix, feature = "windows-appcontainer"))]
// #905: measured native Seatbelt refusal, not an ignored row.
#[cfg_attr(target_os = "macos", should_panic(expected = "refusing to spawn: backend authority on the Net axis is not decidable against the delegated grant ∪ declared runtime closure (L3 BOUND)"))]
which_git, Confined, Fresh, "which git", Some(Passed), Original, false, false, "git", None, "";
#[cfg(any(unix, feature = "windows-appcontainer"))]
// #905: measured native Seatbelt refusal, not an ignored row.
#[cfg_attr(target_os = "macos", should_panic(expected = "refusing to spawn: backend authority on the Net axis is not decidable against the delegated grant ∪ declared runtime closure (L3 BOUND)"))]
which_gh, Confined, Fresh, "which gh", Some(Passed), Original, false, false, "gh", None, "";
plain_which_git, Plain, Fresh, "which git", Some(Passed), Original, false, false, "git", None, "";
plain_which_gh, Plain, Fresh, "which gh", Some(Passed), Original, false, false, "gh", None, "";
confined_build, Confined, Restricted, "cd ../task && cargo check 2>&1 | tail -5", Some(Denied), Task, true, true, "capability denied", Some(DenialKind::Build), "";
plain_build, Plain, Restricted, "cd ../task && cargo check 2>&1 | tail -5", Some(Denied), Task, true, true, "", Some(DenialKind::Exec), "";
#[cfg(unix)]
broker_push, Confined, Restricted, "git push origin", None, Task, true, true, "failed(refused_by_harness)", None, "";
plain_push, Plain, Publish, "git push origin", Some(Passed), Task, true, false, "task", None, "";
#[cfg(unix)]
broker_pr, Confined, Restricted, "gh pr create --base main --head task --title contract --body contract", None, Task, true, true, "failed(refused_by_harness)", None, "";
plain_pr, Plain, Bound, "gh pr create --base main --head task --title contract --body contract", Some(Failed), Task, true, false, "Unrecognized option", None, "";
missing_cwd, Plain, Bound, "cd missing && git status", Some(Unavailable), Neither, true, false, "does not exist", None, "";
#[cfg(unix)]
nested, Confined, Fresh, "git worktree add -b task .worktrees/task", Some(Denied), Neither, false, true, "sibling", None, "";
#[cfg(windows)]
verbatim_path, Plain, Bound, "git rev-parse --show-toplevel", Some(Passed), Task, true, false, "task", None, "";
push_claim, Plain, Bound, "git push origin", Some(Failed), Task, true, false, "#2769", None, "Pushed: origin/task is live.";
commit_claim, Plain, Bound, "git status --short", Some(Passed), Task, true, false, "HEAD did not move", None, "I committed the change locally.";
true_push_claim, Plain, Publish, "git push origin task", Some(Passed), Task, true, false, "Pushed: origin/task is live.", None, "Pushed: origin/task is live.";
true_commit_claim, Plain, Bound, "git -c commit.gpgsign=false commit --allow-empty -m contract", Some(Passed), Task, true, false, "HEAD moved", None, "I committed the change locally.";
#[cfg(unix)]
// #2810: verified read-only output wrappers retain confined adoption.
// #905: native creation is refused before the requested task operation.
#[cfg_attr(target_os = "macos", should_panic(expected = "refusing to spawn: backend authority on the Net axis is not decidable against the delegated grant ∪ declared runtime closure (L3 BOUND)"))]
confined_compound, Confined, Fresh, "git worktree add -b task ../task 2>&1 | tail -5 && git status --short", Some(Passed), Original, true, false, "?? marker", None, "";
#[cfg(unix)]
// #905: native creation is refused before the requested task operation.
#[cfg_attr(target_os = "macos", should_panic(expected = "refusing to spawn: backend authority on the Net axis is not decidable against the delegated grant ∪ declared runtime closure (L3 BOUND)"))]
confined_missing_b, Confined, Fresh, "git worktree add ../task absent", Some(Failed), Original, false, false, "-b", None, "";
nested_plain, Plain, Fresh, "git worktree add -b task .worktrees/task", Some(Passed), Original, true, false, "", None, "";
#[cfg(windows)]
broker_push, Confined, Restricted, "git push origin", None, Task, true, true, "not available under Windows", None, "";
#[cfg(windows)]
broker_pr, Confined, Restricted, "gh pr create --base main --head task --title contract --body contract", None, Task, true, true, "not available under Windows", None, "";

}
