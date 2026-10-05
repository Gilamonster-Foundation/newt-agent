//! Native proof for #2729; run explicitly on macOS with --ignored.
#![cfg(target_os = "macos")]

use newt_core::confined_exec::{build_tool_request, ConstrainedExecutor};
use newt_core::Scope;

/// Grounds the deterministic Build-admission tests in Seatbelt: a host-scoped
/// session can run an offline build child. No model or network request is made.
#[test]
#[ignore = "requires native macOS and cargo on PATH"]
fn host_scoped_build_child_runs_cargo_version() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let session_net = Scope::only(["example.com:443".to_owned()]);
    let request = build_tool_request(&root, &root, "cargo", ["--version"], &session_net)
        .timeout(std::time::Duration::from_secs(30));
    assert_eq!(request.caveats().net, Scope::none());
    let output = ConstrainedExecutor::run(&request).expect("Seatbelt must admit the build child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.success, "stdout={stdout}; stderr={stderr}");
    assert!(stdout.starts_with("cargo "), "{stdout}");
    println!("{stdout}");
    assert_eq!(session_net, Scope::only(["example.com:443".to_owned()]));
}
