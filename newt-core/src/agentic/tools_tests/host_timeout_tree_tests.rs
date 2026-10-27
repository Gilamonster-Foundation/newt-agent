//! #2882 — a host-shell timeout kills the child's whole process group, keeps
//! the output printed before the wall, and carries the confined route's
//! timed-out note. Fully mocked: no subprocess, paused clock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

/// A pipe that has delivered `bytes` and then stays open (its writer is
/// returned so it is never dropped), like a child that printed and then hung.
async fn hung_pipe(bytes: &[u8]) -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
    let (mut writer, reader) = tokio::io::duplex(1024);
    writer.write_all(bytes).await.unwrap();
    (reader, writer)
}

#[tokio::test(start_paused = true)]
async fn host_timeout_kills_the_group_and_keeps_partial_output() {
    let killed = Arc::new(AtomicBool::new(false));
    let (stdout, _out_writer) = hung_pipe(b"started\n").await;
    let (stderr, _err_writer) = hung_pipe(b"warming up\n").await;
    let flag = killed.clone();
    let run = super::shell::supervise_host_child(
        std::future::pending::<std::io::Result<i64>>(),
        move || flag.store(true, Ordering::SeqCst),
        stdout,
        stderr,
        None,
        Duration::from_secs(5),
    )
    .await
    .expect("a timeout is a result, not an error");

    assert!(run.timed_out);
    assert_eq!(run.exit_code, 124);
    assert!(
        killed.load(Ordering::SeqCst),
        "the timeout must kill the child's whole process group"
    );
    assert_eq!(super::shell::decode_shell_stream(&run.stdout), "started\n");
    let stderr = super::shell::decode_shell_stream(&run.stderr);
    assert!(stderr.starts_with("warming up\n"), "{stderr}");
    assert!(stderr.contains("5s host-shell timeout"), "{stderr}");
}

#[tokio::test(start_paused = true)]
async fn host_completion_does_not_kill_the_group() {
    let killed = Arc::new(AtomicBool::new(false));
    let flag = killed.clone();
    let run = super::shell::supervise_host_child(
        async { Ok(0) },
        move || flag.store(true, Ordering::SeqCst),
        &b"done\n"[..],
        &b""[..],
        None,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert!(!run.timed_out);
    assert_eq!(run.exit_code, 0);
    assert!(!killed.load(Ordering::SeqCst));
    assert_eq!(super::shell::decode_shell_stream(&run.stdout), "done\n");
}

/// A reader whose first read fails, like a broken child pipe.
struct FailingPipe;

impl tokio::io::AsyncRead for FailingPipe {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("pipe broke")))
    }
}

/// The user-interrupt exit: dispatch drops the tool future mid-run. The group
/// must die with it, not survive as orphans of a killed `bash` (#2882).
#[tokio::test(start_paused = true)]
async fn dropping_the_supervisor_mid_run_kills_the_group() {
    let killed = Arc::new(AtomicBool::new(false));
    let (stdout, _out_writer) = hung_pipe(b"started\n").await;
    let (stderr, _err_writer) = hung_pipe(b"").await;
    let flag = killed.clone();
    let run = super::shell::supervise_host_child(
        std::future::pending::<std::io::Result<i64>>(),
        move || flag.store(true, Ordering::SeqCst),
        stdout,
        stderr,
        None,
        Duration::from_secs(60),
    );
    // Interrupt well before the wall: the future is dropped unfinished.
    let interrupted = tokio::time::timeout(Duration::from_secs(1), run).await;
    assert!(interrupted.is_err(), "the run must still be in flight");
    assert!(
        killed.load(Ordering::SeqCst),
        "dropping the supervisor must kill the child's process group"
    );
}

/// The failed-pipe-read exit: the run errors out, and the group must not be
/// left running behind the error.
#[tokio::test(start_paused = true)]
async fn a_failed_pipe_read_kills_the_group() {
    let killed = Arc::new(AtomicBool::new(false));
    let (stderr, _err_writer) = hung_pipe(b"").await;
    let flag = killed.clone();
    let run = super::shell::supervise_host_child(
        std::future::pending::<std::io::Result<i64>>(),
        move || flag.store(true, Ordering::SeqCst),
        FailingPipe,
        stderr,
        None,
        Duration::from_secs(60),
    )
    .await;
    assert!(run.is_err(), "a broken pipe is an error");
    assert!(
        killed.load(Ordering::SeqCst),
        "an errored run must kill the child's process group"
    );
}

/// The leader exits (its wait is ready at once) while an escaped descendant
/// keeps a pipe open, then the wall fires. Reaping the leader frees its pgid
/// for reuse, so the group kill must never run after the child was reaped: it
/// could signal an unrelated group that took the number (#2882, PR #2892).
#[tokio::test(start_paused = true)]
async fn the_group_kill_never_runs_after_the_leader_was_reaped() {
    let reaped = Arc::new(AtomicBool::new(false));
    let killed = Arc::new(AtomicBool::new(false));
    let killed_after_reap = Arc::new(AtomicBool::new(false));
    let (stdout, _out_writer) = hung_pipe(b"started\n").await;
    let (stderr, _err_writer) = hung_pipe(b"").await;
    let wait_reaped = reaped.clone();
    let (kill_reaped, kill_flag, kill_late) =
        (reaped.clone(), killed.clone(), killed_after_reap.clone());
    let run = super::shell::supervise_host_child(
        async move {
            wait_reaped.store(true, Ordering::SeqCst);
            Ok(0)
        },
        move || {
            kill_flag.store(true, Ordering::SeqCst);
            if kill_reaped.load(Ordering::SeqCst) {
                kill_late.store(true, Ordering::SeqCst);
            }
        },
        stdout,
        stderr,
        None,
        Duration::from_secs(5),
    )
    .await
    .expect("a timeout is a result, not an error");

    assert!(run.timed_out);
    assert!(
        killed.load(Ordering::SeqCst),
        "the wall must still kill the group"
    );
    assert!(
        !killed_after_reap.load(Ordering::SeqCst),
        "the group kill ran after the leader was reaped; its pgid may be reused"
    );
    assert_eq!(super::shell::decode_shell_stream(&run.stdout), "started\n");
}

