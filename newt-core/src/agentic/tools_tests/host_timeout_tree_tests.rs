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
