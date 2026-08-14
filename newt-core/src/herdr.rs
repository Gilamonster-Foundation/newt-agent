//! Optional bridge to a [herdr](https://github.com/herdrdev/herdr) cockpit pane.
//!
//! When a `newt-agent` process is launched *inside* a herdr pane, herdr exports
//! four environment variables into the process. This module detects that and
//! reports the agent's own state back to herdr over the pane's unix JSON-RPC
//! socket, so newt shows up as a first-class agent in the cockpit UI (working /
//! idle / blocked, tab title, session id) — exactly like the existing
//! claude/codex/qwen integrations.
//!
//! **herdr is strictly optional.** When any of the marker variables is absent,
//! [`HerdrContext::detect`] returns `None` and every interaction becomes a
//! no-op, so a non-herdr run is byte-for-byte identical to a build without this
//! module. Reporting is fire-and-forget with a short timeout and never blocks
//! or fails the agent loop.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// `source` field on every report — the `herdr:<agent>` convention used by all
/// of herdr's bundled integration scripts.
const SOURCE: &str = "herdr:newt";
/// `agent` field on every report.
const AGENT: &str = "newt";

/// The agentic state a pane reports, mirroring herdr's `PaneAgentState`
/// (`herdr/src/api/schema/common.rs`). Serialised `snake_case` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAgentState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl PaneAgentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Unknown => "unknown",
        }
    }
}

/// Why a session is (re)starting, mirroring herdr's `session_start_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
    Branch,
}

impl SessionStartSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
            Self::Compact => "compact",
            Self::Branch => "branch",
        }
    }
}

/// A detected herdr pane this process is running inside.
///
/// Cheap to construct and `Clone`; detect once at startup and share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrContext {
    /// The pane's stable id — the target of every `pane.*` call.
    pub pane_id: String,
    /// Unix socket the herdr JSON-RPC API listens on.
    pub socket_path: PathBuf,
    /// Optional path to the `herdr` CLI binary.
    pub bin_path: Option<PathBuf>,
}