/// Partial stderr that does not end in a newline must not run into the
/// timeout line.
#[tokio::test(start_paused = true)]
async fn the_timeout_line_starts_on_its_own_line() {
    let (stdout, _out_writer) = hung_pipe(b"").await;
    let (stderr, _err_writer) = hung_pipe(b"progress 40%").await;
    let run = super::shell::supervise_host_child(
        std::future::pending::<std::io::Result<i64>>(),
        || {},
        stdout,
        stderr,
        None,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let stderr = super::shell::decode_shell_stream(&run.stderr);
    assert!(
        stderr.starts_with("progress 40%\ncommand exceeded 5s"),
        "{stderr}"
    );
}

#[test]
fn host_timeout_result_carries_the_confined_timed_out_note() {
    let wall = Duration::from_secs(60);
    let envelope = serde_json::json!({
        "exit_code": 124, "timeout_secs": 60, "stdout": "started\n",
        "stderr": "", "timed_out": true,
    });
    let (text, outcome) = super::shell::host_result(
        &envelope,
        |e| e["stdout"].as_str().unwrap().to_string(),
        Default::default(),
    );
    assert_eq!(outcome, crate::ExecOutcome::TimedOut);
    assert!(text.starts_with("started\n"), "{text}");
    assert!(
        text.ends_with(&super::shell::timed_out_note(wall, Default::default())),
        "{text}"
    );
}

/// Grounds [`host_timeout_kills_the_group_and_keeps_partial_output`]: that
/// mocked test believes the kill closure `run_one` hands the supervisor really
/// kills (SIGKILLs) a backgrounded grandchild, and that output printed before the wall
/// survives. This spawns a real `sh` tree and checks both. It does not touch
/// the process environment.
#[cfg(unix)]
#[ignore = "real process tree; grounds host_timeout_kills_the_group_and_keeps_partial_output (#2882)"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_host_timeout_leaves_no_survivor_in_the_group() {
    let run = super::shell::host_shell_output_with_timeout(
        super::shell::ShellRoute::BashSh,
        "echo pgid=$(ps -o pgid= -p $$ | tr -d ' '); sleep 600 & echo started; wait",
        ".",
        None,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert!(run.timed_out);
    let stdout = super::shell::decode_shell_stream(&run.stdout);
    assert!(stdout.contains("started"), "{stdout}");
    let pgid: i32 = stdout
        .lines()
        .find_map(|l| l.strip_prefix("pgid="))
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or_else(|| panic!("no pgid in {stdout}"));
    // killpg(pgid, 0) is a liveness probe: ESRCH means no member survives.
    let mut alive = true;
    for _ in 0..50 {
        // SAFETY: signal 0 delivers nothing; it only checks existence.
        alive = unsafe { libc::killpg(pgid, 0) } == 0;
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !alive,
        "process group {pgid} still has members after the timeout"
    );
}

/// The child half of [`ambient_login_shell_under_a_controlling_tty_does_not_hang`]:
/// runs inside a process that HAS a controlling terminal.
#[cfg(unix)]
#[ignore = "child half of ambient_login_shell_under_a_controlling_tty_does_not_hang"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambient_login_shell_pty_child() {
    let _pin = crate::launch_authority::ScopedAuthority::ambient();
    let run = super::shell::host_shell_output_with_timeout(
        super::shell::ShellRoute::BashSh,
        "echo hi",
        ".",
        None,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    println!(
        "RESULT timed_out={} exit={} stdout={:?} stderr={:?}",
        run.timed_out,
        run.exit_code,
        super::shell::decode_shell_stream(&run.stdout),
        super::shell::decode_shell_stream(&run.stderr),
    );
}

/// #3005 regression: `--ambient` runs `bash -l -i -c`. In a background process group
/// of the controlling terminal's session, an interactive bash SIGTTINs itself and
/// stops, so every command hung to the host timeout. The child here is given a real
/// controlling tty (`setsid` + `TIOCSCTTY`, as a terminal emulator does).
#[cfg(unix)]
#[ignore = "real pty + login shell; regression for the #3005 --ambient hang"]
#[test]
fn ambient_login_shell_under_a_controlling_tty_does_not_hang() {
    use std::os::unix::process::CommandExt as _;
    let pty = tests_pty::Pty::open();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "agentic::tools::host_timeout_tree_tests::ambient_login_shell_pty_child",
            "--ignored",
            "--nocapture",
        ])
        .env("SHELL", "/bin/bash")
        .stdin(pty.slave_stdio())
        .stdout(pty.slave_stdio())
        .stderr(std::process::Stdio::null());
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn the pty child");
    let started = std::time::Instant::now();
    let status = child.wait().expect("wait for the pty child");
    // A hung login shell may still hold the slave, so no EOF: read what was drained.
    pty.wait_for_screen("RESULT", Duration::from_secs(3));
    let screen = pty.screen();
    assert!(status.success(), "child failed:\n{screen:?}");
    assert!(
        screen.contains("RESULT timed_out=false exit=0 stdout=\"hi\\n\" stderr=\"\""),
        "echo hi must return cleanly with no job-control noise:\n{screen:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8), "{screen:?}");
}
