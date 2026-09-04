//! #2051: the widening escalation, at the dispatcher boundary.
//!
//! These are the security tests. The affordance is only worth having if a
//! model cannot use it to widen itself, so each one attacks that: the model
//! calls the tool, and the question is always whether authority moved without
//! a human saying yes.

use super::*;
use crate::agentic::NoMcp;
use crate::agentic::{DispositionRequestControl, DispositionRequestVerdict, PromptDisposition};
use crate::caveats::Caveats;
use crate::{HumanQuestionOutcome, PermissionDecision, PermissionGate, PermissionRequest};
use std::sync::atomic::{AtomicBool, Ordering};

/// A gate that answers the widening question with a scripted outcome and
/// records what it was asked.
struct ScriptedOperator {
    outcome: HumanQuestionOutcome,
    asked: std::sync::Mutex<Vec<String>>,
}

impl ScriptedOperator {
    fn answering(answer: &str) -> Self {
        Self {
            outcome: HumanQuestionOutcome::Answer(answer.to_string()),
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }
    fn with_outcome(outcome: HumanQuestionOutcome) -> Self {
        Self {
            outcome,
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl PermissionGate for ScriptedOperator {
    fn ask(&mut self, _requests: &[PermissionRequest]) -> PermissionDecision {
        // A widening request must never travel this path: `ask` mints caveats.
        panic!("request_disposition must not reach the capability-grant seam");
    }
    fn ask_question(&mut self, question: &str) -> HumanQuestionOutcome {
        self.asked.lock().expect("asked").push(question.to_string());
        self.outcome.clone()
    }
}

/// Session-local grant state, the shape a real surface implements.
#[derive(Default)]
struct GrantState {
    granted: AtomicBool,
}

impl DispositionRequestControl for GrantState {
    fn granted(&self) -> Option<PromptDisposition> {
        self.granted
            .load(Ordering::Acquire)
            .then_some(PromptDisposition::Act)
    }
    fn grant(&self, disposition: PromptDisposition) -> Result<(), String> {
        assert_eq!(
            disposition,
            PromptDisposition::Act,
            "only an Act widening is ever requested today"
        );
        self.granted.store(true, Ordering::Release);
        Ok(())
    }
}

async fn ask_to_widen<'a>(
    justification: &str,
    gate: Option<&'a mut dyn PermissionGate>,
    control: Option<&'a dyn DispositionRequestControl>,
    disposition: PromptDisposition,
) -> String {
    let ws = tempfile::TempDir::new().expect("tempdir");
    execute_tool_with_collaborators(
        "request_disposition",
        &serde_json::json!({ "justification": justification }),
        &ws.path().to_string_lossy(),
        false,
        20,
        &Caveats::top(),
        &mut NoMcp,
        ToolCollaborators {
            permission_gate: gate,
            disposition_request_control: control,
            ..Default::default()
        },
        false,
        disposition,
        None,
    )
    .await
    .expect("test dispatch is not cancellable")
}

/// The happy path, and the only path that moves authority: a human said yes.
#[tokio::test]
async fn an_operator_yes_widens_the_turn_for_the_rest_of_it() {
    let mut gate = ScriptedOperator::answering("yes");
    let control = GrantState::default();
    assert_eq!(control.granted(), None, "nothing is granted before the ask");

    let out = ask_to_widen(
        "I need to edit src/parser.rs to make the fix you asked for",
        Some(&mut gate),
        Some(&control),
        PromptDisposition::Explain,
    )
    .await;

    assert!(out.contains("widened this turn"), "got: {out}");
    assert_eq!(
        control.granted(),
        Some(PromptDisposition::Act),
        "the operator's yes is what widens the turn"
    );
    // The operator must see the model's reason, not a bare "may I?".
    let asked = gate.asked.lock().expect("asked").join("\n");
    assert!(
        asked.contains("I need to edit src/parser.rs"),
        "got: {asked}"
    );
}

/// The model asks and the operator declines. Nothing moves, and the model is
/// told what to do instead — a refusal with no next move is the double-bind
/// this whole feature exists to break.
#[tokio::test]
async fn an_operator_no_leaves_the_turn_exactly_where_it_was() {
    let mut gate = ScriptedOperator::answering("no");
    let control = GrantState::default();

    let out = ask_to_widen(
        "I want to run the test suite",
        Some(&mut gate),
        Some(&control),
        PromptDisposition::Explain,
    )
    .await;

    assert_eq!(out, DispositionRequestVerdict::Denied.model_message());
    assert_eq!(control.granted(), None, "a no must not widen anything");
}

/// **The attack this feature would otherwise open.** Everything short of a
/// plain yes fails closed, including a hedge, a question back, and a
/// conditional grant this seam cannot honour.
#[tokio::test]
async fn nothing_short_of_a_plain_yes_widens_the_turn() {
    for answer in [
        "",
        "maybe",
        "not yet",
        "why?",
        "yes if you only touch the test file",
        "no, explain first",
    ] {
        let mut gate = ScriptedOperator::answering(answer);
        let control = GrantState::default();
        let out = ask_to_widen(
            "I need to write a file",
            Some(&mut gate),
            Some(&control),
            PromptDisposition::Explain,
        )
        .await;
        assert_eq!(
            control.granted(),
            None,
            "{answer:?} must not widen the turn (got: {out})"
        );
    }
}

/// A cancel, an exit, EOF, or an input failure each keep their own honest
/// message and never widen. Reusing `request_user_input`'s vocabulary means a
/// deliberate operator cancel is never reported as "headless".
#[tokio::test]
async fn every_non_answer_outcome_is_honest_and_grants_nothing() {
    for outcome in [
        HumanQuestionOutcome::Unavailable,
        HumanQuestionOutcome::Cancelled,
        HumanQuestionOutcome::ExitRequested,
        HumanQuestionOutcome::InputClosed,
        HumanQuestionOutcome::InputFailed,
    ] {
        let mut gate = ScriptedOperator::with_outcome(outcome.clone());
        let control = GrantState::default();
        let out = ask_to_widen(
            "I need to write a file",
            Some(&mut gate),
            Some(&control),
            PromptDisposition::Explain,
        )
        .await;
        assert_eq!(control.granted(), None, "{outcome:?} must grant nothing");
        assert!(!out.is_empty(), "{outcome:?} must say something honest");
    }
}

/// Headless — the piped / eval / wyvern path. No operator means no widening,
/// and critically no hang: the tool returns a recoverable message the model
/// can act on.
#[tokio::test]
async fn a_headless_session_is_told_plainly_rather_than_left_hanging() {
    let control = GrantState::default();
    let out = ask_to_widen(
        "I need to write a file",
        None,
        Some(&control),
        PromptDisposition::Explain,
    )
    .await;
    assert_eq!(out, DispositionRequestVerdict::NoOperator.model_message());
    assert_eq!(control.granted(), None);

    // And with no control at all — nowhere to record a grant, so there is
    // nothing to ask about. It must not open a prompt whose answer could not
    // be honoured.
    let mut gate = ScriptedOperator::answering("yes");
    let out = ask_to_widen(
        "I need to write a file",
        Some(&mut gate),
        None,
        PromptDisposition::Explain,
    )
    .await;
    assert_eq!(out, DispositionRequestVerdict::NoOperator.model_message());
    assert!(
        gate.asked.lock().expect("asked").is_empty(),
        "the operator must not be interrupted by a question that cannot be honoured"
    );
}

/// An unjustified request is refused before the operator is disturbed, and the
/// message says how to fix it — the failure mode #2051 flags in its postscript
/// was a schema rejection the model apologised for in prose.
#[tokio::test]
async fn an_unjustified_request_never_reaches_the_operator() {
    let mut gate = ScriptedOperator::answering("yes");
    let control = GrantState::default();
    let out = ask_to_widen(
        "   ",
        Some(&mut gate),
        Some(&control),
        PromptDisposition::Explain,
    )
    .await;

    assert!(out.contains("'justification' is required"), "got: {out}");
    assert!(
        out.contains("Call it again"),
        "the model needs the repair: {out}"
    );
    assert!(gate.asked.lock().expect("asked").is_empty());
    assert_eq!(control.granted(), None);
}

/// `Ask` is terminal: its decisions are not locked, so no justification buys
/// execution authority. The tool is refused before it runs, by the disposition
/// boundary itself.
#[tokio::test]
async fn an_ask_turn_cannot_buy_its_way_out_with_a_justification() {
    let mut gate = ScriptedOperator::answering("yes");
    let control = GrantState::default();
    let out = ask_to_widen(
        "I need to proceed",
        Some(&mut gate),
        Some(&control),
        PromptDisposition::Ask,
    )
    .await;

    assert!(
        out.contains("unavailable under the current prompt disposition"),
        "an Ask turn must not run this tool at all: {out}"
    );
    assert_eq!(control.granted(), None);
    assert!(gate.asked.lock().expect("asked").is_empty());
}

/// The grant is what widens, and only the dispatcher applies it. A control
/// that already holds an operator's yes lets a previously-refused tool run;
/// without it the same call is refused.
#[tokio::test]
async fn the_stored_grant_is_what_admits_a_previously_refused_tool() {
    let ws = tempfile::TempDir::new().expect("tempdir");
    let granted = GrantState::default();
    granted.grant(PromptDisposition::Act).expect("grant");

    let refused = execute_tool_with_collaborators(
        "write_file",
        &serde_json::json!({ "path": "notes.txt", "content": "hi" }),
        &ws.path().to_string_lossy(),
        false,
        20,
        &Caveats::top(),
        &mut NoMcp,
        ToolCollaborators::default(),
        false,
        PromptDisposition::Explain,
        None,
    )
    .await
    .expect("dispatch");
    assert!(
        refused.contains("unavailable under the current prompt disposition"),
        "without a grant the write is refused: {refused}"
    );

    let allowed = execute_tool_with_collaborators(
        "write_file",
        &serde_json::json!({ "path": "notes.txt", "content": "hi" }),
        &ws.path().to_string_lossy(),
        false,
        20,
        &Caveats::top(),
        &mut NoMcp,
        ToolCollaborators {
            disposition_request_control: Some(&granted as &dyn DispositionRequestControl),
            ..Default::default()
        },
        false,
        PromptDisposition::Explain,
        None,
    )
    .await
    .expect("dispatch");
    assert!(
        !allowed.contains("unavailable under the current prompt disposition"),
        "the operator's grant admits the tool: {allowed}"
    );
}