impl HerdrContext {
    /// Detect whether this process is running inside a herdr pane.
    ///
    /// Returns `Some(..)` only when herdr's full marker set is present
    /// (`HERDR_ENV=1`, a non-empty `HERDR_PANE_ID`, and `HERDR_SOCKET_PATH`).
    /// Any missing piece ⇒ `None` ⇒ herdr is treated as absent.
    #[must_use]
    pub fn detect() -> Option<Self> {
        if std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
            return None;
        }
        let pane_id = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|s| !s.is_empty())?;
        let socket_path = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from)?;
        let bin_path = std::env::var_os("HERDR_BIN_PATH").map(PathBuf::from);
        Some(Self {
            pane_id,
            socket_path,
            bin_path,
        })
    }

    /// `true` when running inside a herdr pane (convenience for `detect().is_some()`).
    #[must_use]
    pub fn present() -> bool {
        Self::detect().is_some()
    }

    /// Monotonic-ish sequence number herdr uses to drop stale reports.
    fn seq() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// Send one JSON-RPC request line and drop the connection.
    ///
    /// Fire-and-forget: opens the socket, writes one newline-terminated JSON
    /// line, optionally drains a single reply frame, and returns. Every error
    /// is swallowed — a missing/busy herdr must never stall the agent loop.
    fn send(&self, method: &str, params: serde_json::Value) {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;
            let request = serde_json::json!({
                "id": format!("{}:{}", SOURCE, Self::seq()),
                "method": method,
                "params": params,
            });
            let mut line = match serde_json::to_vec(&request) {
                Ok(bytes) => bytes,
                Err(_) => return,
            };
            line.push(b'\n');
            let Ok(mut stream) = UnixStream::connect(&self.socket_path) else {
                return;
            };
            let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(500)));
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            let _ = stream.write_all(&line);
            // Drain one reply frame so the server's send buffer doesn't stall,
            // but never block on it.
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
        }
        #[cfg(not(unix))]
        {
            let _ = (method, params); // herdr panes are unix-socket only.
        }
    }

    /// Report the agent's live state (`pane.report_agent`).
    pub fn report_state(&self, state: PaneAgentState, message: Option<&str>) {
        let mut params = serde_json::json!({
            "pane_id": self.pane_id,
            "source": SOURCE,
            "agent": AGENT,
            "state": state.as_str(),
            "seq": Self::seq(),
        });
        if let Some(m) = message {
            params["message"] = serde_json::Value::String(m.to_string());
        }
        self.send("pane.report_agent", params);
    }

    /// Report a session (re)start (`pane.report_agent_session`).
    pub fn report_session_start(&self, session_id: &str, source: SessionStartSource) {
        let params = serde_json::json!({
            "pane_id": self.pane_id,
            "source": SOURCE,
            "agent": AGENT,
            "agent_session_id": session_id,
            "session_start_source": source.as_str(),
            "seq": Self::seq(),
        });
        self.send("pane.report_agent_session", params);
    }

    /// Set the pane's tab title and/or a single state badge (`pane.report_metadata`).
    pub fn report_metadata(&self, title: Option<&str>, state_label: Option<(&str, &str)>) {
        let mut params = serde_json::json!({
            "pane_id": self.pane_id,
            "source": SOURCE,
            "agent": AGENT,
            "seq": Self::seq(),
        });
        if let Some(t) = title {
            params["title"] = serde_json::Value::String(t.to_string());
        }
        if let Some((key, value)) = state_label {
            params["state_labels"] = serde_json::json!({ key: value });
        }
        self.send("pane.report_metadata", params);
    }

    /// Convenience: mark the pane Working with an optional status line.
    pub fn mark_working(&self, message: Option<&str>) {
        self.report_state(PaneAgentState::Working, message);
    }

    /// Convenience: mark the pane Idle (awaiting operator input).
    pub fn mark_idle(&self) {
        self.report_state(PaneAgentState::Idle, None);
    }

    /// Convenience: mark the pane Blocked (awaiting a decision/permission).
    pub fn mark_blocked(&self, message: Option<&str>) {
        self.report_state(PaneAgentState::Blocked, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-mutating tests must not run in parallel with each other; the
    // process-wide guard serialises them.
    fn with_herdr_env<R>(f: impl FnOnce() -> R) -> R {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        std::env::set_var("HERDR_ENV", "1");
        std::env::set_var("HERDR_PANE_ID", "pane-42");
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/herdr-test.sock");
        let r = f();
        std::env::remove_var("HERDR_ENV");
        std::env::remove_var("HERDR_PANE_ID");
        std::env::remove_var("HERDR_SOCKET_PATH");
        std::env::remove_var("HERDR_BIN_PATH");
        r
    }

    #[test]
    fn detect_returns_none_without_herdr_env() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        std::env::remove_var("HERDR_ENV");
        std::env::remove_var("HERDR_PANE_ID");
        std::env::remove_var("HERDR_SOCKET_PATH");
        assert!(HerdrContext::detect().is_none());
    }

    #[test]
    fn detect_returns_none_when_env_var_not_one() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        std::env::set_var("HERDR_ENV", "0");
        std::env::set_var("HERDR_PANE_ID", "pane-42");
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/herdr-test.sock");
        assert!(HerdrContext::detect().is_none());
        std::env::remove_var("HERDR_ENV");
        std::env::remove_var("HERDR_PANE_ID");
        std::env::remove_var("HERDR_SOCKET_PATH");
    }

    #[test]
    fn detect_requires_all_markers() {
        let _g = crate::test_guard::GlobalSettingsGuard::acquire();
        std::env::set_var("HERDR_ENV", "1");
        std::env::remove_var("HERDR_PANE_ID");
        std::env::set_var("HERDR_SOCKET_PATH", "/tmp/herdr-test.sock");
        assert!(HerdrContext::detect().is_none());
        std::env::remove_var("HERDR_ENV");
        std::env::remove_var("HERDR_SOCKET_PATH");
    }

    #[test]
    fn detect_builds_context_when_present() {
        with_herdr_env(|| {
            let ctx = HerdrContext::detect().expect("present");
            assert_eq!(ctx.pane_id, "pane-42");
            assert_eq!(ctx.socket_path, PathBuf::from("/tmp/herdr-test.sock"));
        });
    }

    #[test]
    fn state_and_source_serialise_snake_case() {
        assert_eq!(PaneAgentState::Working.as_str(), "working");
        assert_eq!(PaneAgentState::Blocked.as_str(), "blocked");
        assert_eq!(SessionStartSource::Startup.as_str(), "startup");
        assert_eq!(SessionStartSource::Compact.as_str(), "compact");
    }

    #[cfg(unix)]
    #[test]
    fn report_emits_jsonrpc_line_over_socket() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        with_herdr_env(|| {
            let dir = std::env::temp_dir().join(format!("herdr-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let sock = dir.join("api.sock");
            let _ = std::fs::remove_file(&sock);
            let listener = UnixListener::bind(&sock).unwrap();

            let mut ctx = HerdrContext::detect().expect("present");
            ctx.socket_path = sock.clone();

            let handle = std::thread::spawn(move || {
                let (mut conn, _) = listener.accept().unwrap();
                // Read raw bytes up to the newline, then decode as UTF-8 so
                // multi-byte characters (e.g. the ellipsis) survive intact.
                let mut bytes: Vec<u8> = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    match conn.read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => {
                            bytes.push(byte[0]);
                            if byte[0] == b'\n' {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                String::from_utf8(bytes).unwrap()
            });

            ctx.report_state(PaneAgentState::Working, Some("thinking…"));
            let line = handle.join().unwrap();
            let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(parsed["method"], "pane.report_agent");
            assert_eq!(parsed["params"]["pane_id"], "pane-42");
            assert_eq!(parsed["params"]["source"], "herdr:newt");
            assert_eq!(parsed["params"]["agent"], "newt");
            assert_eq!(parsed["params"]["state"], "working");
            assert_eq!(parsed["params"]["message"], "thinking…");

            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
