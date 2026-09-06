use super::*;

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_find_on_path(exe: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(exe))
            .find(|p| p.is_file())
    })
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_launcher_path() -> Option<std::path::PathBuf> {
    windows_find_on_path("agent-bridle-aclaunch.exe")
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_netprobe_path() -> Option<std::path::PathBuf> {
    windows_find_on_path("ab-netprobe.exe")
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_appcontainer_available() -> bool {
    let Some(launcher) = windows_launcher_path() else {
        if windows_env_truthy("BRIDLE_REQUIRE_APPCONTAINER") {
            panic!("agent-bridle-aclaunch.exe is required but was not found");
        }
        eprintln!("skipping Windows run_command AppContainer proof: launcher not found");
        return false;
    };
    let out = std::process::Command::new(launcher)
        .args([
            "--name",
            &format!("newt-rc-probe-{}", std::process::id()),
            "cmd.exe",
            "/c",
            "exit 0",
        ])
        .current_dir("C:\\Windows")
        .output()
        .expect("spawn agent-bridle-aclaunch");
    if out.status.success() {
        true
    } else if windows_env_truthy("BRIDLE_REQUIRE_APPCONTAINER") {
        panic!(
            "BRIDLE_REQUIRE_APPCONTAINER is set but AppContainer probe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    } else {
        eprintln!(
            "skipping Windows run_command AppContainer proof: probe failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        false
    }
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_low_dir(kind: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(&format!("newt-rc-{kind}-"))
        .tempdir()
        .expect("create temp dir");
    let _ = std::process::Command::new("icacls")
        .arg(dir.path())
        .args(["/setintegritylevel", "(OI)(CI)Low"])
        .output();
    dir
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_grant_all_appcontainers(path: &std::path::Path) {
    for sid in ["*S-1-15-2-1:(OI)(CI)F", "*S-1-15-2-2:(OI)(CI)F"] {
        let out = std::process::Command::new("icacls")
            .arg(path)
            .args(["/grant", sid])
            .output()
            .expect("run icacls grant");
        assert!(
            out.status.success(),
            "failed to grant AppContainer fixture DACL {sid} on {}; stdout={} stderr={}",
            windows_path(path),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_path(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_stage_netprobe() -> Option<(tempfile::TempDir, std::path::PathBuf)> {
    let Some(source) = windows_netprobe_path() else {
        if windows_env_truthy("BRIDLE_REQUIRE_APPCONTAINER") {
            panic!("ab-netprobe.exe is required but was not found");
        }
        eprintln!("skipping Windows run_command net proof: ab-netprobe.exe not found");
        return None;
    };
    let dir = windows_low_dir("netprobe");
    let dest = dir.path().join("ab-netprobe.exe");
    std::fs::copy(&source, &dest).expect("stage ab-netprobe.exe");
    Some((dir, dest))
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_tcp_listener() -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.write_all(b"ok");
                    let mut buf = Vec::new();
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
                    let _ = stream.read_to_end(&mut buf);
                    let _ = tx.send(buf);
                    return;
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => return,
            }
        }
    });
    (port, rx)
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn windows_host_netprobe_connects(port: u16) -> bool {
    windows_netprobe_path()
        .and_then(|probe| {
            std::process::Command::new(probe)
                .args(["127.0.0.1", &port.to_string()])
                .output()
                .ok()
        })
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
fn cmd_set_content(path: &std::path::Path, value: &str) -> serde_json::Value {
    let command = format!("echo {value}>{}", windows_path(path));
    serde_json::json!({
        "program": "cmd.exe",
        "args": ["/d", "/c", command],
    })
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
struct EnvGuard {
    key: &'static str,
    saved: Option<String>,
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let saved = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, saved }
    }
}

#[cfg(all(windows, feature = "windows-appcontainer"))]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.saved.take() {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Windows route proof: `run_command`'s private `dispatch_bridled_shell`
/// path engages AppContainer for fs-restricted caveats. A write to a granted
/// directory succeeds; the same shell command against an ungranted sibling is
/// blocked by the AppContainer/ACL boundary.
#[cfg(all(windows, feature = "windows-appcontainer"))]
#[tokio::test]
async fn run_command_windows_appcontainer_allows_granted_write_denies_sibling_write() {
    let _lock = disable_ocap_tests::env_lock().await;
    let _engine = disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if !windows_appcontainer_available() {
        return;
    }

    let parent = windows_low_dir("siblings");
    let workspace = parent.path().join("workspace");
    let sibling = parent.path().join("sibling");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let _ = std::process::Command::new("icacls")
        .arg(&workspace)
        .args(["/setintegritylevel", "(OI)(CI)Low"])
        .output();
    windows_grant_all_appcontainers(&workspace);
    let _ = std::process::Command::new("icacls")
        .arg(&sibling)
        .args(["/setintegritylevel", "(OI)(CI)Low"])
        .output();
    let granted = workspace.join("granted.txt");
    let denied = sibling.join("denied.txt");
    std::fs::write(&granted, "ORIG").unwrap();
    std::fs::write(&denied, "ORIG").unwrap();

    let caveats = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([windows_path(&workspace), windows_path(&sibling)]),
        fs_write: crate::caveats::Scope::only([windows_path(&workspace)]),
        exec: crate::caveats::Scope::All,
        net: crate::caveats::Scope::All,
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: crate::caveats::Scope::All,
    };

    let mut granted_args = cmd_set_content(&granted, "GRANTED");
    granted_args["cwd"] = serde_json::Value::String(windows_path(&workspace));
    let ok = super::shell::dispatch_bridled_shell(granted_args, &caveats, None)
        .await
        .expect("granted run_command dispatch");
    assert_eq!(
        ok["sandbox_kind"], "app_container",
        "run_command must engage AppContainer on Windows: {ok}"
    );
    assert!(
        std::fs::read_to_string(&granted)
            .unwrap_or_default()
            .contains("GRANTED"),
        "granted workspace write should succeed through run_command; file={:?}; envelope={ok}",
        std::fs::read_to_string(&granted).unwrap_or_default()
    );

    let mut denied_args = cmd_set_content(&denied, "DENIED");
    denied_args["cwd"] = serde_json::Value::String(windows_path(&workspace));
    let no = super::shell::dispatch_bridled_shell(denied_args, &caveats, None)
        .await
        .expect("denied run_command dispatch");
    assert_eq!(
        no["sandbox_kind"], "app_container",
        "denial must still be from the AppContainer route: {no}"
    );
    assert!(
        !std::fs::read_to_string(&denied)
            .unwrap_or_default()
            .contains("DENIED"),
        "sibling write must not escape run_command's AppContainer fence"
    );
}

/// Windows route proof for the network axis: a run_command child can execute
/// a real helper, but AppContainer net:none prevents it from opening a direct
/// loopback TCP connection that the same helper can open on the host.
#[cfg(all(windows, feature = "windows-appcontainer"))]
#[tokio::test]
async fn run_command_windows_appcontainer_denies_direct_tcp() {
    let _lock = disable_ocap_tests::env_lock().await;
    let _engine = disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    if !windows_appcontainer_available() {
        return;
    }
    let Some((probe_dir, probe)) = windows_stage_netprobe() else {
        return;
    };
    let (host_port, host_rx) = windows_tcp_listener();
    assert!(
        windows_host_netprobe_connects(host_port),
        "host netprobe control must connect"
    );
    assert!(
        host_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok(),
        "host control listener must observe the connection"
    );

    let workspace = windows_low_dir("tcp");
    let (port, rx) = windows_tcp_listener();
    let caveats = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([
            windows_path(workspace.path()),
            windows_path(probe_dir.path()),
        ]),
        fs_write: crate::caveats::Scope::only([windows_path(workspace.path())]),
        exec: crate::caveats::Scope::All,
        net: crate::caveats::Scope::none(),
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: crate::caveats::Scope::All,
    };
    let envelope = super::shell::dispatch_bridled_shell(
        serde_json::json!({
            "program": windows_path(&probe),
            "args": ["127.0.0.1", port.to_string()],
            "cwd": windows_path(workspace.path()),
        }),
        &caveats,
        None,
    )
    .await
    .expect("run_command tcp dispatch");
    assert_eq!(
        envelope["sandbox_kind"], "app_container",
        "direct TCP denial must run through AppContainer: {envelope}"
    );
    assert!(
        envelope["exit_code"].as_i64().unwrap_or_default() != 0,
        "direct TCP probe should fail under AppContainer net:none: {envelope}"
    );
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(500))
            .is_err(),
        "parent listener must not observe a denied run_command TCP connection"
    );
}

/// Windows route proof for #8 on the `run_command` shell path: current
/// agent-bridle `ShellTool` Windows children still inherit ambient parent
/// environment. This pins the ACTIVE shared dependency defect instead of
/// adding a Newt-only shim around the bridle spawn path.
#[cfg(all(windows, feature = "windows-appcontainer"))]
#[tokio::test]
async fn run_command_windows_provider_env_inheritance_is_active() {
    let _lock = disable_ocap_tests::env_lock().await;
    let _engine = disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _key = EnvGuard::set("OPENAI_API_KEY", "sk-run-command-windows-secret");
    if !windows_appcontainer_available() {
        return;
    }
    let workspace = windows_low_dir("env");
    windows_grant_all_appcontainers(workspace.path());
    let caveats = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([windows_path(workspace.path())]),
        fs_write: crate::caveats::Scope::only([windows_path(workspace.path())]),
        exec: crate::caveats::Scope::All,
        net: crate::caveats::Scope::All,
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: crate::caveats::Scope::All,
    };
    let envelope = super::shell::dispatch_bridled_shell(
        serde_json::json!({
            "program": "cmd.exe",
            "args": [
                "/d",
                "/c",
                "if defined OPENAI_API_KEY (echo %OPENAI_API_KEY%) else (echo EMPTY)"
            ],
            "cwd": windows_path(workspace.path()),
        }),
        &caveats,
        None,
    )
    .await
    .expect("run_command env dispatch");
    assert_eq!(
        envelope["sandbox_kind"], "app_container",
        "env proof must run through AppContainer: {envelope}"
    );
    assert_eq!(
        envelope["exit_code"], 0,
        "env probe must execute, not pass by failing to spawn: {envelope}"
    );
    let text = envelope["stdout"].as_str().unwrap_or_default();
    assert!(
            text.contains("sk-run-command-windows-secret"),
            "expected to prove the ACTIVE shared bridle Windows env-inheritance residual; stdout was {text:?}. Flip this test to denial when agent-bridle grows Windows env_clear parity."
        );
}

/// Windows missing-backend truth for `run_command`: with AppContainer support
/// compiled in but the launcher hidden from PATH, the shell route refuses
/// before executing the hostile command. This is a Windows-specific contrast
/// to the cross-platform advisory-backend residual documented in the
/// deviation register.
#[cfg(all(windows, feature = "windows-appcontainer"))]
#[tokio::test]
async fn run_command_windows_missing_launcher_refuses_not_host_fallback() {
    let _lock = disable_ocap_tests::env_lock().await;
    let _engine = disable_ocap_tests::EnvVar::set("NEWT_SHELL_ENGINE", "host");
    let current_exe = std::env::current_exe().expect("current exe");
    if current_exe
        .parent()
        .map(|p| p.join("agent-bridle-aclaunch.exe"))
        .is_some_and(|p| p.exists())
    {
        eprintln!("skipping run_command missing-launcher proof: launcher is next to the test exe");
        return;
    }

    let empty_path = windows_low_dir("empty-path");
    let _path = EnvGuard::set("PATH", &windows_path(empty_path.path()));
    let _path_mixed_case = EnvGuard::set("Path", &windows_path(empty_path.path()));
    let workspace = windows_low_dir("missing");
    let marker = workspace.path().join("fallback.txt");
    std::fs::write(&marker, "ORIG").unwrap();
    let caveats = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::only([windows_path(workspace.path())]),
        fs_write: crate::caveats::Scope::only([windows_path(workspace.path())]),
        exec: crate::caveats::Scope::All,
        net: crate::caveats::Scope::All,
        max_calls: crate::caveats::CountBound::Unlimited,
        valid_for_generation: crate::caveats::Scope::All,
    };
    let result = super::shell::dispatch_bridled_shell(
            serde_json::json!({"cmd": "echo HOST-FALLBACK>fallback.txt", "cwd": windows_path(workspace.path())}),
            &caveats,
            None,
        )
        .await;
    assert!(
            result.is_err(),
            "missing AppContainer launcher should refuse, not return a host/advisory envelope: {result:?}"
        );
    assert!(
        !std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .contains("HOST-FALLBACK"),
        "missing launcher must not run the shell command on the host"
    );
}
