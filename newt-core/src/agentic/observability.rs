//! W0 of the output-based model-behavior ADR (epic #1506, issue #1511): the
//! TYPED signals `newt solve` needs to emit the observability-contract record
//! the external evaluator consumes.
//!
//! Two signal families live here, both STRUCTURAL (never string-matched):
//!
//! - **Dispatch-error classification** ([`ErrorClass`] / [`DispatchError`]):
//!   the contract's `outcome` taxonomy (`model_error` / `transport_error` /
//!   `timeout` / `harness_error`) is decided from the TYPED reqwest error at
//!   the send site (`is_connect` / `is_timeout` / …), wrapped in a
//!   [`DispatchError`] whose `Display` is byte-identical to the historical
//!   message (the retry layer and the tools-unsupported/cw-400 recoveries all
//!   read that text), and recovered at the driver boundary by walking the
//!   anyhow chain ([`error_class`]). Grepping error text for `"timeout"` is
//!   exactly the string heuristic the ADR retires.
//!
//! - **Tool-call parse status** ([`ParseSignal`] / [`round_parse_signal`]):
//!   per-round evidence for the evaluator's artifact-vs-weakness split
//!   (#1500) — `recovered_tool_call{dialect}` when `tool_recovery` fired
//!   (and which dialect matched), `no_parseable_tool_call` when a round
//!   produced content but neither native `tool_calls` nor a recovery hit.
//!   `reasoning_overflow` is deliberately ABSENT — that detection is W3
//!   (#1508), not W0.
//!
//! [`SolveObservation`] is the per-turn out-param bundle the loops fill (the
//! `tool_events` lending pattern): the parse signals plus the `model` field
//! the backend actually reported, so the contract's `effective_model` is the
//! served reality, not an echo of the request.

use serde::{Deserialize, Serialize};

/// Which content dialect `tool_recovery` matched. Pure provenance — the
/// recovery itself already ran; this names the shape for the trace (ADR §5)
/// so the evaluator can attribute recoveries per dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallDialect {
    /// `<function=NAME><parameter=K>V</parameter></function>` blocks.
    FunctionTag,
    /// `<TOOL><arg>value</arg></TOOL>` root tags (known built-ins only).
    RootTag,
    /// A bare or fenced `{"name": …, "arguments": …}` JSON object.
    BareJson,
}

/// One per-round tool-call parse observation, serialized as its own JSONL
/// trace line (`kind` is the ADR §5 event name). These lines carry no
/// `contract_version` key, so the external evaluator's contract scan skips
/// them structurally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParseSignal {
    /// The round produced content but no native tool call and no recovery hit
    /// — nothing actionable was parsed (a healthy final answer also lands
    /// here; the evaluator correlates with the terminal round).
    NoParseableToolCall { round: usize },
    /// `tool_recovery` turned content into executable call(s); `dialect` is
    /// the shape that matched.
    RecoveredToolCall {
        round: usize,
        dialect: ToolCallDialect,
    },
}

/// The parse-status decision for one probe round — pure so each signal is
/// unit-tested without a loop. `native_calls` = the wire `tool_calls` array
/// was non-empty; `recovered` = the dialect `tool_recovery` matched, if any.
pub fn round_parse_signal(
    round: usize,
    content_nonempty: bool,
    native_calls: bool,
    recovered: Option<ToolCallDialect>,
) -> Option<ParseSignal> {
    if let Some(dialect) = recovered {
        return Some(ParseSignal::RecoveredToolCall { round, dialect });
    }
    if !native_calls && content_nonempty {
        return Some(ParseSignal::NoParseableToolCall { round });
    }
    None
}

/// Per-turn observability out-params (#1511), lent by the headless driver the
/// way `tool_events` is: the loop fills it, the driver folds it into the
/// [`TurnOutcome`](super::TurnOutcome), `newt solve` serializes it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SolveObservation {
    /// The `model` field of the last chat response body, when the backend
    /// reported one — what the backend says it actually served, feeding the
    /// contract's `effective_model`. `None` when no response carried it.
    pub served_model: Option<String>,
    /// Per-round parse-status signals, in round order.
    pub parse_signals: Vec<ParseSignal>,
}

