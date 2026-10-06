//! #2780: task cwd routing through the actual dispatch boundary, portable to Windows.
use super::*;
use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
use crate::worktree_adoption::{AdoptedWorktree, WorktreeSession};

#[derive(Default)]
struct Notices(Vec<String>);
impl ToolPresentation for Notices {
    fn preview(&mut self, text: &str, _: usize) {
        self.0.push(text.into());
    }
    fn document(&mut self, _: &str) {}
    fn override_result(&mut self, _: String) {}
}

fn fixture() -> (tempfile::TempDir, AdoptedWorktree) {
    let (temp, policy, _) = crate::worktree_adoption::tests::fixture(false);
    crate::worktree_adoption::tests::link(&policy);
    // Git metadata uses repository-relative locators, not Rust's Windows
    // canonical/verbatim API spelling (which Git treats as non-local).
    std::fs::write(
        policy.worktree.join(".git"),
        "gitdir: ../main/.git/worktrees/task\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("main/.git/worktrees/task/gitdir"),
        "../../../../task/.git\n",
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join("main/.git/refs/heads")).unwrap();
    std::fs::write(
        temp.path().join("main/.git/worktrees/task/HEAD"),
        "ref: refs/heads/task\n",
    )
    .unwrap();
    (temp, policy)
}

async fn call(
    session: &WorktreeSession,
    root: &Path,
    name: &str,
    args: serde_json::Value,
    caveats: &Caveats,
) -> (String, Option<PathBuf>, Vec<String>) {
    let directory = std::sync::OnceLock::new();
    let mut notices = Notices::default();
    let result = execute(
        &mut notices,
        name,
        &args,
        root.to_str().unwrap(),
        false,
        40,
        caveats,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            worktree_session: Some(session),
            command_directory: Some(&directory),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    (result, directory.into_inner(), notices.0)
}

/// #2780: plain Git runs in the bound task checkout; explicit selection wins,
/// lift restores the original, and both operator/model see exactly one notice.
#[tokio::test]
async fn shell_task_cwd_2780_actual_git_and_lift() {
    let _lock = env_lock().await;
    let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual = EnvVar::unset("VIRTUAL_ENV");
    let _git_env: Vec<_> = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_CEILING_DIRECTORIES",
    ]
    .iter()
    .map(|name| EnvVar::unset(name))
    .collect();
    let _system = EnvVar::set("GIT_CONFIG_NOSYSTEM", "1");
    let (temp, policy) = fixture();
    let original = temp.path().join("main").canonicalize().unwrap();
    let empty = temp.path().join("empty-config");
    std::fs::write(&empty, "").unwrap();
    let _global = EnvVar::set("GIT_CONFIG_GLOBAL", empty.to_str().unwrap());
    let session = WorktreeSession::default();
    let args = || serde_json::json!({"command":"git branch --show-current"});
    let (out, cwd, notices) =
        call(&session, &original, "run_command", args(), &Caveats::top()).await;
    assert_eq!(out.trim(), "main", "{out}");
    assert_eq!(cwd, Some(original.clone()));
    assert!(notices.is_empty());
    session.record_task_worktree(&policy.worktree, "task");
    let hint = format!(
        "Commands now run in the task worktree {}",
        dunce::simplified(&policy.worktree).display()
    );
    // Explicit selection must neither emit nor consume the default notice.
    let (out, _, notices) = call(
        &session,
        &original,
        "run_command",
        serde_json::json!({"command":"git branch --show-current", "cwd":"."}),
        &Caveats::top(),
    )
    .await;
    assert_eq!(out.trim(), "main", "{out}");
    assert!(notices.is_empty());
    for first in [true, false] {
        let (out, cwd, notices) =
            call(&session, &original, "run_command", args(), &Caveats::top()).await;
        assert_eq!(out.lines().next(), Some("task"), "{out}");
        assert_eq!(
            cwd.as_ref().map(|path| path.canonicalize().unwrap()),
            Some(policy.worktree.clone())
        );
        assert_eq!(out.contains(&hint), first, "{out}");
        assert_eq!(notices.iter().any(|text| text.contains(&hint)), first);
    }
    let (out, cwd, notices) = call(&session, &original, "run_command",
        serde_json::json!({"command":"git branch --show-current", "cwd":policy.worktree.join("missing")}), &Caveats::top()).await;
    assert!(
        out.contains(&format!(
            "default working directory remains {}",
            policy.worktree.display()
        )),
        "{out}"
    );
    assert!(cwd.is_none() && notices.is_empty());
    for explicit in [
        serde_json::json!({"command":"git branch --show-current", "cwd":"."}),
        serde_json::json!({"command":format!("cd \"{}\" && git branch --show-current", original.display())}),
    ] {
        let (out, cwd, _) = call(
            &session,
            &original,
            "run_command",
            explicit,
            &Caveats::top(),
        )
        .await;
        assert_eq!(out.trim(), "main", "{out}");
        assert_eq!(cwd, Some(original.clone()));
    }
    session.lift();
    let (out, cwd, _) = call(&session, &original, "run_command", args(), &Caveats::top()).await;
    assert_eq!(out.trim(), "main", "{out}");
    assert_eq!(cwd, Some(original.clone()));
    session.record_task_worktree(&policy.worktree, "task");
    let (out, _, notices) = call(&session, &original, "run_command", args(), &Caveats::top()).await;
    assert!(out.contains(&hint) && notices.iter().any(|text| text.contains(&hint)));
    std::fs::remove_file(original.join(".git/worktrees/task/gitdir")).unwrap();
    let (out, cwd, _) = call(&session, &original, "run_command", args(), &Caveats::top()).await;
    assert_eq!(out.trim(), "main", "unbound task: {out}");
    assert_eq!(cwd, Some(original));
}

