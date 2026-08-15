//! The Herdr transport: newline-delimited JSON-RPC over the pane's unix
//! socket.
//!
//! This is the piece folded in from the competing detect-and-report work
//! (#1664) — a direct socket conversation with the Herdr server, instead of
//! forking the `herdr` CLI once per lifecycle change. Same protocol, no
//! process spawn: nothing here appears in the spawn inventory.
//!
//! Every operation is bounded, because this runs on the reporter worker and a
//! worker wedged forever is an integration that never recovers:
//!
//! - **Connect** is attempted on a helper thread and awaited for at most
//!   [`CONNECT_WAIT`]. A connect that hangs (a Herdr alive enough to have a
//!   socket but not to accept) leaves the attempt in flight; the sink reports
//!   failure immediately and collects the result on a later call. At most one
//!   attempt is ever outstanding, so a permanently hung Herdr costs one parked
//!   thread, not one per event.
//! - **Write** is bounded by [`IO_TIMEOUT`] via the socket's send timeout. A
//!   Herdr that stops reading fails the write instead of blocking; the
//!   connection is retired and the state redelivered on the next attempt.
//! - **The response is never waited for.** Herdr answers every request with a
//!   line; we drain whatever has already arrived, non-blockingly, purely so
//!   the server's send buffer cannot fill. A response that never arrives costs
//!   nothing.
//!
//! None of these bounds is what protects the *agent* — the agent never touches
//! this file (see `super`'s bounded queue). They protect the integration's
//! ability to recover.

use super::protocol::Call;

/// How long the worker waits for a connect before giving up on this event.
#[cfg(unix)]
const CONNECT_WAIT: std::time::Duration = std::time::Duration::from_millis(200);
/// Socket write timeout — the bound on "Herdr stopped reading".
#[cfg(unix)]
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Where lifecycle calls go. The adapter owns one; tests inject fakes.
pub(crate) trait Sink: Send {
    /// Deliver one call. `true` means it reached Herdr.
    fn deliver(&mut self, call: &Call) -> bool;
}

#[cfg(unix)]
pub(crate) use unix::SocketSink;

#[cfg(unix)]
mod unix {
    use super::{Call, Sink, CONNECT_WAIT, IO_TIMEOUT};
    use std::io::{ErrorKind, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
    use std::sync::Arc;

    /// Open a connection to a Herdr API socket — the default connector.
    pub(super) fn connect_socket(path: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
        std::os::unix::net::UnixStream::connect(path)
    }

    /// The connect seam: production opens a real socket; tests inject one that
    /// hangs, fails, or succeeds on demand. Shared with the helper thread, so
    /// it must be `Sync`.
    pub(super) type Connector =
        Arc<dyn Fn(&Path) -> std::io::Result<std::os::unix::net::UnixStream> + Send + Sync>;

    /// A reconnecting, bounded JSON-RPC sink over the pane socket.
    pub(crate) struct SocketSink {
        path: PathBuf,
        connect: Connector,
        conn: Option<std::os::unix::net::UnixStream>,
        /// An attempt whose deadline expired while still connecting. Polled
        /// before starting another, so hung connects cannot pile up.
        pending: Option<Receiver<std::io::Result<std::os::unix::net::UnixStream>>>,
        next_id: u64,
    }

    impl SocketSink {
        pub(crate) fn new(path: PathBuf) -> Self {
            Self::with_connector(path, Arc::new(connect_socket))
        }

        pub(super) fn with_connector(path: PathBuf, connect: Connector) -> Self {
            Self {
                path,
                connect,
                conn: None,
                pending: None,
                next_id: 0,
            }
        }

        /// A live connection, or `None` — never a wait longer than
        /// [`CONNECT_WAIT`].
        fn connection(&mut self) -> Option<&mut std::os::unix::net::UnixStream> {
            if self.conn.is_none() {
                self.reconnect();
            }
            self.conn.as_mut()
        }

        fn reconnect(&mut self) {
            // An attempt from an earlier event may have landed by now.
            if let Some(rx) = self.pending.take() {
                match rx.try_recv() {
                    Ok(Ok(stream)) => self.conn = Some(stream),
                    Ok(Err(_)) | Err(TryRecvError::Disconnected) => {}
                    // Still hanging: keep waiting on it, start nothing new.
                    Err(TryRecvError::Empty) => self.pending = Some(rx),
                }
                return;
            }
            // Cheap negative: no socket file means Herdr is not listening.
            // Skips the helper thread entirely for the common absent case.
            if !self.path.exists() {
                return;
            }
            let (tx, rx) = mpsc::channel();
            let connect = Arc::clone(&self.connect);
            let path = self.path.clone();
            if std::thread::Builder::new()
                .name("herdr-connect".into())
                .spawn(move || {
                    // The receiver may be gone (worker shut down, or the
                    // deadline passed and a later attempt superseded this
                    // one); that is exactly the abandoned-attempt case.
                    let _ = tx.send(connect(&path));
                })
                .is_err()
            {
                return;
            }
            match rx.recv_timeout(CONNECT_WAIT) {
                Ok(Ok(stream)) => self.conn = Some(stream),
                Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => {}
                // Hung or merely slow: abandon the wait, keep the receiver.
                Err(RecvTimeoutError::Timeout) => self.pending = Some(rx),
            }
        }

        /// Read whatever response bytes have already arrived and discard them,
        /// without ever waiting. `false` means the peer is gone.
        fn drain_responses(stream: &mut std::os::unix::net::UnixStream) -> bool {
            if stream.set_nonblocking(true).is_err() {
                return false;
            }
            let mut buf = [0u8; 1024];
            let alive = loop {
                match stream.read(&mut buf) {
                    Ok(0) => break false, // peer closed
                    Ok(_) => continue,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break true,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break false,
                }
            };
            alive && stream.set_nonblocking(false).is_ok()
        }
    }

    impl Sink for SocketSink {
        fn deliver(&mut self, call: &Call) -> bool {
            let Some(line) = call.encode(self.next_id) else {
                return false;
            };
            self.next_id = self.next_id.wrapping_add(1);
            let Some(stream) = self.connection() else {
                return false;
            };
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
            let wrote = stream
                .write_all(&line)
                .and_then(|()| stream.flush())
                .is_ok();
            let alive = wrote && Self::drain_responses(stream);
            if !alive {
                // Any failure retires the connection; the next call reconnects
                // and the state machine redelivers what went undelivered.
                self.conn = None;
            }
            alive
        }
    }
}

#[cfg(not(unix))]
pub(crate) use fallback::SocketSink;

// ---------------------------------------------------------------------------
// CLI fallback sink (HERDR_BIN_PATH)
// ---------------------------------------------------------------------------
//
// The socket is the preferred transport, but it only exists on unix. Where it
// does not — Windows, or a pane whose socket path is absent — an explicit
// `HERDR_BIN_PATH` pointing at the `herdr` binary keeps the integration alive:
// each call is one short-lived `herdr pane <verb>` process. That is a process
// spawn per lifecycle change, so this is strictly a fallback, never the
// default; the reporter worker absorbs the spawn cost, never the agent loop.
//
// Fail-open is preserved: a missing/exiting-nonzero binary just returns
// `false`, and the state machine redelivers on the next event.
pub(crate) mod cli {
    use super::{Call, Sink};
    use std::path::PathBuf;
    use std::process::Command;

