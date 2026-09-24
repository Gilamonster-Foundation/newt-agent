//! W0 of the output-based model-behavior ADR (epic #1506, issue #1511): the
//! TYPED signals `newt solve` needs to emit the observability-contract record
//! the external evaluator consumes.
//!
//! Dispatch classes and parse signals are decided at their observation sites:
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
//!   A recognized server capacity-rejection body is `context_exceeded`,
//!   classified by the shared context-overflow detector before retry policy.
//!
//! - **Tool-call parse status** ([`ParseSignal`] / [`round_parse_signal`]):
//!   per-round evidence for the evaluator's artifact-vs-weakness split
//!   (#1500) — `recovered_tool_call{dialect}` when `tool_recovery` fired
//!   (and which dialect matched), `no_parseable_tool_call` when a round
//!   produced content but neither native `tool_calls` nor a recovery hit.
//!
//! - **Reasoning-overflow recovery** ([`BehaviorSignal`] /
//!   [`reasoning_overflow_signature`]): the exact reasoning-only length-stop
//!   signature and the result of its one bounded continuation.
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
/// The identity the harness derived for one call it recovered from reply text:
/// the full content id (the identity) and the wire locator rendered from its
/// digest (what a provider sees as `tool_call_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveredIdentity {
    pub locator: String,
    pub cid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        /// The derived identity of each recovered call, in call order. Empty
        /// on a wire that does not require ids (the Ollama arm).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<RecoveredIdentity>,
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
    identities: Vec<RecoveredIdentity>,
) -> Option<ParseSignal> {
    if let Some(dialect) = recovered {
        return Some(ParseSignal::RecoveredToolCall {
            round,
            dialect,
            calls: identities,
        });
    }
    if !native_calls && content_nonempty {
        return Some(ParseSignal::NoParseableToolCall { round });
    }
    None
}

/// Whether one Chat Completions response exhausted its output allowance inside
/// reasoning without producing visible content or an executable tool call.
/// This remains structural: ordinary empty/stop responses and parser failures
/// are different outcomes and must not acquire a retry.
#[must_use]
pub fn reasoning_overflow_signature(
    finish_reason: Option<&str>,
    content_empty: bool,
    reasoning_nonempty: bool,
    has_calls: bool,
) -> bool {
    finish_reason == Some("length") && content_empty && reasoning_nonempty && !has_calls
}

/// Output-behavior trace events emitted alongside parse-status signals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BehaviorSignal {
    /// #2315: one result-aware verification decision at a concluding answer:
    /// `accept`, `nudge`, the stop reason, `no_checks` (nothing to verify) or
    /// `check_scan_failed`; the per-check status; and which state evidence
    /// (`tree` or `mutation_chain`) decided freshness.
    Verification {
        round: usize,
        decision: String,
        repairs_used: usize,
        allowance: usize,
        report: super::self_verify::VerificationReport,
    },
    /// A rejected request and the strictly smaller projection selected next.
    /// `None` means recovery stopped; attempts count within the user turn.
    ContextExceeded {
        round: usize,
        attempt: u32,
        estimated_tokens: usize,
        projected_tokens: Option<usize>,
    },
    /// The backend's finish reason for every parsed Chat Completions response.
    /// `None` is retained rather than invented when a compatible server omits
    /// the field.
    ChatCompletionFinish {
        round: usize,
        finish_reason: Option<String>,
    },
    /// Generation stopped at the exact reasoning-only length signature. The
    /// booleans distinguish detection, recovery eligibility, and recovery
    /// outcome without reinterpreting an empty reply downstream.
    ReasoningOverflow {
        round: usize,
        reasoning_overflow_detected: bool,
        continuation_attempted: bool,
        continuation_succeeded: bool,
        finish_reason: String,
        reasoning_tokens_estimate: usize,
    },
    /// F34: a reasoning overflow was re-dispatched once at the next-lower
    /// cognition level (labels, e.g. `thoughtful` → `rational`).
    CognitionDropRetry {
        round: usize,
        from: String,
        to: String,
    },
}

