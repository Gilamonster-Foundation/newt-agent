//! #1528 B5 — strict Responses wire validation (the last line before an
//! irreversible `POST /v1/responses`).
//!
//! Every Responses dispatch in the agentic loop ([`super::openai_responses_complete`])
//! goes through ONE typed gate: [`validate_responses_request`] takes the exact JSON
//! body the loop built and either returns a [`ValidatedResponsesRequest`] — a newtype
//! with **no public constructor** other than a successful validation — or a typed
//! [`WireValidationError`]. [`super::dispatch_responses_json`] accepts ONLY a
//! `ValidatedResponsesRequest`, so a broken request cannot compile its way to the
//! wire (make-it-unrepresentable over fix-each-site — reuse discipline).
//!
//! This is the typed WIDENING of [`super::preflight_responses_request`]: the budget
//! fit + irreducibility refusal is still that ONE primitive (called from within this
//! validator), now wrapped by the full wire-invariant set so no second parallel
//! validator stands beside it. A validation failure produces ZERO HTTP dispatches,
//! NO round advancement, NO tool side effect, NO usage observation, and a REDACTED
//! diagnostic (structural facts only — role names, counts, content-address handles,
//! budget numbers — never operator prompt text or tool arguments).
//!
//! The concrete Serde/HTTP behavior is proven by the mock-server contract tests in
//! `mod.rs`; the abstract validator↦dispatchable laws are machine-checked in
//! `formal/ResponsesWire`.

use super::content_spill::{SpillCid, SpillStore};
use serde_json::Value;

/// A `spill:`/`compaction:` content handle in the running `input` is a base32
/// CIDv1 (~59 chars). Only an alphanumeric run at least this long right after the
/// prefix is treated as a handle to validate, so ordinary prose that merely
/// contains `spill:`/`compaction:` is never a false marker.
const MIN_HANDLE_LEN: usize = 40;

/// Which session store a content handle must resolve in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
    Spill,
    Compaction,
}

impl MarkerKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spill => "spill",
            Self::Compaction => "compaction",
        }
    }
}

/// The EXPLICIT endpoint policy a Responses request is validated against (#1528,
/// invariants #10/#11: remote-storage policy is explicit, never inherited). The
/// budget/calibration/model triple feeds the ONE shared preflight; the session
/// stores mediate content-handle membership.
pub(super) struct ResponsesWirePolicy<'a> {
    /// The required `store` value (newt is stateless ⇒
    /// [`crate::responses_wire::STORE_RESPONSE_SERVER_SIDE`]). A mismatch fails closed.
    pub(super) store: bool,
    /// Whether a `tools` array may be present. FALSE for the final tools-disabled
    /// summary (which MUST carry none); TRUE for tool-capable rounds (present or not).
    pub(super) tools_permitted: bool,
    /// The model id the request must name (used only for the redacted diagnostic +
    /// the shared preflight's refusal message).
    pub(super) model: &'a str,
    /// The authoritative input budget the request estimate must fit (`None` =
    /// ceiling-less cloud Responses). Enforced by the shared preflight.
    pub(super) authoritative_budget: Option<usize>,
    pub(super) calibration: f32,
    pub(super) estimation: crate::tokens::TokenEstimation,
    /// The session spill store — membership authorizes a `spill:` handle.
    pub(super) spill: Option<&'a dyn SpillStore>,
    /// The session compaction store — membership authorizes a `compaction:` handle.
    pub(super) compaction: Option<&'a dyn SpillStore>,
}

impl ResponsesWirePolicy<'_> {
    fn store_for(&self, kind: MarkerKind) -> Option<&dyn SpillStore> {
        match kind {
            MarkerKind::Spill => self.spill,
            MarkerKind::Compaction => self.compaction,
        }
    }
}

/// A Responses request body that has passed EVERY wire invariant. There is NO
/// public constructor other than a successful [`validate_responses_request`], so a
/// value of this type is a proof the body is dispatchable; [`super::dispatch_responses_json`]
/// requires it.
#[derive(Debug)]
pub(super) struct ValidatedResponsesRequest {
    body: Value,
}

impl ValidatedResponsesRequest {
    /// The validated body to POST. Read-only — the body cannot be mutated after
    /// validation without re-validating.
    pub(super) fn body(&self) -> &Value {
        &self.body
    }

