//! Real process checks ground the separately opened handles in writer_ownership.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread::JoinHandle,
    time::Duration,
};

use agent_harness::{Session, SessionConfig};
use content_addressable::RawContentId;
use serde_json::{json, Value};

const MODE: &str = "NEWT_FRAME_WRITER_TEST_MODE";
const DIRECTORY: &str = "NEWT_FRAME_WRITER_TEST_DIRECTORY";
const HEAD: &str = "NEWT_FRAME_WRITER_TEST_HEAD";
const MESSAGE: &str = "NEWT_FRAME_WRITER_TEST_MESSAGE ";

struct WriterProcess {
    child: Child,
    input: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
}

impl WriterProcess {
    fn spawn(directory: &Path, mode: &str, head: Option<&str>) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "writer_child", "--nocapture", "--test-threads=1"])
            .env(MODE, mode)
            .env(DIRECTORY, directory)
            .env_remove(HEAD)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(head) = head {
            command.env(HEAD, head);
        }
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (sender, messages) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        let _ = sender.send(Err(error.to_string()));
                        break;
                    }
                };
                if let Some(body) = line.strip_prefix(MESSAGE) {
                    let message = serde_json::from_str(body).map_err(|error| error.to_string());
                    if sender.send(message).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            child,
            input,
            messages,
            reader: Some(reader),
        }
    }

    fn message(&self) -> Value {
        // This bounds a broken child protocol; barriers, not elapsed time,
        // establish when ownership is held or released.
        self.messages
            .recv_timeout(Duration::from_secs(60))
            .expect("writer child must report its state")
            .expect("writer child must send valid JSON")
    }

    fn command(&mut self, command: &str) {
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{command}").unwrap();
        input.flush().unwrap();
    }

    fn wait(&mut self) -> ExitStatus {
        self.input.take();
        self.child.wait().unwrap()
    }

    fn release(&mut self) {
        self.command("release");
        assert_eq!(self.message()["status"], "released");
        assert!(self.wait().success());
    }
}

impl Drop for WriterProcess {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn advance(session: &mut Session, text: &str) {
    session
        .record_messages(&[json!({"role":"user","content":text})])
        .unwrap();
}

fn report(value: Value) {
    let mut stdout = std::io::stdout().lock();
    // Start a line independently of the surrounding libtest progress output.
    writeln!(stdout, "\n{MESSAGE}{value}").unwrap();
    stdout.flush().unwrap();
}

fn state(session: &Session, status: &str) -> Value {
    json!({"status":status,"run":session.run_id().to_string(),"head":session.head().to_string()})
}

#[test]
fn writer_child() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    let directory = std::env::var_os(DIRECTORY).unwrap();
    if mode == "restore" {
        let head = std::env::var(HEAD).unwrap().parse().unwrap();
        match Session::restore(PathBuf::from(directory), head, "local-session") {
            Ok(mut session) => {
                advance(&mut session, "resumed process observation");
                report(state(&session, "restored"));
            }
            Err(error) => report(json!({"status":"refused","error":error.to_string()})),
        }
        return;
    }
    assert_eq!(mode, "own");
    let mut session = Session::open(PathBuf::from(directory), SessionConfig::default()).unwrap();
    advance(&mut session, "owning process observation");
    report(state(&session, "owned"));
    for command in std::io::stdin().lock().lines() {
        match command.unwrap().as_str() {
            "advance" => {
                advance(&mut session, "observation after ownership check");
                report(state(&session, "owned"));
            }
            "release" => {
                drop(session);
                report(json!({"status":"released"}));
                return;
            }
            command => panic!("unknown writer child command: {command}"),
        }
    }
}

fn owner(directory: &Path) -> (WriterProcess, Value) {
    let process = WriterProcess::spawn(directory, "own", None);
    let state = process.message();
    assert_eq!(state["status"], "owned");
    (process, state)
}

