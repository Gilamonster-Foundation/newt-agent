//! #2723: both build routes must admit an already granted sibling root.
//! Real temporary directories ground canonical/symlink containment; a recording
//! gate stops before external execution, inspecting the actual build request.
use super::*;
use crate::caveats::{Caveats, Scope};
use std::path::Path;

#[derive(Default)]
struct BuildGate(Vec<(Caveats, String)>);
impl PermissionGate for BuildGate {
    fn ask(&mut self, _: &[PermissionRequest]) -> PermissionDecision {
        panic!("build admission must retain its requested caveats")
    }
    fn ask_with_caveats(
        &mut self,
        baseline: &Caveats,
        requests: &[PermissionRequest],
    ) -> PermissionDecision {
        assert_eq!(requests.len(), 1);
        self.0.push((baseline.clone(), requests[0].target.clone()));
        PermissionDecision::Deny // deterministically stop before spawning
    }
    fn ask_question(&mut self, _: &str) -> super::super::permissions::HumanQuestionOutcome {
        panic!("no question expected")
    }
}

async fn build(
    shell_route: bool,
    workspace: &Path,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut BuildGate,
) -> (String, crate::ExecOutcome) {
    let mut display = super::super::display::ToolDisplay::new(Vec::new(), false, 80, 20, false);
    let mut permission: Option<&mut dyn PermissionGate> = Some(gate);
    if shell_route {
        build_shell::execute(
            "cargo check; echo done",
            "cargo",
            cwd.to_str().unwrap(),
            workspace.to_str().unwrap(),
            caveats,
            &[],
            &mut permission,
            None,
            20,
            false,
            false,
            None,
            None,
            &mut display,
            None,
            None,
            Default::default(),
        )
        .await
    } else {
        run_confined_build_lane(
            workspace.to_str().unwrap(),
            cwd,
            "cargo",
            vec!["check".into()],
            "cargo check",
            None,
            caveats,
            &mut permission,
            20,
            false,
            false,
            None,
            None,
            None,
            shell::LIFECYCLE_BUILD_TIMEOUT,
            &mut display,
        )
        .await
    }
}

fn grants(root: &Path) -> Caveats {
    let mut caveats = Caveats::top();
    caveats.fs_write = Scope::only([root.to_string_lossy().into_owned()]);
    caveats.exec = Scope::none(); // force the existing Build gate
    caveats
}

/// #2723: a sibling's grant must reach Build admission with the sibling fence,
/// including when cwd is a nested crate; the launch checkout must not be writable.
async fn approved_sibling_build(shell_route: bool) {
    let temp = tempfile::tempdir().unwrap();
    let launch = temp.path().join("main");
    let sibling = temp.path().join("sibling");
    let cwd = sibling.join("crate");
    std::fs::create_dir(&launch).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let sibling = sibling.canonicalize().unwrap();
    let launch = launch.canonicalize().unwrap();
    let mut gate = BuildGate::default();
    let (text, outcome) = build(shell_route, &launch, &cwd, &grants(&sibling), &mut gate).await;
    assert_eq!(gate.0.len(), 1, "route={shell_route}: {text}");
    let (requested, target) = &gate.0[0];
    assert_eq!(target, sibling.to_str().unwrap());
    assert!(tui_permits_path(
        &requested.fs_write,
        sibling.to_str().unwrap()
    ));
    assert!(!tui_permits_path(
        &requested.fs_write,
        launch.to_str().unwrap()
    ));
    assert_eq!(
        outcome,
        crate::ExecOutcome::Denied,
        "the build gate still binds"
    );
}

/// #2723: argv builds must admit the granted sibling, preserving Build approval.
#[tokio::test]
async fn approved_sibling_argv_build_uses_its_own_fence() {
    approved_sibling_build(false).await;
}

/// #2723: build-bearing shell commands must use the same granted sibling fence.
#[tokio::test]
async fn approved_sibling_shell_build_uses_its_own_fence() {
    approved_sibling_build(true).await;
}

/// #2723: missing sibling authority must not be promoted to a Build approval.
#[tokio::test]
async fn unapproved_sibling_build_is_refused_with_grant_guidance() {
    let temp = tempfile::tempdir().unwrap();
    let launch = temp.path().join("main");
    let sibling = temp.path().join("sibling");
    std::fs::create_dir(&launch).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    for shell_route in [false, true] {
        let mut gate = BuildGate::default();
        let (text, outcome) =
            build(shell_route, &launch, &sibling, &grants(&launch), &mut gate).await;
        assert_eq!(outcome, crate::ExecOutcome::Denied);
        assert!(gate.0.is_empty());
        assert!(text.contains("--write"), "route={shell_route}: {text}");
    }
}

/// #2723: canonical cwd escaping a granted sibling via a symlink is denied
/// before Build admission, just as an escape from the launch root was denied.
#[cfg(unix)]
#[tokio::test]
async fn sibling_build_symlink_escape_is_refused_by_both_routes() {
    let temp = tempfile::tempdir().unwrap();
    let launch = temp.path().join("main");
    let sibling = temp.path().join("sibling");
    let outside = temp.path().join("outside");
    for dir in [&launch, &sibling, &outside] {
        std::fs::create_dir(dir).unwrap();
    }
    let link = sibling.join("escape");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    for shell_route in [false, true] {
        let mut gate = BuildGate::default();
        let (_, outcome) = build(shell_route, &launch, &link, &grants(&sibling), &mut gate).await;
        assert_eq!(outcome, crate::ExecOutcome::Denied);
        assert!(gate.0.is_empty());
    }
}

