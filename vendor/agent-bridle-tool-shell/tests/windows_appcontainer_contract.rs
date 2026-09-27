//! Windows route-level proof for the v0.8 Newt `CONFINED` contract.
//!
//! This exercises the production `ShellTool` spawn path with the exact Newt-style
//! caveats: `{fs_read: workspace, fs_write: workspace, net: deny-all,
//! exec: allowlist}` under `EnforcementFloor::CONFINED`. It complements the pure
//! `enforcement_report` tests by proving the route really selects AppContainer
//! and fails closed when `agent-bridle-aclaunch.exe` cannot be found.

#![cfg(all(target_os = "windows", feature = "windows-appcontainer"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_bridle_core::{
    Caveats, EnforcementFloor, Gate, SandboxPolicy, Scope, Tool, ToolContext, ToolError,
};
use agent_bridle_tool_shell::ShellTool;
use tokio::sync::Mutex;

static N: AtomicU64 = AtomicU64::new(0);
static PATH_TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn tag(kind: &str) -> String {
    format!(
        "{kind}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

fn fresh_dir(kind: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("ab-newt-contract-{}", tag(kind)));
    std::fs::create_dir_all(&d).expect("create temp dir");
    let _ = Command::new("icacls")
        .arg(&d)
        .args(["/setintegritylevel", "(OI)(CI)Low"])
        .output();
    d
}

fn ctx_confined(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .with_enforcement_floor(EnforcementFloor::CONFINED)
        .authorize(&ShellTool::new(), &granted)
        .expect("authorize")
}

fn ctx_confined_for(tool: &ShellTool, granted: Caveats) -> ToolContext {
    Gate::new(0)
        .with_enforcement_floor(EnforcementFloor::CONFINED)
        .authorize(tool, &granted)
        .expect("authorize")
}

fn newt_contract(workspace: &Path) -> Caveats {
    let ws = workspace.to_string_lossy().into_owned();
    Caveats {
        fs_read: Scope::only([ws.clone()]),
        fs_write: Scope::only([ws]),
        net: Scope::none(),
        exec: Scope::only(["cmd.exe".to_string()]),
        ..Caveats::top()
    }
}

fn launcher_path() -> PathBuf {
    let mut p = std::env::current_exe().expect("current test executable");
    p.pop(); // deps
    p.pop(); // debug
    p.push("agent-bridle-aclaunch.exe");
    assert!(
        p.exists(),
        "Windows contract proof requires agent-bridle-aclaunch.exe to be built at {p:?}; \
         run `cargo test -p agent-bridle-aclaunch --bins` before this test"
    );
    p
}

fn sandbox_with_launcher(launcher: &Path) -> SandboxPolicy {
    SandboxPolicy {
        appcontainer_launcher_path: Some(launcher.to_string_lossy().into_owned()),
        ..SandboxPolicy::default()
    }
}

fn compile_fake_aclaunch(dir: &Path) -> PathBuf {
    let source = dir.join("fake_aclaunch.rs");
    let exe = dir.join("agent-bridle-aclaunch.exe");
    std::fs::write(
        &source,
        r#"
fn main() {
    let marker = std::env::var("AB_FAKE_ACLAUNCH_MARKER")
        .expect("AB_FAKE_ACLAUNCH_MARKER");
    std::fs::write(marker, "FAKE_LAUNCHER_RAN").expect("write fake marker");
    std::process::exit(91);
}
"#,
    )
    .expect("write fake launcher source");
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let out = Command::new(rustc)
        .arg("--edition=2021")
        .arg(&source)
        .arg("-o")
        .arg(&exe)
        .output()
        .expect("spawn rustc for fake launcher");
    assert!(
        out.status.success(),
        "compile fake launcher failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    exe
}

struct PathGuard {
    old: Option<std::ffi::OsString>,
}

impl PathGuard {
    fn set(new_path: std::ffi::OsString) -> Self {
        let old = std::env::var_os("PATH");
        std::env::set_var("PATH", new_path);
        Self { old }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        if let Some(old) = &self.old {
            std::env::set_var("PATH", old);
        } else {
            std::env::remove_var("PATH");
        }
    }
}

fn prepend_to_path(dir: &Path) -> PathGuard {
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    PathGuard::set(std::env::join_paths(paths).expect("join PATH"))
}

#[tokio::test]
async fn newt_confined_contract_runs_only_inside_appcontainer() {
    let launcher = launcher_path();
    let tool = ShellTool::new().with_sandbox_policy(sandbox_with_launcher(&launcher));
    let workspace = fresh_dir("admit");
    let marker = workspace.join("marker.txt");
    std::fs::write(&marker, "ORIG").expect("seed marker");

    let out = tool
        .invoke(
            serde_json::json!({
                "program": "cmd.exe",
                "args": ["/c", "echo", "NEWT_OK", ">", marker.to_string_lossy()],
                "cwd": workspace.to_string_lossy(),
                "timeout_secs": 5
            }),
            &ctx_confined_for(&tool, newt_contract(&workspace)),
        )
        .await
        .expect("invoke");

    assert_eq!(out["exit_code"], 0, "{out}");
    assert_eq!(out["sandbox_kind"], "app_container", "{out}");
    assert_eq!(out["enforcement"]["fs_read"], "kernel", "{out}");
    assert_eq!(out["enforcement"]["fs_write"], "kernel", "{out}");
    assert_eq!(out["enforcement"]["net"], "kernel", "{out}");
    assert_eq!(out["enforcement"]["exec"], "interceptor", "{out}");
    assert!(
        std::fs::read_to_string(&marker)
            .expect("read marker")
            .contains("NEWT_OK"),
        "admitted AppContainer run must write only inside the granted workspace"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn path_shadowed_aclaunch_refuses_before_fake_launcher_or_target_runs() {
    let _path_test = PATH_TEST_LOCK.lock().await;
    let fake_dir = fresh_dir("fake-path");
    let _fake_launcher = compile_fake_aclaunch(&fake_dir);
    let _path = prepend_to_path(&fake_dir);

    let mut adjacent = std::env::current_exe().expect("current test executable");
    adjacent.set_file_name("agent-bridle-aclaunch.exe");
    assert!(
        !adjacent.exists(),
        "PATH-shadow proof cannot hide a trusted adjacent launcher at {adjacent:?}"
    );

    let workspace = fresh_dir("fake-path-workspace");
    let fake_marker = workspace.join("fake-launcher.txt");
    let target_marker = workspace.join("target.txt");
    std::fs::write(&target_marker, "ORIG").expect("seed target marker");

    let err = ShellTool::new()
        .invoke(
            serde_json::json!({
                "program": "cmd.exe",
                "args": ["/c", "echo", "HOSTILE_STARTED", ">", target_marker.to_string_lossy()],
                "cwd": workspace.to_string_lossy(),
                "env": { "AB_FAKE_ACLAUNCH_MARKER": fake_marker.to_string_lossy() },
                "timeout_secs": 5
            }),
            &ctx_confined(newt_contract(&workspace)),
        )
        .await
        .expect_err("ambient PATH aclaunch must deny before spawn");

    assert!(
        matches!(err, ToolError::Denied { ref reason } if reason.contains("agent-bridle-aclaunch.exe not found")),
        "PATH-shadowed launcher must be a typed denial, got {err:?}"
    );
    assert!(
        !fake_marker.exists(),
        "fake PATH launcher must never execute"
    );
    assert_eq!(
        std::fs::read_to_string(&target_marker).expect("read target marker"),
        "ORIG",
        "hostile target must not start through a PATH-shadowed launcher"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&fake_dir);
}

#[tokio::test]
async fn missing_aclaunch_refuses_newt_confined_contract_before_spawn() {
    let _path_test = PATH_TEST_LOCK.lock().await;
    let mut empty_path = std::env::temp_dir();
    empty_path.push(format!("ab-empty-path-{}", tag("missing")));
    std::fs::create_dir_all(&empty_path).expect("create empty PATH dir");
    let _path = PathGuard::set(empty_path.into_os_string());

    let mut adjacent = std::env::current_exe().expect("current test executable");
    adjacent.set_file_name("agent-bridle-aclaunch.exe");
    assert!(
        !adjacent.exists(),
        "missing-launcher proof cannot hide an adjacent launcher at {adjacent:?}"
    );

    let workspace = fresh_dir("missing");
    let marker = workspace.join("marker.txt");
    std::fs::write(&marker, "ORIG").expect("seed marker");

    let err = ShellTool::new()
        .invoke(
            serde_json::json!({
                "program": "cmd.exe",
                "args": ["/c", "echo", "HOSTILE_STARTED", ">", marker.to_string_lossy()],
                "cwd": workspace.to_string_lossy(),
                "timeout_secs": 5
            }),
            &ctx_confined(newt_contract(&workspace)),
        )
        .await
        .expect_err("missing AppContainer launcher must deny before spawn");

    assert!(
        matches!(err, ToolError::Denied { ref reason } if reason.contains("agent-bridle-aclaunch.exe not found")),
        "missing launcher must be a typed denial, got {err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read marker"),
        "ORIG",
        "hostile command must not start when the AppContainer launcher is unavailable"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}
