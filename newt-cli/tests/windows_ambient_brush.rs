//! #2776: native runtime proof for the production Windows ambient selector,
//! runner, sanitized child PATH, and job-object supervisor. No model/server.
#![cfg(windows)]

use newt_core::ambient_brush::test_dispatch;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn install_runner(root: &Path) -> PathBuf {
    // The embedding host deliberately has no CLI/Brush dispatch entrypoint.
    let host = root.join("embedding-server.exe");
    std::fs::write(&host, b"not a CLI interpreter").unwrap();
    let runner = newt_core::ambient_brush::runner_path(&host);
    std::fs::create_dir(runner.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_newt-ambient-brush"), &runner).unwrap();
    host
}

#[tokio::test]
async fn windows_2776_posix_pipeline_cd_redirect_and_exit_status() {
    let temp = tempfile::tempdir().unwrap();
    let executable = install_runner(temp.path());
    std::fs::create_dir(temp.path().join("space dir")).unwrap();
    let result = test_dispatch(&executable,
        "echo alpha | { read value; echo \"$value\"; }; cd 'space dir'; echo beta > result.txt; echo $?; false",
        temp.path().to_str().unwrap(), false, Duration::from_secs(30)).await.unwrap();
    assert_eq!(result["exit_code"], 1, "{result}");
    assert_eq!(
        result["stdout"].as_str().unwrap().replace('\r', ""),
        "alpha\n0\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("space dir/result.txt"))
            .unwrap()
            .trim(),
        "beta"
    );
    let missing = test_dispatch(
        &executable,
        "newt_missing_2776",
        temp.path().to_str().unwrap(),
        false,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(missing["exit_code"], 127, "{missing}");
    assert!(missing["stderr"]
        .as_str()
        .unwrap()
        .contains("newt_missing_2776"));
}

#[tokio::test]
async fn windows_2776_descendants_inherit_carried_tools_path_and_cmd_opt_out() {
    let temp = tempfile::tempdir().unwrap();
    let executable = install_runner(temp.path());
    let result = test_dispatch(
        &executable,
        "cmd.exe /C set PATH",
        temp.path().to_str().unwrap(),
        false,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(result["exit_code"], 0, "{result}");
    let output = result["stdout"].as_str().unwrap();
    let path = output
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            key.eq_ignore_ascii_case("PATH").then_some(value)
        })
        .expect("descendant PATH");
    assert_eq!(
        std::env::split_paths(path).next().unwrap(),
        executable.parent().unwrap().join("tools")
    );
    let cmd = test_dispatch(
        &executable,
        "echo cmd-route & exit /b 7",
        temp.path().to_str().unwrap(),
        true,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(cmd["exit_code"], 7, "{cmd}");
    assert!(cmd["stdout"].as_str().unwrap().contains("cmd-route"));
}

// Hold a real process handle before cancellation, avoiding PID-reuse ambiguity.
struct Process(windows_sys::Win32::Foundation::HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: this handle was opened by this test and is owned exactly once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

async fn started_descendant(path: PathBuf) -> Process {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
                    // SAFETY: OpenProcess accepts a scalar PID, no borrowed pointers.
                    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
                    assert!(
                        !handle.is_null(),
                        "descendant must be alive before cancellation"
                    );
                    return Process(handle);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("descendant started")
}

async fn descendant_teardown(cancel: bool) {
    let temp = tempfile::tempdir().unwrap();
    let executable = install_runner(temp.path());
    // ASCII Set-Content avoids Windows PowerShell's UTF-16 output default.
    let script = "powershell.exe -NoProfile -Command '[System.IO.File]::WriteAllText(\"pid.txt\", [string]$PID); Start-Sleep -Seconds 300'";
    let mut run = Box::pin(test_dispatch(
        &executable,
        script,
        temp.path().to_str().unwrap(),
        false,
        Duration::from_secs(120),
    ));
    let process = tokio::select! {
        process = started_descendant(temp.path().join("pid.txt")) => process,
        result = &mut run => panic!("command ended before cancellation: {result:?}"),
    };
    if cancel {
        drop(run);
    } else {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(120)).await;
        let result = run.await.unwrap();
        tokio::time::resume();
        assert_eq!(result["timed_out"], true, "{result}");
        assert_eq!(result["exit_code"], 124);
    }
    // SAFETY: the process handle remains live through the finite wait.
    let wait =
        unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(process.0, 5000) };
    assert_eq!(
        wait,
        windows_sys::Win32::Foundation::WAIT_OBJECT_0,
        "descendant survived tree teardown"
    );
}

#[tokio::test]
async fn windows_2776_cancellation_kills_descendants() {
    descendant_teardown(true).await;
}

#[tokio::test]
async fn windows_2776_timeout_kills_descendants() {
    descendant_teardown(false).await;
}

/// #2776: cancellation during stdin delivery must not execute a script prefix.
#[tokio::test]
async fn windows_2776_partial_script_is_refused_before_evaluation() {
    use tokio::io::AsyncWriteExt;
    let temp = tempfile::tempdir().unwrap();
    let executable = install_runner(temp.path());
    let mut child =
        tokio::process::Command::new(newt_core::ambient_brush::runner_path(&executable))
            .current_dir(temp.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"\"echo partial > unintended.txt")
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(!temp.path().join("unintended.txt").exists());
}

/// #2789: an embedding consumer without the separately installed runner keeps
/// a working ambient route; its own entrypoint is never launched as a shell.
#[tokio::test]
async fn windows_2776_missing_runner_executes_cmd_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let host = temp.path().join("server-without-brush.exe");
    let result = test_dispatch(
        &host,
        "echo cmd-fallback & exit /b 9",
        temp.path().to_str().unwrap(),
        false,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(result["exit_code"], 9, "{result}");
    assert!(result["stdout"].as_str().unwrap().contains("cmd-fallback"));
}