/// Structural classification of a failed turn — the contract `outcome`
/// taxonomy minus `completed` (a clean turn has no error to classify).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The backend was reached but errored: an HTTP error status, or a body
    /// that could not be decoded. A real attempt — it carries capability
    /// signal (the model/stack answered and answered badly).
    Model,
    /// The backend could not be reached: connection refused/reset, DNS,
    /// or a connect-phase timeout. NOT a real attempt.
    Transport,
    /// The request deadline elapsed AFTER the backend was reached (reqwest's
    /// total-request timeout — `inference_timeout_secs`).
    Timeout,
    /// The failure is on our side of the wire (a malformed request we built)
    /// — or, at the boundary, any error carrying no [`DispatchError`] at all.
    Harness,
}

/// Classify a typed reqwest error into the contract taxonomy. Order matters:
/// a connect-phase timeout reports BOTH `is_connect` and `is_timeout`, and
/// the contract files "could not reach the model" under `transport_error`,
/// so the connect check wins; a plain `is_timeout` is then the post-connect
/// request deadline. Everything else that isn't agent-side (`is_builder`) is
/// a wire-level failure mid-exchange → transport.
pub fn classify_reqwest(e: &reqwest::Error) -> ErrorClass {
    if e.is_connect() {
        return ErrorClass::Transport;
    }
    if e.is_timeout() {
        return ErrorClass::Timeout;
    }
    if e.is_status() || e.is_decode() || e.is_body() {
        return ErrorClass::Model;
    }
    if e.is_builder() {
        return ErrorClass::Harness;
    }
    ErrorClass::Transport
}

/// A dispatch failure with its structural class attached. Constructed at the
/// send site (where the reqwest error is still typed) and carried through the
/// anyhow chain so the driver boundary can read `class` without re-parsing
/// text. **`Display` is byte-identical to the strings these sites emitted
/// before** — `retry::classify`, `is_tools_unsupported_error`, and the cw-400
/// recovery all match on that text, and the trace/error surfaces keep their
/// wording.
#[derive(Debug)]
pub struct DispatchError {
    /// The structural class, decided from the typed source.
    pub class: ErrorClass,
    msg: String,
}

impl DispatchError {
    /// Wrap a reqwest send/decode failure, classifying it while it is still
    /// typed. `prefix` preserves the historical site wording (`"request
    /// failed"` on the probe, `"stream request failed"` on the re-issue).
    pub fn from_reqwest(prefix: &str, e: reqwest::Error) -> Self {
        Self {
            class: classify_reqwest(&e),
            msg: format!("{prefix}: {e}"),
        }
    }