    /// Delivers calls by invoking the `herdr` CLI from `HERDR_BIN_PATH`.
    pub(crate) struct CliSink {
        bin: PathBuf,
    }

    impl CliSink {
        pub(crate) fn new(bin: PathBuf) -> Self {
            Self { bin }
        }

        /// Build the argv for one call. Returns `None` for a method the CLI
        /// has no typed subcommand for (unknown future methods fail open).
        fn argv(call: &Call) -> Option<Vec<String>> {
            let p = &call.params;
            let pane = p.get("pane_id")?.as_str()?.to_string();
            let mut v = vec!["pane".to_string()];
            match call.method {
                "pane.report_agent" => {
                    v.push("report-agent".into());
                    v.push(pane);
                    v.push("--source".into());
                    v.push(p.get("source")?.as_str()?.to_string());
                    v.push("--agent".into());
                    v.push(p.get("agent")?.as_str()?.to_string());
                    v.push("--state".into());
                    v.push(p.get("state")?.as_str()?.to_string());
                    if let Some(m) = p.get("message").and_then(|m| m.as_str()) {
                        v.push("--message".into());
                        v.push(m.to_string());
                    }
                }
                "pane.report_agent_session" => {
                    v.push("report-agent-session".into());
                    v.push(pane);
                    v.push("--source".into());
                    v.push(p.get("source")?.as_str()?.to_string());
                    v.push("--agent".into());
                    v.push(p.get("agent")?.as_str()?.to_string());
                    if let Some(id) = p.get("agent_session_id").and_then(|s| s.as_str()) {
                        v.push("--agent-session-id".into());
                        v.push(id.to_string());
                    }
                    if let Some(path) = p.get("agent_session_path").and_then(|s| s.as_str()) {
                        v.push("--agent-session-path".into());
                        v.push(path.to_string());
                    }
                }
                "pane.release_agent" => {
                    v.push("release-agent".into());
                    v.push(pane);
                }
                // report_metadata_title and anything else: no CLI verb → skip.
                _ => return None,
            }
            Some(v)
        }
    }

