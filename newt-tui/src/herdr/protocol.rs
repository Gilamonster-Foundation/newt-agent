//! The Herdr JSON-RPC wire vocabulary — request construction only.
//!
//! Herdr's API is newline-delimited JSON-RPC over a unix socket: one request
//! object per line, one response line back. The request shape is
//! `{"id": …, "method": "pane.…", "params": {…}}`, mirroring Herdr's own
//! `Request`/`Method` schema (`herdr/src/api/schema.rs`).
//!
//! Two deliberate choices:
//!
//! - **Source is `custom:newt`.** The `herdr:` prefix belongs to Herdr's own
//!   bundled integrations, several of which its pane state machine
//!   special-cases by exact `(source, agent)` pair. newt is a third party and
//!   says so.
//! - **No `seq`.** Herdr's `seq` is optional and exists to discard reports
//!   that arrive out of order. Delivery here is strictly serialized by a
//!   single worker, so it would order nothing — while re-introducing the
//!   restart hazard where a fresh process's counter starts below the
//!   high-water mark Herdr may still remember for this pane and source. Every
//!   process generation emits identical, generation-independent reports.

use serde_json::{json, Value};

/// Report source. Herdr accepts `[A-Za-z0-9:._-]{1,80}`.
pub(crate) const SOURCE: &str = "custom:newt";
/// Agent label. Report and release MUST carry the same identity triple
/// (pane, source, agent) or Herdr will not correlate them.
pub(crate) const AGENT: &str = "newt";

/// Longest status message we will put on the wire. Messages are derived from
/// validated tool names and fixed strings, never from model prose.
const MAX_MESSAGE_CHARS: usize = 40;

/// The agentic state a pane reports — Herdr's `PaneAgentState`, minus
/// `unknown` (newt always knows which of the three it is).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneAgentState {
    Idle,
    Working,
    Blocked,
}

impl PaneAgentState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// Why a session is (re)starting — Herdr's `session_start_source`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionStartSource {
    Startup,
}

impl SessionStartSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
        }
    }
}

/// One JSON-RPC call: a method and its params, ready to frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Call {
    pub(crate) method: &'static str,
    pub(crate) params: Value,
}

impl Call {
    /// Frame this call as one newline-terminated request line.
    ///
    /// `id` is echoed by Herdr in its response; we never read it, but the
    /// field is mandatory in the schema.
    ///
    /// Only the unix transport frames anything — Herdr panes are unix-socket
    /// only — but the wire format stays platform-independent so its tests run
    /// everywhere.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn encode(&self, id: u64) -> Option<Vec<u8>> {
        let request = json!({
            "id": format!("{SOURCE}:{id}"),
            "method": self.method,
            "params": self.params,
        });
        let mut line = serde_json::to_vec(&request).ok()?;
        line.push(b'\n');
        Some(line)
    }
}