fn checkpoint(directory: &Path, state: &Value) -> PathBuf {
    directory.join("heads").join(state["run"].as_str().unwrap())
}

fn evidence(directory: &Path, locator: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        // Lock sidecars are coordination state, not committed evidence. In
        // particular Windows byte-range locks can prohibit reading sidecars.
        if path.extension().is_some_and(|ext| ext == "cbor")
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.parse::<RawContentId>().is_ok())
        {
            result.insert(path.clone(), std::fs::read(path).unwrap());
        }
    }
    result.insert(locator.to_path_buf(), std::fs::read(locator).unwrap());
    result
}

fn restore_after_release(directory: &Path, previous: &Value) {
    let mut process = WriterProcess::spawn(directory, "restore", previous["head"].as_str());
    let restored = process.message();
    assert!(process.wait().success());
    assert_eq!(restored["status"], "restored", "{restored}");
    assert_eq!(restored["run"], previous["run"]);
    assert_ne!(restored["head"], previous["head"]);
    assert_eq!(
        std::fs::read_to_string(checkpoint(directory, previous))
            .unwrap()
            .trim(),
        restored["head"].as_str().unwrap()
    );
}

/// Grounds same-process exclusive-handle tests in two independent processes;
/// refusal must preserve both the current locator and all addressed evidence.
#[test]
fn competing_process_cannot_mutate_an_owned_run() {
    let directory = tempfile::tempdir().unwrap();
    let (mut owner, initial) = owner(directory.path());
    let locator = checkpoint(directory.path(), &initial);
    let before = evidence(directory.path(), &locator);
    let mut contender = WriterProcess::spawn(directory.path(), "restore", initial["head"].as_str());
    let result = contender.message();
    assert!(contender.wait().success());
    assert_eq!(result["status"], "refused", "{result}");
    assert!(
        result["error"].as_str().unwrap().contains(&format!(
            "run {} already has a writer",
            initial["run"].as_str().unwrap()
        )),
        "{result}"
    );
    assert_eq!(evidence(directory.path(), &locator), before);
    owner.command("advance");
    let advanced = owner.message();
    assert_ne!(advanced["head"], initial["head"]);
    assert_eq!(
        std::fs::read_to_string(locator).unwrap().trim(),
        advanced["head"].as_str().unwrap()
    );
    owner.release();
}

/// Grounds RAII release in an actual child exit before a different process
/// restores and appends to the same run.
#[test]
fn clean_process_exit_releases_run_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let (mut process, initial) = owner(directory.path());
    process.release();
    restore_after_release(directory.path(), &initial);
}

/// Grounds crash recovery in forced process termination: no Rust destructor or
/// cleanup of a persistent lock file may be required to acquire ownership again.
#[test]
fn killed_process_releases_run_ownership_without_changing_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let (mut process, initial) = owner(directory.path());
    let locator = checkpoint(directory.path(), &initial);
    let before = evidence(directory.path(), &locator);
    process.child.kill().unwrap();
    assert!(!process.wait().success());
    assert_eq!(evidence(directory.path(), &locator), before);
    restore_after_release(directory.path(), &initial);
}

/// Grounds run-scoped ownership in overlapping child lifetimes sharing one
/// real store: owning one run must not serialize unrelated sessions.
#[test]
fn distinct_runs_remain_writable_in_overlapping_processes() {
    let directory = tempfile::tempdir().unwrap();
    let (mut first, initial_first) = owner(directory.path());
    let (mut second, initial_second) = owner(directory.path());
    assert_ne!(initial_first["run"], initial_second["run"]);
    for (process, initial) in [(&mut first, &initial_first), (&mut second, &initial_second)] {
        process.command("advance");
        let advanced = process.message();
        assert_ne!(advanced["head"], initial["head"]);
        assert_eq!(
            std::fs::read_to_string(checkpoint(directory.path(), initial))
                .unwrap()
                .trim(),
            advanced["head"].as_str().unwrap()
        );
        process.release();
    }
}