impl BehaviorSignal {
    pub(crate) fn mark_continuation_succeeded(&mut self) {
        if let Self::ReasoningOverflow {
            continuation_succeeded,
            ..
        } = self
        {
            *continuation_succeeded = true;
        }
    }
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
    /// Output-behavior signals, in detection order.
    pub behavior_signals: Vec<BehaviorSignal>,
    /// The output cap the turn's wire applied (#2312), when it applied one.
    pub output_allowance: Option<OutputAllowance>,
    /// Captured Responses declarations, present only after policy admission.
    pub responses_capability: Option<crate::model_card::ResponsesCapability>,
    /// The actual accepted Responses effort, independent of semantic intent.
    pub reasoning_effort: Option<crate::model_card::ReasoningEffort>,
    /// The reply is harness-written text (an empty-response note, a refusal
    /// placeholder, a cap-exit fallback), not a model claim (#2372).
    pub harness_reply: bool,
}

/// Mark the turn's reply as harness-written rather than model-authored.
pub(crate) fn observe_harness_reply(obs: &mut Option<&mut SolveObservation>) {
    if let Some(obs) = obs.as_deref_mut() {
        obs.harness_reply = true;
    }
}

/// An output cap as a turn applied it (#2312): the tokens, and whether the
/// request carried the cap to the server or newt only reserved it locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OutputAllowance {
    pub tokens: u32,
    pub enforced: Enforcement,
}

/// Who enforces an [`OutputAllowance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    /// The request sent the cap (`max_tokens`).
    Server,
    /// The request sent no cap; newt reserved the tokens from its input budget.
    Local,
}

/// Record the cap a wire applied, at the one point it decides the request:
/// `sent` only when that request carries the cap.
pub(crate) fn observe_output_allowance(
    obs: &mut Option<&mut SolveObservation>,
    tokens: Option<u32>,
    sent: bool,
) {
    if let Some(obs) = obs.as_deref_mut() {
        obs.output_allowance = tokens.map(|tokens| OutputAllowance {
            tokens,
            enforced: if sent {
                Enforcement::Server
            } else {
                Enforcement::Local
            },
        });
    }
}

/// Structural classification of a failed turn — the contract `outcome`
/// taxonomy minus `completed` (a clean turn has no error to classify).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The backend was reached but errored: an HTTP error status, or a body
    /// that could not be decoded. A real attempt — it carries capability
    /// signal (the model/stack answered and answered badly).
    Model,
    /// The server rejected the context. This request needs a smaller
    /// projection; unchanged transport retries cannot make it fit.
    ContextExceeded,
    /// The backend could not be reached: connection refused/reset, DNS,
    /// or a connect-phase timeout. NOT a real attempt.
    Transport,
    /// The configured bound elapsed AFTER the backend was reached: a total
    /// request timeout or a streamed response's idle read timeout.
    Timeout,
    /// The failure is on our side of the wire (a malformed request we built)
    /// — or, at the boundary, any error carrying no [`DispatchError`] at all.
    Harness,
}

/// Classify a typed reqwest error into the contract taxonomy. Order matters:
/// a connect-phase timeout reports BOTH `is_connect` and `is_timeout`, and
/// the contract files "could not reach the model" under `transport_error`,
/// so the connect check wins; a plain `is_timeout` is then a post-connect
/// request or idle-read bound. Everything else that isn't agent-side (`is_builder`) is
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
    /// Usage the rejected response reported: it was generated and billed.
    usage: Option<crate::TokenUsage>,
}

impl DispatchError {
    /// A failure whose class the caller decided from typed evidence.
    pub fn new(class: ErrorClass, msg: impl Into<String>) -> Self {
        Self {
            class,
            msg: msg.into(),
            usage: None,
        }
    }

