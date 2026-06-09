//! Integration tests for `newt worker` startup: operator-key resolution
//! (issue #94), the `--allow-no-key` debug fallback, and the optional
//! Prometheus metrics server (`NEWT_METRICS_PORT`).
//!
//! Every test runs with `HOME` redirected to a tempdir and the operator key
//! path passed explicitly, so the developer's real `~/.newt/identity.pem` is
//! never read or written.

#![cfg(unix)]

use assert_cmd::Command;
use predicates::prelude::*;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

const INIT_LINE: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n";

/// `newt worker` isolated from the developer's environment. Stdin gets one
/// `initialize` request and is then closed, so a successfully started worker
/// answers and exits on EOF.
fn worker(home: &tempfile::TempDir) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.env("HOME", home.path())
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        .env("RUST_LOG", "info")
        .env_remove("NEWT_OPERATOR_KEY")
        .env_remove("NEWT_METRICS_PORT")
        .env_remove("NEWT_CODER")
        .arg("worker")
        .write_stdin(INIT_LINE)
        .timeout(Duration::from_secs(30));
    cmd
}

/// A path whose parent is a regular file — key generation can never succeed
/// there, which is how we force the unresolved-key startup refusal.
fn blocked_key_path(home: &tempfile::TempDir) -> std::path::PathBuf {
    let blocker = home.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    blocker.join("identity.pem")
}

#[test]
fn worker_refuses_to_start_when_key_unavailable() {
    let home = tempfile::tempdir().unwrap();
    let key = blocked_key_path(&home);

    worker(&home)
        .arg("--operator-key-path")
        .arg(&key)
        .assert()
        .failure()
        .stderr(predicate::str::contains("headless worker refused to start"))
        .stderr(predicate::str::contains("--allow-no-key"));
}

#[test]
fn worker_generates_key_and_answers_initialize() {
    let home = tempfile::tempdir().unwrap();
    let key = home.path().join("keys").join("identity.pem");
    assert!(!key.exists());

    worker(&home)
        .arg("--operator-key-path")
        .arg(&key)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"jsonrpc\":\"2.0\""))
        .stderr(predicate::str::contains("operator-rooted identity"));

    assert!(key.exists(), "first run must persist the operator key");
}

#[test]
fn worker_allow_no_key_falls_back_to_debug_identity() {
    let home = tempfile::tempdir().unwrap();
    let key = blocked_key_path(&home);

    worker(&home)
        .arg("--operator-key-path")
        .arg(&key)
        .arg("--allow-no-key")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"jsonrpc\":\"2.0\""))
        .stderr(predicate::str::contains("unbounded debug authority"));
}

#[test]
fn worker_ignores_invalid_metrics_port() {
    let home = tempfile::tempdir().unwrap();
    let key = home.path().join("identity.pem");

    // Port 0 fails the `p > 0` filter — the worker must start without a
    // metrics server rather than crash.
    worker(&home)
        .arg("--operator-key-path")
        .arg(&key)
        .env("NEWT_METRICS_PORT", "0")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"jsonrpc\":\"2.0\""))
        .stderr(predicate::str::contains("Prometheus metrics server started").not());
}

/// One raw HTTP/1.0 request against the metrics server; returns the full
/// response (status line + headers + body).
fn http_get(port: u16, path: &str) -> std::io::Result<String> {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    Ok(resp)
}

/// Like [`http_get`], but retries transient connection failures for up to
/// `secs` seconds. Even after the server answers `/healthz`, an *individual*
/// connect can be briefly refused while the single-threaded accept loop is
/// busy — and that window stretches under `cargo llvm-cov`, whose instrumented
/// binary runs several times slower. Single-shot requests were flaky there
/// (the `/nope` connect failed on `main`'s coverage job); callers that already
/// know the server is up use this instead.
fn http_get_retry(port: u16, path: &str, secs: u64) -> std::io::Result<String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match http_get(port, path) {
            Ok(resp) => return Ok(resp),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

#[test]
fn worker_metrics_server_serves_healthz_and_metrics() {
    let home = tempfile::tempdir().unwrap();
    let key = home.path().join("identity.pem");

    // Reserve a free port, then release it for the worker to bind.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };

    let newt_bin = assert_cmd::cargo::cargo_bin("newt");
    let mut child = std::process::Command::new(&newt_bin)
        .env("HOME", home.path())
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        .env("RUST_LOG", "info")
        .env("NEWT_METRICS_PORT", port.to_string())
        .env_remove("NEWT_OPERATOR_KEY")
        .args(["worker", "--operator-key-path"])
        .arg(&key)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn newt worker");

    // Poll until the metrics endpoint answers (the server task starts
    // concurrently with the worker's stdio loop).
    let deadline = Instant::now() + Duration::from_secs(15);
    let healthz = loop {
        match http_get(port, "/healthz") {
            Ok(resp) => break resp,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = child.kill();
                panic!("metrics server never came up on port {port}: {e}");
            }
        }
    };
    assert!(healthz.contains("200 OK"), "healthz response: {healthz}");
    assert!(healthz.contains("ok"), "healthz body: {healthz}");

    let metrics = http_get_retry(port, "/metrics", 15).expect("GET /metrics");
    assert!(metrics.contains("200 OK"), "metrics response: {metrics}");

    let not_found = http_get_retry(port, "/nope", 15).expect("GET /nope");
    assert!(not_found.contains("404 Not Found"), "response: {not_found}");

    // Close stdin → the worker's readline loop exits and the process ends.
    drop(child.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            None => {
                let _ = child.kill();
                panic!("worker did not exit after stdin EOF");
            }
        }
    };
    assert!(status.success(), "worker exit status: {status}");

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr piped")
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains("Prometheus metrics server started"),
        "stderr: {stderr}"
    );
}
