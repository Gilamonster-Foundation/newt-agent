use super::*;

// -- #728 request_user_input (generic ask-the-human) --------------------

/// A gate that answers a free-text question with a scripted
/// [`HumanQuestionOutcome`]. Its grant path (`ask`) is irrelevant here — it
/// denies.
struct AskGate {
    outcome: HumanQuestionOutcome,
    asked: Vec<String>,
}

impl AskGate {
    /// `Some(answer)` → an answer; `None` → no operator available.
    fn new(answer: Option<&str>) -> Self {
        let outcome = answer.map_or(HumanQuestionOutcome::Unavailable, |a| {
            HumanQuestionOutcome::Answer(a.to_string())
        });
        Self::with_outcome(outcome)
    }
    fn with_outcome(outcome: HumanQuestionOutcome) -> Self {
        Self {
            outcome,
            asked: Vec::new(),
        }
    }
}

impl super::PermissionGate for AskGate {
    fn ask(&mut self, _requests: &[super::PermissionRequest]) -> super::PermissionDecision {
        super::PermissionDecision::Deny
    }
    fn ask_question(&mut self, question: &str) -> HumanQuestionOutcome {
        self.asked.push(question.to_string());
        self.outcome.clone()
    }
}

#[test]
fn request_user_input_returns_the_human_answer() {
    // A gate whose ask_question returns Some(answer) → the tool returns that
    // answer verbatim, and the gate was asked the exact question.
    let mut gate = AskGate::new(Some("postgres"));
    let out = execute_request_user_input(
        &serde_json::json!({"question": "which database should I target?"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, "postgres");
    assert_eq!(
        gate.asked,
        vec!["which database should I target?".to_string()]
    );
}

#[test]
fn request_user_input_reaches_the_operator_even_when_permissions_are_denied() {
    // Blocker: disabling permission prompts must NOT erase the operator. A
    // gate whose authorization path denies (AskGate.ask → Deny) but which has
    // a present human still answers request_user_input — never "headless".
    let mut gate = AskGate::new(Some("postgres"));
    let out = execute_request_user_input(
        &serde_json::json!({"question": "which database?"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, "postgres");
    assert!(
        !out.contains("headless"),
        "a present operator is not headless: {out}"
    );
}

#[test]
fn request_user_input_no_gate_reports_headless_never_hangs() {
    // No gate (headless / eval / ACP) → the recoverable "no human available"
    // message — never a hang. (This test completing IS the no-hang proof: it
    // touches no real stdin.)
    let out = execute_request_user_input(
        &serde_json::json!({"question": "are you sure?"}),
        None,
        false,
        20,
    );
    assert_eq!(out, HEADLESS_NO_HUMAN);
    assert!(out.contains("no human available"), "got: {out}");
}

#[test]
fn request_user_input_unavailable_reports_no_operator_not_headless() {
    // A gate present but with no interactive operator (Unavailable) → the
    // no-operator message, NOT "headless": only an absent gate is headless.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::Unavailable);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, NO_OPERATOR_AVAILABLE);
    assert!(
        !out.contains("headless"),
        "Unavailable must not say headless: {out}"
    );
}

#[test]
fn request_user_input_cancelled_reports_cancel_not_headless() {
    // Esc / slash back-out (Cancelled) → an explicit cancel message, never
    // "headless" or "no human available" — the operator IS present.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::Cancelled);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, OPERATOR_CANCELLED);
    assert!(!out.contains("headless"), "got: {out}");
    assert!(!out.contains("no human available"), "got: {out}");
}

#[test]
fn request_user_input_exit_reports_exit_not_headless() {
    // Ctrl-C / Ctrl-D (ExitRequested) → an explicit exit message, not headless.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::ExitRequested);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, OPERATOR_EXIT_REQUESTED);
    assert!(!out.contains("headless"), "got: {out}");
}

#[test]
fn request_user_input_eof_is_not_an_empty_answer() {
    // EOF (InputClosed) must NOT surface as an empty answer (""), and must
    // not be reported as headless.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::InputClosed);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, OPERATOR_INPUT_CLOSED);
    assert!(!out.is_empty(), "EOF must not become an empty answer");
    assert!(!out.contains("headless"), "got: {out}");
}

#[test]
fn request_user_input_failure_is_distinct_from_headless() {
    // An input I/O failure (InputFailed) is distinct from a headless session.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::InputFailed);
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, OPERATOR_INPUT_FAILED);
    assert_ne!(out, HEADLESS_NO_HUMAN);
    assert!(!out.contains("headless"), "got: {out}");
}

#[test]
fn request_user_input_empty_answer_stays_an_empty_answer() {
    // An explicitly submitted empty line is Answer("") — distinct from EOF.
    let mut gate = AskGate::with_outcome(HumanQuestionOutcome::Answer(String::new()));
    let out = execute_request_user_input(
        &serde_json::json!({"question": "pick one"}),
        Some(&mut gate),
        false,
        20,
    );
    assert_eq!(out, "");
}

#[test]
fn request_user_input_requires_a_question() {
    // Missing / blank question → coach; the gate is never consulted.
    let mut gate = AskGate::new(Some("unused"));
    let out = execute_request_user_input(
        &serde_json::json!({"question": "   "}),
        Some(&mut gate),
        false,
        20,
    );
    assert!(out.contains("'question' is required"), "got: {out}");
    assert!(
        gate.asked.is_empty(),
        "gate not consulted for a blank question"
    );
}

#[test]
fn request_user_input_is_a_real_tool_not_a_phantom() {
    // #728: a real, always-advertised tool — never an alias of itself or a
    // hallucination.
    assert!(resolve_tool_alias("request_user_input").is_none());
    assert!(ALL_TOOL_NAMES.contains(&"request_user_input"));
    assert!(classify_phantom_reach(
        "request_user_input",
        &serde_json::json!({"question": "which db?"}),
        "postgres",
        true,
    )
    .is_none());
    // The always-advertised def rides in every session (empty MCP).
    let defs = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    let names: Vec<&str> = defs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"request_user_input"), "got: {names:?}");
}

#[test]
fn ask_verbs_rewrite_to_request_user_input() {
    // #728: the instinctive ask-the-human verbs resolve to the real tool.
    for verb in [
        "ask_user",
        "ask_human",
        "prompt_user",
        "get_user_input",
        "ask_question",
        "clarify",
        "ask",
    ] {
        match resolve_tool_alias(verb) {
            Some(AliasOutcome::Rewrite(c)) => {
                assert_eq!(c, "request_user_input", "verb: {verb}");
            }
            _ => panic!("expected Rewrite(request_user_input) for {verb}"),
        }
    }
}

#[tokio::test]
async fn request_user_input_dispatches_through_execute_tool() {
    // End-to-end through the dispatcher: the question reaches the gate and
    // the answer flows back. Fully mocked (AskGate, no real stdin).
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let mut gate = AskGate::new(Some("the answer"));
    let out = execute_tool(
        "request_user_input",
        &serde_json::json!({"question": "what now?"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None, // memory_source
        Some(&mut gate),
        None,
        None, // git_tool
        None, // crew_runner
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await;
    assert_eq!(out, "the answer");
    assert_eq!(gate.asked, vec!["what now?".to_string()]);
}

#[tokio::test]
async fn explain_request_user_input_keeps_the_interactive_question_gate() {
    // Regression: the non-Act authority clamp used to erase the whole gate,
    // which made this advertised Explain tool falsely report headless. Its
    // free-text path does not mint authority, so it must keep the operator.
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let mut mcp = NoMcp;
    let mut gate = AskGate::new(Some("send an Act request"));

    let out = run_tool_with_disposition(
        "request_user_input",
        serde_json::json!({"question": "Please send this as an explicit action request."}),
        ws.path(),
        &caveats,
        &mut mcp,
        Some(&mut gate),
        None,
        PromptDisposition::Explain,
    )
    .await;

    assert_eq!(out, "send an Act request");
    assert_eq!(
        gate.asked,
        vec!["Please send this as an explicit action request.".to_string()]
    );
}