    /// Wrap a reqwest send/decode failure, classifying it while it is still
    /// typed. `prefix` preserves the historical site wording (e.g. `"request
    /// failed"`).
    pub fn from_reqwest(prefix: &str, e: reqwest::Error) -> Self {
        Self {
            class: classify_reqwest(&e),
            msg: format!("{prefix}: {e}"),
            usage: None,
        }
    }

    /// Wrap a failure while reading an already accepted response stream.
    /// Application decoding happens after the byte read, so body/decode flags
    /// here describe the transport; only an elapsed read timeout is `Timeout`.
    pub fn response_read(prefix: &str, e: reqwest::Error) -> Self {
        Self {
            class: if e.is_timeout() {
                ErrorClass::Timeout
            } else {
                ErrorClass::Transport
            },
            msg: format!("{prefix}: {e}"),
            usage: None,
        }
    }

    /// Refuse a measured over-budget candidate before generation dispatch.
    pub fn context_exceeded(message: impl Into<String>) -> Self {
        Self {
            class: ErrorClass::ContextExceeded,
            msg: message.into(),
            usage: None,
        }
    }

    /// Wrap a non-success HTTP status: recognized context rejection is
    /// `context_exceeded`; other backend rejections are `model_error`. `msg` is
    /// the caller's fully-formatted historical string (`"Ollama {status}:
    /// {text}"` / `"inference endpoint {status}: {text}"`).
    pub fn http_status(msg: String) -> Self {
        Self {
            class: if super::cw_overflow::is_context_overflow(&msg) {
                ErrorClass::ContextExceeded
            } else {
                ErrorClass::Model
            },
            msg,
            usage: None,
        }
    }

    /// Attach the usage a rejected response reported.
    pub(crate) fn with_usage(mut self, usage: Option<crate::TokenUsage>) -> Self {
        self.usage = usage;
        self
    }
}

