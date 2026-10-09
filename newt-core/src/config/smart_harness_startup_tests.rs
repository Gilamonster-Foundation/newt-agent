//! Parent-directory startup and private-frame boundary regressions.
use super::*;

/// A genuine mutable grant remains refused, but the operator must be told
/// exactly which anchor to remove or relocate instead of receiving a bare error.
#[test]
fn mutable_anchor_refusal_names_the_offending_path() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().canonicalize().unwrap();
    let bin = workspace.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let error = validate_stable_anchor(&bin, &[(workspace.clone(), workspace)])
        .unwrap_err()
        .to_string();
    assert!(error.contains(bin.to_str().unwrap()), "{error}");
}

/// Ground the admission check in a real symlink replacement: no independent
/// PATH directory rule may make the private frame readable through workspace/bin.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn swapped_path_directory_cannot_read_private_frame() {
    use crate::confined_exec::{ConstrainedExecutor, ExecOrigin, ExecRequest};
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let frame = root.join("frame");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&frame).unwrap();
    std::fs::write(frame.join("record"), "private frame sentinel").unwrap();
    let bin = workspace.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let writable = [(workspace.clone(), workspace.clone())];
    assert!(validate_stable_anchor(&bin, &writable).is_err());
    std::fs::remove_dir(&bin).unwrap();
    std::os::unix::fs::symlink(&frame, &bin).unwrap();
    assert!(validate_stable_anchor(&bin, &writable).is_err());
    let caveats = crate::confined_exec::workspace_confined_caveats(&workspace);
    let request = |path: &Path| {
        ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            "/bin/cat",
            [path.to_string_lossy().into_owned()],
            &workspace,
            caveats.clone(),
        )
    };
    std::fs::write(workspace.join("readable"), "allowed control").unwrap();
    let control = ConstrainedExecutor::run(&request(&workspace.join("readable"))).unwrap();
    assert!(control.success, "control: {control:?}");
    assert_eq!(control.stdout, b"allowed control");
    let denied = ConstrainedExecutor::run(&request(&bin.join("record"))).unwrap();
    assert!(!denied.success, "private frame was readable: {denied:?}");
    assert!(denied.stdout.is_empty());
}

/// Exact TUI authority shape: bare commands plus a PATH directory beneath the
/// launch root. Keep the primary session alive with a path-bearing notice;
/// the same authority with a trusted PATH still admits the feature silently.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn parent_directory_path_disables_only_smart_harness() {
    let _lock = crate::process_env::lock();
    struct Restore(Vec<(&'static str, Option<String>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                crate::process_env::set_or_remove(key, value.as_deref());
            }
        }
    }
    let keys = ["PATH", "NEWT_DISABLE_OCAP", "NEWT_FULL_ACCESS"];
    let _restore = Restore(keys.map(|k| (k, std::env::var(k).ok())).to_vec());
    crate::process_env::set_var("NEWT_DISABLE_OCAP", "0");
    crate::process_env::set_var("NEWT_FULL_ACCESS", "0");
    let workspace = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let bin = workspace.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let frame = private.path().join("frame");
    let mut caveats = crate::confined_exec::build_tool_caveats(workspace.path());
    caveats.exec = Scope::only(["cargo".into(), "gh".into()]);
    let launch = HarnessLaunch {
        workspace: workspace.path(),
        caveats: &caveats,
        frame_dir: Some(&frame),
        resume_from: None,
        hermetic: false,
    };
    crate::process_env::set_var("PATH", &format!("{}:/usr/bin:/bin", bin.display()));
    let mut notices = Vec::new();
    let disabled =
        SmartHarnessConfig::default().admit_startup(&launch, |n| notices.push(n.to_owned()));
    assert!(disabled.is_none());
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains(bin.to_str().unwrap()), "{}", notices[0]);
    assert!(notices[0].contains("restart"));
    assert!(!frame.exists());
    crate::process_env::set_var("PATH", "/usr/bin:/bin");
    let admitted =
        SmartHarnessConfig::default().admit_startup(&launch, |n| panic!("unexpected: {n}"));
    assert!(admitted.is_some());
    assert!(!frame.exists(), "admission must not open a session");
}