    /// TEST-ONLY seam: build a validated request WITHOUT validation, for the shared
    /// transport-retry test that exercises `dispatch_responses_json`'s backoff (the
    /// wire invariants are covered by the contract tests). Never compiled into a
    /// release build, so the production "no constructor but validation" property holds.
    #[cfg(test)]
    pub(super) fn from_body_for_test(body: Value) -> Self {
        Self { body }
    }
}

/// Why a Responses request is NOT dispatchable. Every variant is a fail-closed
/// refusal; the `Display` text is REDACTED — structural facts only (never operator
/// prompt text or tool-call arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WireValidationError {
    /// `model` absent or empty.
    MissingModel,
    /// `store` does not match the explicit endpoint policy.
    StorePolicyMismatch { expected: bool, found: Option<bool> },
    /// A Responses request carried Ollama's `num_ctx` (never valid on this wire).
    NumCtxPresent,
    /// `input` is absent or not an array.
    InputNotArray,
    /// The instruction source count is not exactly one (zero, or duplicated across
    /// the top-level `instructions` field and a `system`/`developer` input item).
    InstructionsNotExactlyOnce { sources: usize },
    /// An `input` item carries an unsupported `type`.
    UnsupportedInputItem { item_type: String },
    /// A privileged role (`system`/`developer`/`tool`) leaked into `input`.
    RawPrivilegedRole { role: String },
    /// An `input` message item has a missing/unknown role that would silently
    /// default to `user`.
    MalformedInputRole,
    /// A `function_call` / `function_call_output` is missing its correlation id.
    MissingCorrelationId { kind: &'static str },
    /// A `function_call_output` has no matching preceding `function_call`.
    DanglingFunctionOutput,
    /// A `function_call` has no matching following `function_call_output` (a call
    /// left dangling, e.g. by a mis-fenced compaction).
    DanglingFunctionCall,
    /// The final tools-disabled summary carried a `tools` array.
    ToolsOnFinalSummary { count: usize },
    /// A tool is not in the flattened Responses shape (missing name / non-object
    /// parameters / still Chat-shaped).
    MalformedTool,
    /// A strict-shaped tool schema (`parameters.additionalProperties == false`)
    /// lost its `strict: true` marker, silently downgrading validation.
    StrictSchemaLoss,
    /// A content handle in `input` does not parse as a canonical `SpillCid`.
    MalformedCidMarker { kind: &'static str },
    /// A canonical content handle in `input` does not belong to this session's store.
    ForeignSessionCid { kind: &'static str },
    /// The request estimate does not fit the actionable budget (the shared
    /// preflight's refusal; already redacted — names no operator content).
    OverBudget(String),
}

impl std::fmt::Display for WireValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingModel => write!(f, "Responses request has no model"),
            Self::StorePolicyMismatch { expected, found } => write!(
                f,
                "Responses `store` policy mismatch: expected {expected}, found {found:?}"
            ),
            Self::NumCtxPresent => {
                write!(
                    f,
                    "Responses request carries num_ctx (forbidden on this wire)"
                )
            }
            Self::InputNotArray => write!(f, "Responses request `input` is not an array"),
            Self::InstructionsNotExactlyOnce { sources } => write!(
                f,
                "Responses request has {sources} instruction sources (must be exactly one)"
            ),
            Self::UnsupportedInputItem { item_type } => {
                write!(f, "unsupported Responses input item type {item_type:?}")
            }
            Self::RawPrivilegedRole { role } => {
                write!(f, "privileged role {role:?} leaked into Responses input")
            }
            Self::MalformedInputRole => write!(
                f,
                "a Responses input message item has a missing/unknown role (would default to user)"
            ),
            Self::MissingCorrelationId { kind } => {
                write!(f, "Responses {kind} is missing its correlation id")
            }
            Self::DanglingFunctionOutput => write!(
                f,
                "a Responses function_call_output has no matching preceding function_call"
            ),
            Self::DanglingFunctionCall => write!(
                f,
                "a Responses function_call has no matching following function_call_output"
            ),
            Self::ToolsOnFinalSummary { count } => write!(
                f,
                "the tools-disabled final summary carried {count} tool(s)"
            ),
            Self::MalformedTool => write!(f, "a Responses tool is not the flattened wire shape"),
            Self::StrictSchemaLoss => write!(
                f,
                "a strict-shaped Responses tool schema lost its `strict: true` marker"
            ),
            Self::MalformedCidMarker { kind } => {
                write!(
                    f,
                    "a {kind}: content handle in input is not a canonical CID"
                )
            }
            Self::ForeignSessionCid { kind } => write!(
                f,
                "a {kind}: content handle in input does not belong to this session"
            ),
            Self::OverBudget(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for WireValidationError {}

/// Validate the exact JSON `body` about to be POSTed to `/v1/responses` against the
/// explicit `policy`. On success the body is dispatchable; on ANY failure the
/// caller must NOT dispatch (fail closed). This is the sole path to a
/// [`ValidatedResponsesRequest`].
pub(super) fn validate_responses_request(
    body: &Value,
    policy: &ResponsesWirePolicy,
) -> Result<ValidatedResponsesRequest, WireValidationError> {
    // 1. model present + nonempty.
    match body.get("model").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => {}
        _ => return Err(WireValidationError::MissingModel),
    }

    // 2. store matches the explicit endpoint policy.
    let found_store = body.get("store").and_then(Value::as_bool);
    if found_store != Some(policy.store) {
        return Err(WireValidationError::StorePolicyMismatch {
            expected: policy.store,
            found: found_store,
        });
    }

    // 3. num_ctx must be ABSENT (this is the Ollama display hint; never on this wire).
    if body.get("num_ctx").is_some() {
        return Err(WireValidationError::NumCtxPresent);
    }

    // 4. input must be an array.
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return Err(WireValidationError::InputNotArray);
    };

    // 5. instructions occurs EXACTLY once — the top-level field plus any
    //    system/developer input item (a second, laundered source) must sum to one.
    let top_level = body.get("instructions").is_some_and(|v| !v.is_null());
    let laundered_sources = input
        .iter()
        .filter(|it| {
            it.get("type").is_none()
                && matches!(
                    it.get("role").and_then(Value::as_str),
                    Some("system") | Some("developer")
                )
        })
        .count();
    let sources = usize::from(top_level) + laundered_sources;
    if sources != 1 {
        return Err(WireValidationError::InstructionsNotExactlyOnce { sources });
    }

    // 6. every input item is a supported shape; no privileged/malformed role.
    validate_input_items(input)?;

    // 7. function-call ↔ output correlation (no dangling call or output).
    validate_correlation(input)?;

    // 8. tools: absent on the final summary; flattened + strict-preserving otherwise.
    validate_tools(body, policy)?;

    // 9. every content handle in input parses canonically AND is in this session.
    validate_cid_markers(input, policy)?;

    // 10. the request estimate fits the actionable budget (the ONE shared preflight).
    let instructions = body.get("instructions").and_then(Value::as_str);
    let tools = body.get("tools").and_then(Value::as_array);
    super::preflight_responses_request(
        instructions,
        input,
        tools.map(Vec::as_slice),
        policy.authoritative_budget,
        policy.calibration,
        policy.estimation,
        policy.model,
    )
    .map_err(|e| WireValidationError::OverBudget(e.to_string()))?;

    Ok(ValidatedResponsesRequest { body: body.clone() })
}

/// Every `input` item is a supported shape and never carries a privileged or
/// malformed role.
fn validate_input_items(input: &[Value]) -> Result<(), WireValidationError> {
    for item in input {
        if let Some(t) = item.get("type").and_then(Value::as_str) {
            if !matches!(
                t,
                "function_call" | "function_call_output" | "reasoning" | "message"
            ) {
                return Err(WireValidationError::UnsupportedInputItem {
                    item_type: t.to_string(),
                });
            }
            // A typed item that ALSO carries a role must still be user/assistant.
            if let Some(role) = item.get("role").and_then(Value::as_str) {
                check_message_role(role)?;
            }
        } else if let Some(role) = item.get("role").and_then(Value::as_str) {
            check_message_role(role)?;
        } else {
            // No type and no role: a bare item that would silently become `user`.
            return Err(WireValidationError::MalformedInputRole);
        }
    }
    Ok(())
}

fn check_message_role(role: &str) -> Result<(), WireValidationError> {
    match role {
        "user" | "assistant" => Ok(()),
        "system" | "developer" | "tool" => Err(WireValidationError::RawPrivilegedRole {
            role: role.to_string(),
        }),
        _ => Err(WireValidationError::MalformedInputRole),
    }
}

/// Each `function_call_output` has a matching preceding `function_call`, and each
/// `function_call` a matching following output — no dangling correlation survives.
fn validate_correlation(input: &[Value]) -> Result<(), WireValidationError> {
    let mut calls: Vec<(usize, &str)> = Vec::new();
    let mut outputs: Vec<(usize, &str)> = Vec::new();
    for (i, item) in input.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("id").and_then(Value::as_str));
                match id {
                    Some(s) if !s.is_empty() => calls.push((i, s)),
                    _ => {
                        return Err(WireValidationError::MissingCorrelationId {
                            kind: "function_call",
                        })
                    }
                }
            }
            Some("function_call_output") => match item.get("call_id").and_then(Value::as_str) {
                Some(s) if !s.is_empty() => outputs.push((i, s)),
                _ => {
                    return Err(WireValidationError::MissingCorrelationId {
                        kind: "function_call_output",
                    })
                }
            },
            _ => {}
        }
    }
    for (oi, oid) in &outputs {
        if !calls.iter().any(|(ci, cid)| ci < oi && cid == oid) {
            return Err(WireValidationError::DanglingFunctionOutput);
        }
    }
    for (ci, cid) in &calls {
        if !outputs.iter().any(|(oi, oid)| oi > ci && oid == cid) {
            return Err(WireValidationError::DanglingFunctionCall);
        }
    }
    Ok(())
}