/// #2780: lifecycle detection shares the bound default and preserves explicit dir.
#[tokio::test]
async fn shell_task_cwd_2780_lifecycle_default() {
    let _lock = env_lock().await;
    let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let (temp, policy) = fixture();
    let original = temp.path().join("main").canonicalize().unwrap();
    std::fs::write(
        policy.worktree.join("Cargo.toml"),
        "[package]\nname='task'\nversion='0.1.0'\n",
    )
    .unwrap();
    let session = WorktreeSession::default();
    session.record_task_worktree(&policy.worktree, "task");
    let (out, _, _) = call(
        &session,
        &original,
        "lifecycle",
        serde_json::json!({"phase":"test","action":"list"}),
        &Caveats::top(),
    )
    .await;
    assert!(out.contains("cargo test"), "{out}");
    let (out, _, _) = call(
        &session,
        &original,
        "lifecycle",
        serde_json::json!({"phase":"test","action":"list","dir":"."}),
        &Caveats::top(),
    )
    .await;
    assert!(out.contains("no command configured"), "{out}");
}

/// #2780/#2784: builds use the task directory in both modes. Confined/direct
/// builds require build approval; an explicit exec floor keeps even an
/// OCAP-disabled shell from running. A recording denial prevents any child run.
#[tokio::test]
async fn shell_task_cwd_2780_build_routes_keep_authority() {
    #[derive(Default)]
    struct Gate(Vec<(Caveats, String)>);
    impl PermissionGate for Gate {
        fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
            PermissionDecision::Deny
        }
        fn ask_with_caveats(
            &mut self,
            baseline: &Caveats,
            requests: &[PermissionRequest],
        ) -> PermissionDecision {
            self.0.extend(
                requests
                    .iter()
                    .map(|request| (baseline.clone(), request.target.clone())),
            );
            PermissionDecision::Deny
        }
        fn ask_question(&mut self, _: &str) -> crate::agentic::permissions::HumanQuestionOutcome {
            crate::agentic::permissions::HumanQuestionOutcome::Unavailable
        }
    }
    let _lock = env_lock().await;
    for bypass in ["0", "1"] {
        let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", bypass);
        let (temp, policy) = fixture();
        let original = temp.path().join("main").canonicalize().unwrap();
        std::fs::write(
            policy.worktree.join("Cargo.toml"),
            "[package]\nname='task'\nversion='0.1.0'\n",
        )
        .unwrap();
        let session = WorktreeSession::default();
        session.record_task_worktree(&policy.worktree, "task");
        // Adoption's held-handle verifier is Unix-only; Windows still tests
        // confined routing from the same portable, reciprocally bound record.
        if bypass == "0" && cfg!(unix) {
            session.adopt(policy.clone());
        }
        let caveats = Caveats {
            exec: crate::Scope::none(),
            fs_write: crate::Scope::only([policy.worktree.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        for (name, args) in [
            ("run_command", serde_json::json!({"command":"cargo check"})),
            (
                "run_command",
                serde_json::json!({"command":"cargo check && echo done"}),
            ),
            (
                "build_exec",
                serde_json::json!({"program":"cargo","argv":["check"]}),
            ),
            (
                "lifecycle",
                serde_json::json!({"phase":"test","action":"build"}),
            ),
        ] {
            let mut gate = Gate::default();
            let execution = std::sync::OnceLock::new();
            let directory = std::sync::OnceLock::new();
            let result = execute(
                &mut Notices::default(),
                name,
                &args,
                original.to_str().unwrap(),
                false,
                40,
                &caveats,
                &mut crate::agentic::NoMcp,
                ToolCollaborators {
                    worktree_session: Some(&session),
                    command_directory: Some(&directory),
                    exec_floor: Some(&caveats.exec),
                    permission_gate: Some(&mut gate),
                    execution: Some(&execution),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
            )
            .await;
            assert_eq!(
                execution.get(),
                Some(&crate::ExecOutcome::Denied),
                "{bypass} {name}: {result}"
            );
            assert_eq!(gate.0.len(), 1, "{bypass} {name}: {result}");
            let (requested, target) = &gate.0[0];
            if bypass == "1" && name == "run_command" {
                // Ordinary shell admission asks for exec, not a build fence.
                assert_eq!(target, "cargo", "{result}");
                assert_eq!(
                    directory.get().and_then(|cwd| cwd.canonicalize().ok()),
                    Some(policy.worktree.clone()),
                    "host-mode commands retain the bound task cwd"
                );
            } else {
                assert_eq!(
                    Path::new(target),
                    policy.worktree,
                    "{bypass} {name}: {result}"
                );
            }
            assert!(!crate::permits_path(
                &requested.fs_write,
                original.to_str().unwrap()
            ));
        }
    }
}

/// #2780 round 2: all task cwd projections must use child-compatible spellings.
/// Windows canonicalization supplies the verbatim prefix; Unix is unchanged.
#[test]
fn shell_task_cwd_2780_projected_paths_are_child_compatible() {
    let (temp, policy) = fixture();
    let original = temp.path().join("main").canonicalize().unwrap();
    let session = WorktreeSession::default();
    session.record_task_worktree(&policy.worktree, "task");
    #[cfg(windows)]
    assert!(policy.worktree.to_str().unwrap().starts_with(r"\\?\"));
    for name in ["run_command", "bash", "build_exec", "lifecycle"] {
        let args = serde_json::json!({"command":"git branch --show-current"});
        let projected = shell::command_args_with_default_cwd(
            name,
            &args,
            original.to_str().unwrap(),
            None,
            Some(&session),
        )
        .unwrap();
        let key = shell::command_cwd_key(name).unwrap();
        let cwd = projected[key].as_str().unwrap();
        assert!(!cwd.starts_with(r"\\?\"), "{name}: {cwd}");
        assert_eq!(Path::new(cwd).canonicalize().unwrap(), policy.worktree);
    }
}