    /// Wrap a non-success HTTP status: the backend was reached and answered
    /// — a `model_error` structurally, whatever the body text says. `msg` is
    /// the caller's fully-formatted historical string (`"Ollama {status}:
    /// {text}"` / `"inference endpoint {status}: {text}"`).
    pub fn http_status(msg: String) -> Self {
        Self {
            class: ErrorClass::Model,
            msg,
        }
    }
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for DispatchError {}

/// Recover the structural class from an anyhow chain at the driver boundary.
/// `None` means no [`DispatchError`] anywhere in the chain — the failure
/// happened outside a dispatch (the caller files it as `harness_error`,
/// fail-closed: an unattributed error must never masquerade as a model one).
pub fn error_class(e: &anyhow::Error) -> Option<ErrorClass> {
    e.chain()
        .find_map(|c| c.downcast_ref::<DispatchError>())
        .map(|d| d.class)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // --- outcome taxonomy: one classification test per class, from TYPED
    // errors (never message text) ---

    /// `timeout`: the deadline elapses AFTER the server is reached — reqwest
    /// reports `is_timeout` without `is_connect`. (wiremock + a short client
    /// timeout, the same local-socket pattern as `backend_probe`'s tests.)
    #[tokio::test]
    async fn request_deadline_after_connect_classifies_timeout() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(250)))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        let err = client.get(server.uri()).send().await.unwrap_err();
        assert_eq!(classify_reqwest(&err), ErrorClass::Timeout);
    }

    /// `transport_error`: connection refused — the model was never reached.
    /// A listener is bound then dropped so the port is local and closed (no
    /// external network; the socket dance mirrors wiremock's own).
    #[tokio::test]
    async fn connection_refused_classifies_transport() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
            // listener drops here — nothing accepts on `port` anymore
        };
        let err = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(classify_reqwest(&err), ErrorClass::Transport);
    }

    /// `model_error`: a non-success HTTP status means the backend answered —
    /// reached-but-errored, whatever the body says.
    #[test]
    fn http_status_classifies_model_error() {
        let e = DispatchError::http_status("Ollama 500 Internal Server Error: boom".into());
        assert_eq!(e.class, ErrorClass::Model);
        // Display is the historical wording, verbatim — the retry layer and
        // the recovery heuristics match on it.
        assert_eq!(e.to_string(), "Ollama 500 Internal Server Error: boom");
    }

    /// `harness_error` (builder side): a request WE built wrong is our
    /// failure, not the wire's and not the model's.
    #[tokio::test]
    async fn builder_error_classifies_harness() {
        // An empty URL fails in the builder before any socket is touched.
        let err = reqwest::Client::new().get("").send().await.unwrap_err();
        assert!(err.is_builder());
        assert_eq!(classify_reqwest(&err), ErrorClass::Harness);
    }

    /// The boundary recovery: the class survives an anyhow chain (with
    /// context wrapping), and an error with no `DispatchError` yields `None`
    /// — which the caller files as `harness_error`, fail-closed.
    #[test]
    fn error_class_walks_the_anyhow_chain() {
        let wrapped = anyhow::Error::new(DispatchError::http_status("Ollama 404: nope".into()))
            .context("round 3 dispatch");
        assert_eq!(error_class(&wrapped), Some(ErrorClass::Model));
        assert_eq!(error_class(&anyhow::anyhow!("spawn failed")), None);
    }

    /// `Display` parity for the reqwest wrapper: `"{prefix}: {e}"`, exactly
    /// the string `anyhow!("request failed: {e}")` produced before — the
    /// retry layer's `classify` greps for `"request failed"`.
    #[tokio::test]
    async fn from_reqwest_preserves_the_historical_message() {
        let err = reqwest::Client::new().get("").send().await.unwrap_err();
        let expect = format!("request failed: {err}");
        let d = DispatchError::from_reqwest("request failed", err);
        assert_eq!(d.to_string(), expect);
        assert!(d.to_string().contains("request failed"));
    }

    // --- parse-status signals: one test per signal ---

    #[test]
    fn content_without_any_call_signals_no_parseable_tool_call() {
        assert_eq!(
            round_parse_signal(2, true, false, None),
            Some(ParseSignal::NoParseableToolCall { round: 2 })
        );
    }

    #[test]
    fn recovery_hit_signals_recovered_tool_call_with_its_dialect() {
        assert_eq!(
            round_parse_signal(1, true, false, Some(ToolCallDialect::FunctionTag)),
            Some(ParseSignal::RecoveredToolCall {
                round: 1,
                dialect: ToolCallDialect::FunctionTag
            })
        );
    }

    #[test]
    fn healthy_native_call_and_empty_content_signal_nothing() {
        // A native structured call is the healthy channel — no signal.
        assert_eq!(round_parse_signal(0, true, true, None), None);
        // Empty content with no calls is the suspicious-empty case, not a
        // parse status (W3 territory) — no signal here either.
        assert_eq!(round_parse_signal(0, false, false, None), None);
    }

    /// The trace-line shape: `kind` carries the ADR §5 event name; a signal
    /// line never carries `contract_version` (the evaluator's contract scan
    /// keys on that field's presence).
    #[test]
    fn parse_signals_serialize_as_adr_event_lines() {
        let no_parse = serde_json::to_value(ParseSignal::NoParseableToolCall { round: 4 }).unwrap();
        assert_eq!(
            no_parse,
            serde_json::json!({"kind": "no_parseable_tool_call", "round": 4})
        );
        let recovered = serde_json::to_value(ParseSignal::RecoveredToolCall {
            round: 1,
            dialect: ToolCallDialect::BareJson,
        })
        .unwrap();
        assert_eq!(
            recovered,
            serde_json::json!({"kind": "recovered_tool_call", "round": 1, "dialect": "bare_json"})
        );
        assert!(no_parse.get("contract_version").is_none());
    }
}