/// #2723: full filesystem authority also admits a sibling, but the build
/// request itself remains calibrated to that cwd, not the filesystem root.
#[tokio::test]
async fn full_access_sibling_build_keeps_a_narrow_fence() {
    let launch = tempfile::tempdir().unwrap();
    let sibling = tempfile::tempdir().unwrap();
    let cwd = sibling.path().canonicalize().unwrap();
    let mut caveats = Caveats::top();
    caveats.exec = Scope::none();
    for shell_route in [false, true] {
        let mut gate = BuildGate::default();
        let (text, _) = build(shell_route, launch.path(), &cwd, &caveats, &mut gate).await;
        assert_eq!(gate.0.len(), 1, "{text}");
        let (requested, target) = &gate.0[0];
        assert_eq!(target, cwd.to_str().unwrap());
        assert!(!tui_permits_path(
            &requested.fs_write,
            launch.path().to_str().unwrap()
        ));
    }
}

/// #2723: a relative grant must not acquire a different meaning from the
/// harness process cwd when choosing an absolute build fence.
#[tokio::test]
async fn relative_grant_does_not_authorize_a_sibling_build() {
    let launch = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    for shell_route in [false, true] {
        let mut gate = BuildGate::default();
        let (_, outcome) = build(
            shell_route,
            launch.path(),
            &cwd,
            &grants(Path::new(".")),
            &mut gate,
        )
        .await;
        assert_eq!(outcome, crate::ExecOutcome::Denied);
        assert!(gate.0.is_empty());
    }
}

/// #2729: host-scoped session authority must narrow before Build admission on
/// both routes, without modifying the session or bypassing a denied Build gate.
async fn host_scoped_build_child_narrows_network(shell_route: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let mut session = grants(&root);
    session.net = Scope::only(["example.com:443".to_owned()]);
    let original = session.clone();
    let mut gate = BuildGate::default();
    let (text, outcome) = build(shell_route, &root, &root, &session, &mut gate).await;
    assert_eq!(gate.0.len(), 1, "route={shell_route}: {text}");
    assert_eq!(gate.0[0].0.net, Scope::none(), "route={shell_route}");
    assert_eq!(outcome, crate::ExecOutcome::Denied);
    assert_eq!(session, original);
}

/// #2729: argv builds narrow host authority before admission.
#[tokio::test]
async fn host_scoped_build_children_narrow_network_argv() {
    host_scoped_build_child_narrows_network(false).await;
}

/// #2729: build-bearing shell commands use the same network narrowing.
#[tokio::test]
async fn host_scoped_build_children_narrow_network_shell() {
    host_scoped_build_child_narrows_network(true).await;
}

/// #2773: a missing cwd is an execution precondition failure, not missing
/// authority. Real directories ground both build routes without spawning Cargo.
#[tokio::test]
async fn missing_cwd_2773_build_routes() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    for shell_route in [false, true] {
        let mut gate = BuildGate::default();
        let (text, outcome) = build(
            shell_route,
            root.path(),
            &missing,
            &grants(root.path()),
            &mut gate,
        )
        .await;
        assert_eq!(outcome, crate::ExecOutcome::Unavailable, "{text}");
        assert!(text.contains("does not exist; no command ran"), "{text}");
        assert!(text.contains(&missing.display().to_string()), "{text}");
        assert!(!text.contains("capability denied"), "{text}");
        assert!(gate.0.is_empty());
    }
}

/// #2773: a failed cd must never become an observed command directory or
/// change the next call's default. Real host effects ground the dispatch seam
/// on Linux, macOS and Windows; no external executable beyond the host shell.
#[tokio::test]
async fn missing_cwd_2773_dispatch_keeps_previous_directory() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _ocap = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let previous = root.join("previous");
    std::fs::create_dir(&previous).unwrap();
    for args in [
        serde_json::json!({"command":"cd missing && cargo --version"}),
        serde_json::json!({"command":"echo should-not-run > marker", "cwd":previous.join("missing")}),
        serde_json::json!({"command":"echo recovered > marker"}),
    ] {
        let directory = std::sync::OnceLock::new();
        let execution = std::sync::OnceLock::new();
        let result = execute_tool_with_collaborators(
            "run_command",
            &args,
            root.to_str().unwrap(),
            false,
            100,
            &Caveats::top(),
            &mut crate::agentic::NoMcp,
            ToolCollaborators {
                default_command_cwd: Some(&previous),
                command_directory: Some(&directory),
                execution: Some(&execution),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        if args["command"] == "echo recovered > marker" {
            assert_eq!(
                execution.get(),
                Some(&crate::ExecOutcome::Passed),
                "{result}"
            );
            assert_eq!(
                std::fs::read_to_string(previous.join("marker"))
                    .unwrap()
                    .trim(),
                "recovered"
            );
            assert!(!root.join("marker").exists());
        } else {
            assert!(
                result.contains("does not exist; no command ran"),
                "{result}"
            );
            assert!(result.contains("working directory remains"), "{result}");
            assert!(result.contains(&previous.display().to_string()), "{result}");
            assert_eq!(execution.get(), Some(&crate::ExecOutcome::Unavailable));
            assert!(
                directory.get().is_none(),
                "a nonexistent cwd is not an observation"
            );
            assert!(!previous.join("marker").exists());
        }
    }
}