/// The final summary carries no tools; every other request's tools are the
/// flattened Responses shape with strictness preserved.
fn validate_tools(body: &Value, policy: &ResponsesWirePolicy) -> Result<(), WireValidationError> {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return Ok(());
    };
    if !policy.tools_permitted {
        return Err(WireValidationError::ToolsOnFinalSummary { count: tools.len() });
    }
    for tool in tools {
        // Flattened Responses shape: `{type:function, name, parameters, …}`; a
        // residual `function` key means it was never flattened.
        let name_ok = tool
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| !n.is_empty());
        if !name_ok || tool.get("function").is_some() || !tool["parameters"].is_object() {
            return Err(WireValidationError::MalformedTool);
        }
        // A schema that forbids additional properties is only ENFORCED as strict
        // when `strict: true` rides at the tool's top level; without it the
        // provider treats the schema as advisory — a silent strictness loss.
        let strict_shaped = tool["parameters"]["additionalProperties"] == Value::Bool(false);
        let is_strict = tool.get("strict") == Some(&Value::Bool(true));
        if strict_shaped && !is_strict {
            return Err(WireValidationError::StrictSchemaLoss);
        }
    }
    Ok(())
}

/// Every `spill:`/`compaction:` content handle in `input` parses as a canonical
/// `SpillCid` AND resolves in this session's corresponding store.
fn validate_cid_markers(
    input: &[Value],
    policy: &ResponsesWirePolicy,
) -> Result<(), WireValidationError> {
    let mut markers: Vec<(MarkerKind, String)> = Vec::new();
    for item in input {
        scan_value_for_markers(item, &mut markers);
    }
    for (kind, handle) in markers {
        let cid =
            SpillCid::parse(&handle).map_err(|_| WireValidationError::MalformedCidMarker {
                kind: kind.as_str(),
            })?;
        let in_session = policy
            .store_for(kind)
            .is_some_and(|store| store.fetch(&cid).is_some());
        if !in_session {
            return Err(WireValidationError::ForeignSessionCid {
                kind: kind.as_str(),
            });
        }
    }
    Ok(())
}