/// The usage a failed dispatch's response reported, if its chain carries any.
pub(crate) fn reported_usage(e: &anyhow::Error) -> Option<crate::TokenUsage> {
    // `downcast_ref` also reaches a DispatchError attached as context.
    e.downcast_ref::<DispatchError>()?.usage
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for DispatchError {}

/// Recover the structural class from an anyhow chain at the driver boundary.
/// A [`DispatchError`] wins; a raw [`reqwest::Error`] still in the chain (the
/// body-decode paths propagate it via `anyhow::Error::from`) is classified
/// typed as a fallback. `None` means neither is present — the failure
/// happened outside a dispatch (the caller files it as `harness_error`,
/// fail-closed: an unattributed error must never masquerade as a model one).
pub fn error_class(e: &anyhow::Error) -> Option<ErrorClass> {
    // `anyhow::Error::downcast_ref` reaches a DispatchError at the root or
    // attached with `.context()` (as `decode_openai_response` attaches one); a
    // chain walk sees a context layer only as its wrapper. Nothing places a
    // DispatchError behind a foreign `source()`, so only reqwest is walked for.
    e.downcast_ref::<DispatchError>()
        .map(|d| d.class)
        .or_else(|| {
            e.chain()
                .find_map(|c| c.downcast_ref::<reqwest::Error>().map(classify_reqwest))
        })
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

    /// #2318: `decode_openai_response` attaches its `DispatchError` with
    /// `.context()`, and later layers add more context. A chain walk cannot
    /// downcast a context layer to its context type, so every strict-decode
    /// rejection of a 2xx reply lost its class and was filed as a harness error.
    #[test]
    fn a_dispatch_error_attached_as_context_keeps_its_class() {
        let rejected = |message: &str| {
            anyhow::anyhow!("streamed tool call has no ID")
                .context(DispatchError::http_status(message.to_string()))
                .context("round 3")
        };
        assert_eq!(
            error_class(&rejected("streamed tool call has no ID")),
            Some(ErrorClass::Model)
        );
        assert_eq!(
            error_class(&rejected(
                r#"OpenAI stream error: {"message":"Context size has been exceeded."}"#
            )),
            Some(ErrorClass::ContextExceeded)
        );
    }

    /// `harness_error` (builder side): a request WE built wrong is our
    /// failure, not the wire's and not the model's.
    #[tokio::test]
    async fn builder_error_classifies_harness() {
        // An empty URL fails in the builder before any socket is touched.
        let err = reqwest::Client::new().get("").send().await.unwrap_err();
        assert!(err.is_builder());
        assert_eq!(classify_reqwest(&err), ErrorClass::Harness);
        // A RAW reqwest error in an anyhow chain classifies typed too — the
        // `resp.json()` decode paths propagate it via `anyhow::Error::from`
        // without a DispatchError wrapper.
        let chained = anyhow::Error::from(err).context("decoding response body");
        assert_eq!(error_class(&chained), Some(ErrorClass::Harness));
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
            round_parse_signal(2, true, false, None, vec![]),
            Some(ParseSignal::NoParseableToolCall { round: 2 })
        );
    }

    #[test]
    fn recovery_hit_signals_recovered_tool_call_with_its_dialect() {
        assert_eq!(
            round_parse_signal(1, true, false, Some(ToolCallDialect::FunctionTag), vec![]),
            Some(ParseSignal::RecoveredToolCall {
                round: 1,
                dialect: ToolCallDialect::FunctionTag,
                calls: vec![],
            })
        );
    }

    #[test]
    fn healthy_native_call_and_empty_content_signal_nothing() {
        // A native structured call is the healthy channel — no signal.
        assert_eq!(round_parse_signal(0, true, true, None, vec![]), None);
        // Empty content with no calls is the suspicious-empty case, not a
        // parse status (W3 territory) — no signal here either.
        assert_eq!(round_parse_signal(0, false, false, None, vec![]), None);
    }

    #[test]
    fn reasoning_overflow_requires_the_exact_structural_signature() {
        assert!(reasoning_overflow_signature(
            Some("length"),
            true,
            true,
            false
        ));
        for (finish_reason, content_empty, reasoning_nonempty, has_calls) in [
            (Some("stop"), true, true, false),
            (Some("length"), false, true, false),
            (Some("length"), true, false, false),
            (Some("length"), true, true, true),
            (None, true, true, false),
        ] {
            assert!(
                !reasoning_overflow_signature(
                    finish_reason,
                    content_empty,
                    reasoning_nonempty,
                    has_calls
                ),
                "non-matching signature must not trigger: {finish_reason:?}"
            );
        }
    }

    #[test]
    fn reasoning_overflow_signal_serializes_recovery_outcome() {
        let signal = BehaviorSignal::ReasoningOverflow {
            round: 2,
            reasoning_overflow_detected: true,
            continuation_attempted: true,
            continuation_succeeded: false,
            finish_reason: "length".into(),
            reasoning_tokens_estimate: 37,
        };

        assert_eq!(
            serde_json::to_value(signal).unwrap(),
            serde_json::json!({
                "kind": "reasoning_overflow",
                "round": 2,
                "reasoning_overflow_detected": true,
                "continuation_attempted": true,
                "continuation_succeeded": false,
                "finish_reason": "length",
                "reasoning_tokens_estimate": 37,
            })
        );
    }

    #[test]
    fn chat_completion_finish_signal_records_each_finish_reason() {
        assert_eq!(
            serde_json::to_value(BehaviorSignal::ChatCompletionFinish {
                round: 4,
                finish_reason: Some("tool_calls".into()),
            })
            .unwrap(),
            serde_json::json!({
                "kind": "chat_completion_finish",
                "round": 4,
                "finish_reason": "tool_calls",
            })
        );
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
            calls: vec![],
        })
        .unwrap();
        assert_eq!(
            recovered,
            serde_json::json!({"kind": "recovered_tool_call", "round": 1, "dialect": "bare_json"})
        );
        assert!(no_parse.get("contract_version").is_none());
    }
}