    impl Sink for CliSink {
        fn deliver(&mut self, call: &Call) -> bool {
            let Some(argv) = Self::argv(call) else {
                return false;
            };
            // No stdin, no captured stdout: herdr prints nothing we need, and
            // a null stdin keeps the child from ever waiting on us.
            Command::new(&self.bin)
                .args(&argv)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::super::protocol::{release_agent, report_agent, PaneAgentState};
        use super::*;

        #[test]
        fn report_agent_maps_to_the_typed_cli_verb() {
            let call = report_agent("w1:p2", PaneAgentState::Working, Some("read_file"));
            let argv = CliSink::argv(&call).unwrap();
            assert_eq!(
                argv,
                vec![
                    "pane",
                    "report-agent",
                    "w1:p2",
                    "--source",
                    "custom:newt",
                    "--agent",
                    "newt",
                    "--state",
                    "working",
                    "--message",
                    "read_file",
                ]
            );
        }

        #[test]
        fn release_agent_maps_to_the_typed_cli_verb() {
            let call = release_agent("w1:p2");
            let argv = CliSink::argv(&call).unwrap();
            assert_eq!(argv, vec!["pane", "release-agent", "w1:p2"]);
        }

        #[test]
        fn a_method_with_no_cli_verb_fails_open() {
            // report_metadata has no CLI subcommand; argv is None and deliver
            // returns false rather than inventing an invocation.
            let call = super::super::super::protocol::report_metadata_title("w1:p2", "title");
            assert!(CliSink::argv(&call).is_none());
        }

        #[test]
        fn a_missing_binary_delivers_false_not_panic() {
            let mut sink = CliSink::new(PathBuf::from("/no/such/herdr-binary"));
            let call = release_agent("w1:p2");
            assert!(!sink.deliver(&call));
        }
    }
}

#[cfg(not(unix))]
mod fallback {
    use super::{Call, Sink};
    use std::path::PathBuf;

    /// Herdr panes speak a unix socket; elsewhere the integration is simply
    /// unavailable and every delivery fails silently.
    pub(crate) struct SocketSink;

    impl SocketSink {
        pub(crate) fn new(_path: PathBuf) -> Self {
            Self
        }
    }

    impl Sink for SocketSink {
        fn deliver(&mut self, _call: &Call) -> bool {
            false
        }
    }
}

/// Real-resource tier (see CLAUDE.md "Testing strategy"): these drive an
/// actual unix socket, because "a hung Herdr cannot wedge the reporter" is a
/// property of real sockets and real connect/write semantics — no mock can
/// observe it. They GROUND the fully-mocked adapter tests in `super`, which
/// stand in a `FakeSink` for this file. Serialized onto one lane
/// (`real_fs`), matching the rest of this crate's real-resource tests.
#[cfg(all(test, unix))]
mod tests {
    use super::unix::{Connector, SocketSink};
    use super::*;
    use crate::herdr::protocol::{report_agent, PaneAgentState};
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "newt-herdr-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn working() -> Call {
        report_agent("w1:p2", PaneAgentState::Working, None)
    }

    /// Deliver, retrying a cold connect. A first attempt can miss its 200 ms
    /// deadline on a loaded runner; the sink collects that attempt on a later
    /// call, and a failed attempt with no connection writes nothing — so a
    /// retry cannot duplicate a frame.
    fn deliver_eventually(sink: &mut SocketSink, call: &Call) -> bool {
        (0..50).any(|i| {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
            sink.deliver(call)
        })
    }

    // Herdr absent (no socket file at all): delivery fails immediately and
    // without spawning anything.
    #[serial_test::serial(real_fs)]
    #[test]
    fn a_missing_socket_fails_fast() {
        let dir = scratch("missing");
        let mut sink = SocketSink::new(dir.join("nope.sock"));
        let t0 = Instant::now();
        assert!(!sink.deliver(&working()));
        assert!(!sink.deliver(&working()));
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "absent Herdr must not cost a connect attempt: {:?}",
            t0.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The path exists but is not a socket (a stale file, a directory): connect
    // errors, delivery fails, nothing hangs.
    #[serial_test::serial(real_fs)]
    #[test]
    fn a_path_that_is_not_a_socket_fails() {
        let dir = scratch("notasocket");
        let path = dir.join("api.sock");
        std::fs::write(&path, b"not a socket").unwrap();
        let mut sink = SocketSink::new(path);
        assert!(!sink.deliver(&working()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The happy path, end to end over a real unix socket: Herdr receives one
    // JSON-RPC line per call, and the connection is REUSED across calls (one
    // accept, two frames).
    #[serial_test::serial(real_fs)]
    #[test]
    fn calls_are_framed_over_one_reused_connection() {
        let dir = scratch("roundtrip");
        let path = dir.join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn);
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // client hung up
                    Ok(_) => lines.push(line),
                }
            }
            lines
        });