/// Recursively collect every `spill:`/`compaction:` handle candidate from a JSON
/// value's string leaves.
fn scan_value_for_markers(v: &Value, out: &mut Vec<(MarkerKind, String)>) {
    match v {
        Value::String(s) => extract_markers(s, out),
        Value::Array(a) => a.iter().for_each(|x| scan_value_for_markers(x, out)),
        Value::Object(o) => o.values().for_each(|x| scan_value_for_markers(x, out)),
        _ => {}
    }
}

/// Extract content-handle candidates: a `spill:`/`compaction:` prefix immediately
/// followed by an alphanumeric run of at least [`MIN_HANDLE_LEN`] chars (the length
/// floor keeps ordinary prose that merely contains the prefix from being a marker).
fn extract_markers(s: &str, out: &mut Vec<(MarkerKind, String)>) {
    for (kind, prefix) in [
        (MarkerKind::Spill, "spill:"),
        (MarkerKind::Compaction, "compaction:"),
    ] {
        for (idx, _) in s.match_indices(prefix) {
            let rest = &s[idx + prefix.len()..];
            let handle: String = rest
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            if handle.len() >= MIN_HANDLE_LEN {
                out.push((kind, handle));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentic::content_spill::{
        SessionSpillStore, SpillProvenance, SpillRecordV1, SpillScope, SpillStore,
    };

    const NONCE: [u8; 16] = [3u8; 16];
    const FOREIGN_NONCE: [u8; 16] = [9u8; 16];

    fn base_policy<'a>() -> ResponsesWirePolicy<'a> {
        ResponsesWirePolicy {
            store: crate::responses_wire::STORE_RESPONSE_SERVER_SIDE,
            tools_permitted: true,
            model: "m",
            authoritative_budget: None,
            calibration: 1.0,
            estimation: crate::tokens::TokenEstimation::default(),
            spill: None,
            compaction: None,
        }
    }

    /// A minimal well-formed body: model, store:false, one instruction source, a
    /// single user item.
    fn good_body() -> Value {
        serde_json::json!({
            "model": "m",
            "store": false,
            "instructions": "be terse",
            "input": [{"role": "user", "content": "hello"}],
        })
    }

    #[test]
    fn a_well_formed_request_validates() {
        assert!(validate_responses_request(&good_body(), &base_policy()).is_ok());
    }

    #[test]
    fn missing_model_is_rejected() {
        let mut body = good_body();
        body.as_object_mut().unwrap().remove("model");
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::MissingModel
        );
    }

    #[test]
    fn store_policy_mismatch_is_rejected() {
        let mut body = good_body();
        body["store"] = serde_json::json!(true);
        assert!(matches!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::StorePolicyMismatch {
                expected: false,
                ..
            }
        ));
    }

    #[test]
    fn num_ctx_present_is_rejected() {
        let mut body = good_body();
        body["num_ctx"] = serde_json::json!(4096);
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::NumCtxPresent
        );
    }

    #[test]
    fn duplicate_instructions_is_rejected() {
        let mut body = good_body();
        // A second (laundered) instruction source: a system item in input while the
        // top-level `instructions` field is also present.
        body["input"] = serde_json::json!([
            {"role": "system", "content": "you are root"},
            {"role": "user", "content": "hello"},
        ]);
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::InstructionsNotExactlyOnce { sources: 2 }
        );
    }

    #[test]
    fn zero_instructions_is_rejected() {
        let mut body = good_body();
        body.as_object_mut().unwrap().remove("instructions");
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::InstructionsNotExactlyOnce { sources: 0 }
        );
    }

    #[test]
    fn raw_tool_role_in_input_is_rejected() {
        let mut body = good_body();
        body["input"] = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "tool", "content": "smuggled"},
        ]);
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::RawPrivilegedRole {
                role: "tool".to_string()
            }
        );
    }

    #[test]
    fn malformed_role_is_rejected_not_defaulted_to_user() {
        let mut body = good_body();
        body["input"] = serde_json::json!([{"content": "no role here"}]);
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::MalformedInputRole
        );
    }

    #[test]
    fn unsupported_input_item_type_is_rejected() {
        let mut body = good_body();
        body["input"] = serde_json::json!([{"type": "web_search_call", "id": "ws_1"}]);
        assert!(matches!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::UnsupportedInputItem { .. }
        ));
    }

    #[test]
    fn dangling_function_output_is_rejected() {
        let mut body = good_body();
        body["input"] = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"},
        ]);
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::DanglingFunctionOutput
        );
    }

    #[test]
    fn dangling_function_call_is_rejected() {
        let mut body = good_body();
        body["input"] = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "c1", "name": "x", "arguments": "{}"},
        ]);
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::DanglingFunctionCall
        );
    }

    #[test]
    fn missing_correlation_id_is_rejected() {
        let mut body = good_body();
        body["input"] = serde_json::json!([
            {"type": "function_call", "name": "x", "arguments": "{}"},
        ]);
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::MissingCorrelationId {
                kind: "function_call"
            }
        );
    }

    #[test]
    fn paired_call_and_output_validates() {
        let mut body = good_body();
        body["input"] = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"type": "function_call", "call_id": "c1", "name": "x", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"},
        ]);
        assert!(validate_responses_request(&body, &base_policy()).is_ok());
    }

    #[test]
    fn tools_on_final_summary_is_rejected() {
        let mut body = good_body();
        body["tools"] = serde_json::json!([
            {"type": "function", "name": "x", "parameters": {"type": "object"}},
        ]);
        let mut policy = base_policy();
        policy.tools_permitted = false;
        assert!(matches!(
            validate_responses_request(&body, &policy).unwrap_err(),
            WireValidationError::ToolsOnFinalSummary { count: 1 }
        ));
    }

    #[test]
    fn strict_schema_loss_is_rejected() {
        let mut body = good_body();
        // additionalProperties:false but NO top-level strict:true → silently relaxed.
        body["tools"] = serde_json::json!([{
            "type": "function",
            "name": "write_file",
            "parameters": {"type": "object", "additionalProperties": false},
        }]);
        assert_eq!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::StrictSchemaLoss
        );
    }

    #[test]
    fn strict_schema_preserved_validates() {
        let mut body = good_body();
        body["tools"] = serde_json::json!([{
            "type": "function",
            "name": "write_file",
            "strict": true,
            "parameters": {"type": "object", "additionalProperties": false},
        }]);
        assert!(validate_responses_request(&body, &base_policy()).is_ok());
    }

    #[test]
    fn malformed_cid_marker_is_rejected() {
        let mut body = good_body();
        // A long alnum run after `spill:` that is not a canonical CID.
        let bogus = "b".to_string() + &"z".repeat(58);
        body["input"] =
            serde_json::json!([{"role": "user", "content": format!("see spill:{bogus} now")}]);
        assert!(matches!(
            validate_responses_request(&body, &base_policy()).unwrap_err(),
            WireValidationError::MalformedCidMarker { kind: "spill" }
        ));
    }

    #[test]
    fn foreign_session_cid_marker_is_rejected() {
        // A canonically-spelled handle minted under ANOTHER session's nonce: it
        // parses, but does not resolve in THIS session's store.
        let foreign = SpillCid::of(&SpillRecordV1::new(
            SpillScope::Session(FOREIGN_NONCE),
            SpillProvenance::ToolOutput { tool_name: None },
            "secret".to_string(),
        ))
        .unwrap();
        let store = SessionSpillStore::new(NONCE);
        let mut policy = base_policy();
        policy.spill = Some(&store);
        let mut body = good_body();
        body["input"] = serde_json::json!([{
            "role": "user",
            "content": format!("memory_fetch(\"spill:{}\")", foreign.to_handle()),
        }]);
        assert_eq!(
            validate_responses_request(&body, &policy).unwrap_err(),
            WireValidationError::ForeignSessionCid { kind: "spill" }
        );
    }

    #[test]
    fn in_session_cid_marker_validates() {
        let store = SessionSpillStore::new(NONCE);
        let handle = store
            .store(SpillRecordV1::new(
                SpillScope::Session(NONCE),
                SpillProvenance::ToolOutput { tool_name: None },
                "payload".to_string(),
            ))
            .unwrap();
        let mut policy = base_policy();
        policy.spill = Some(&store);
        let mut body = good_body();
        body["input"] = serde_json::json!([{
            "role": "user",
            "content": format!("memory_fetch(\"spill:{}\")", handle.to_handle()),
        }]);
        assert!(validate_responses_request(&body, &policy).is_ok());
    }

    #[test]
    fn over_budget_request_is_rejected() {
        let mut policy = base_policy();
        policy.authoritative_budget = Some(1); // 1-token budget cannot fit anything
        let mut body = good_body();
        body["input"] = serde_json::json!([{"role": "user", "content": "x ".repeat(4_000)}]);
        assert!(matches!(
            validate_responses_request(&body, &policy).unwrap_err(),
            WireValidationError::OverBudget(_)
        ));
    }

    #[test]
    fn prose_containing_the_prefix_is_not_a_marker() {
        // A short word after `spill:` is not a handle candidate — no false refusal.
        let mut body = good_body();
        body["input"] = serde_json::json!([{"role": "user", "content": "do not spill: the beans"}]);
        assert!(validate_responses_request(&body, &base_policy()).is_ok());
    }
}