/// Keep a status message to an identifier-shaped, bounded token. Tool names
/// arrive already validated against the tool catalog; this is the second
/// fence, so nothing model-authored can shape a pane's UI.
///
/// Control characters, escapes and punctuation are dropped, and what survives
/// must still *begin like a name* — otherwise the residue of a stripped escape
/// sequence (`\x1b[31m` leaving `31m`) would reach the cockpit as a label.
fn sanitize_message(message: &str) -> Option<String> {
    let cleaned: String = message
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
        .take(MAX_MESSAGE_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_string();
    cleaned
        .starts_with(|c: char| c.is_ascii_alphabetic())
        .then_some(cleaned)
}

/// `pane.report_agent` — the live state of this pane's agent.
pub(crate) fn report_agent(pane: &str, state: PaneAgentState, message: Option<&str>) -> Call {
    let mut params = json!({
        "pane_id": pane,
        "source": SOURCE,
        "agent": AGENT,
        "state": state.as_str(),
    });
    if let Some(message) = message.and_then(sanitize_message) {
        params["message"] = Value::String(message);
    }
    Call {
        method: "pane.report_agent",
        params,
    }
}

/// `pane.report_agent_session` — this pane's agent session identity.
pub(crate) fn report_agent_session(
    pane: &str,
    session_id: &str,
    start: SessionStartSource,
) -> Call {
    Call {
        method: "pane.report_agent_session",
        params: json!({
            "pane_id": pane,
            "source": SOURCE,
            "agent": AGENT,
            "agent_session_id": session_id,
            "session_start_source": start.as_str(),
        }),
    }
}

/// `pane.report_metadata` — the pane's tab title.
pub(crate) fn report_metadata_title(pane: &str, title: &str) -> Call {
    Call {
        method: "pane.report_metadata",
        params: json!({
            "pane_id": pane,
            "source": SOURCE,
            "agent": AGENT,
            "title": title,
        }),
    }
}

/// `pane.release_agent` — give up lifecycle authority for this pane.
pub(crate) fn release_agent(pane: &str) -> Call {
    Call {
        method: "pane.release_agent",
        params: json!({
            "pane_id": pane,
            "source": SOURCE,
            "agent": AGENT,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The wire shape is Herdr's: one JSON object per line, `id`/`method`/
    // `params`, snake_case state, and the identity triple on every call.
    #[test]
    fn encoded_request_is_one_json_line_in_herdr_shape() {
        let line = report_agent("w1:p2", PaneAgentState::Working, Some("read_file"))
            .encode(7)
            .unwrap();
        assert_eq!(*line.last().unwrap(), b'\n');
        assert_eq!(
            line.iter().filter(|b| **b == b'\n').count(),
            1,
            "exactly one frame per call"
        );
        let parsed: Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(parsed["id"], "custom:newt:7");
        assert_eq!(parsed["method"], "pane.report_agent");
        assert_eq!(parsed["params"]["pane_id"], "w1:p2");
        assert_eq!(parsed["params"]["source"], "custom:newt");
        assert_eq!(parsed["params"]["agent"], "newt");
        assert_eq!(parsed["params"]["state"], "working");
        assert_eq!(parsed["params"]["message"], "read_file");
    }

    // Report and release must correlate: same pane, source, agent.
    #[test]
    fn release_identity_matches_report_identity() {
        let identity = |call: &Call| {
            (
                call.params["pane_id"].clone(),
                call.params["source"].clone(),
                call.params["agent"].clone(),
            )
        };
        assert_eq!(
            identity(&report_agent("w1:p2", PaneAgentState::Idle, None)),
            identity(&release_agent("w1:p2"))
        );
    }

    // No call carries `seq`: reports are generation-independent, so a restart
    // in the same pane can never be rejected by a stale high-water mark.
    #[test]
    fn no_call_carries_a_sequence_number() {
        for call in [
            report_agent("p", PaneAgentState::Working, None),
            report_agent_session("p", "s", SessionStartSource::Startup),
            report_metadata_title("p", "repo"),
            release_agent("p"),
        ] {
            assert!(
                call.params.get("seq").is_none(),
                "{} must not carry seq",
                call.method
            );
        }
    }

    // A message is a bounded identifier-ish token — model-shaped text cannot
    // reach the cockpit UI.
    #[test]
    fn messages_are_sanitized_and_bounded() {
        assert_eq!(sanitize_message("read_file"), Some("read_file".into()));
        assert_eq!(
            sanitize_message("drop\ntable; rm -rf /"),
            Some("droptable rm -rf".into())
        );
        assert_eq!(sanitize_message("…"), None);
        assert_eq!(sanitize_message("  "), None);
        assert_eq!(
            sanitize_message("\u{1b}[31m"),
            None,
            "the residue of a stripped escape is not a name"
        );
        assert_eq!(sanitize_message(&"x".repeat(200)).unwrap().len(), 40);
        let call = report_agent("p", PaneAgentState::Working, Some("\u{1b}[31m"));
        assert!(
            call.params.get("message").is_none(),
            "an all-control-character message is omitted, not emitted empty"
        );
    }
}