        let mut sink = SocketSink::new(path);
        assert!(deliver_eventually(
            &mut sink,
            &report_agent("w1:p2", PaneAgentState::Working, Some("read_file"))
        ));
        // The connection is warm now, so this one must land on the first try —
        // that is the reuse claim.
        assert!(
            sink.deliver(&report_agent("w1:p2", PaneAgentState::Idle, None)),
            "a warm connection must be reused, not reconnected"
        );
        drop(sink); // EOF for the server

        let lines = server.join().unwrap();
        assert_eq!(lines.len(), 2, "both calls arrive on the same connection");
        let first: serde_json::Value = serde_json::from_str(lines[0].trim()).unwrap();
        assert_eq!(first["method"], "pane.report_agent");
        assert_eq!(first["params"]["state"], "working");
        assert_eq!(first["params"]["message"], "read_file");
        let second: serde_json::Value = serde_json::from_str(lines[1].trim()).unwrap();
        assert_eq!(second["params"]["state"], "idle");
        assert_ne!(first["id"], second["id"], "ids are per-call");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Herdr accepts but never answers: we do not wait for a response, so
    // delivery succeeds promptly. (A never-arriving reply is free.)
    #[serial_test::serial(real_fs)]
    #[test]
    fn a_silent_server_does_not_stall_delivery() {
        let dir = scratch("silent");
        let path = dir.join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            // Stay connected and silent for longer than the test needs, so a
            // slow cold connect cannot turn into a closed-peer race.
            std::thread::sleep(Duration::from_millis(1500));
            drop(conn);
        });
        let mut sink = SocketSink::new(path);
        assert!(deliver_eventually(&mut sink, &working()));
        // Warm connection, server still silent: delivery is immediate because
        // the reply is never awaited.
        let t0 = Instant::now();
        assert!(sink.deliver(&working()));
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "no response wait: {:?}",
            t0.elapsed()
        );
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Herdr dies mid-session: the failed delivery is reported as failed (so
    // the caller can redeliver) and the dead connection is retired rather than
    // reused forever.
    #[serial_test::serial(real_fs)]
    #[test]
    fn a_closed_peer_fails_and_retires_the_connection() {
        let dir = scratch("closed");
        let path = dir.join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            drop(conn); // hang up immediately
            drop(listener); // …and stop listening, so Herdr is truly gone
        });
        let mut sink = SocketSink::new(path);
        // The first call races the hangup: the write may land in the socket
        // buffer before the peer's close is observable. What must NOT happen
        // is a permanently poisoned sink. (Delivered through the retry helper
        // so the server's `accept` definitely happens and its join returns.)
        let _ = deliver_eventually(&mut sink, &working());
        server.join().unwrap();
        assert!(
            !sink.deliver(&working()),
            "a hung-up peer must not be reported as delivered"
        );
        assert!(
            !sink.deliver(&working()),
            "and the dead connection must have been retired, not reused"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A connect that never returns: the sink gives up after CONNECT_WAIT,
    // keeps exactly ONE attempt outstanding no matter how many calls follow,
    // and recovers if that attempt ever lands.
    #[serial_test::serial(real_fs)]
    #[test]
    fn a_hanging_connect_is_abandoned_and_never_duplicated() {
        let dir = scratch("hang");
        let path = dir.join("api.sock");
        // A real socket file must exist for the sink to bother connecting.
        let _listener = UnixListener::bind(&path).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let release = Arc::new(std::sync::Mutex::new(()));
        let held = release.lock().unwrap();
        let gate = Arc::clone(&release);
        // Keep the far end of the socketpair alive, or the "connection" would
        // be born already hung up and prove nothing.
        let peers: Arc<std::sync::Mutex<Vec<UnixStream>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let peer_sink = Arc::clone(&peers);
        let connect: Connector = Arc::new(move |_p: &std::path::Path| {
            counter.fetch_add(1, Ordering::SeqCst);
            let _wait = gate.lock().unwrap(); // blocks until the test releases
            let (near, far) = UnixStream::pair()?;
            peer_sink.lock().unwrap().push(far);
            Ok(near)
        });
        let mut sink = SocketSink::with_connector(path, connect);

        let t0 = Instant::now();
        assert!(!sink.deliver(&working()));
        let first = t0.elapsed();
        assert!(
            first >= CONNECT_WAIT && first < CONNECT_WAIT * 10,
            "the first attempt waits about one CONNECT_WAIT: {first:?}"
        );
        for _ in 0..5 {
            assert!(!sink.deliver(&working()));
        }
        assert!(
            t0.elapsed() < CONNECT_WAIT * 12,
            "later calls must not each pay the wait: {:?}",
            t0.elapsed()
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "exactly one connect attempt stays outstanding"
        );
        drop(held); // the hung connect completes
        let collected = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            sink.deliver(&working())
        });
        assert!(collected, "the abandoned attempt is collected, not leaked");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
