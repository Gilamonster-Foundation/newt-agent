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
