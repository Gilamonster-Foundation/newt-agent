//! The model's move when the harness classified its turn wrong (#2051).
//!
//! `PromptIntake` decides the turn's disposition from a keyword lexicon before
//! the model runs, and until now that decision was terminal: a misclassified
//! turn left the model two options, comply or narrate that it is complying —
//! and a 9b local model picks the second. The harness even said so out loud
//! ("`/mode` … cannot widen an already accepted turn"), which is a strange
//! thing to tell something you then expect to stay quiet about it.
//!
//! This is the third option: **ask**. Modelled on how `codex` frames the same
//! problem — an escalation the agent *requests*, with a justification, rather
//! than a cage it explains to the user.
//!
//! # The authority rule, which is the whole design
//!
//! Widening is amplification, so it needs the human root. Nothing here lets a
//! model widen its own turn:
//!
//! - the tool cannot grant. It asks the operator through the SAME human seam a
//!   permission prompt uses ([`crate::PermissionGate::ask_question`]) — a new
//!   asking mechanism would have been a second implementation of one that
//!   already works;
//! - only a plainly affirmative human answer reaches
//!   [`DispositionRequestControl::grant`], and anything else fails closed;
//! - the dispatcher reads [`DispositionRequestControl::granted`], which is set
//!   only by an operator's answer;
//! - [`PromptDisposition::Ask`] never widens — it is terminal at the harness
//!   layer, and a turn whose decisions are not locked cannot buy execution
//!   authority with an explanation;
//! - a model-entered Plan phase still attenuates AFTER any grant, so the
//!   model's own self-clamp cannot be undone by asking.
//!
//! Absent an operator the tool degrades honestly rather than hanging, exactly
//! as `request_user_input` does headless. The piped / headless / wyvern path
//! must never block on a human who is not there.

use super::prompt_intake::PromptDisposition;

/// What the operator said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispositionRequestVerdict {
    /// The operator widened this turn to `disposition`.
    Granted(PromptDisposition),
    /// The operator said no. The turn continues under its original authority.
    Denied,
    /// No operator was available to ask (headless, piped, or a wyvern worker).
    NoOperator,
}

impl DispositionRequestVerdict {
    /// The model-facing result text.
    ///
    /// Every arm ends by telling the model what to do NEXT, because a refusal
    /// with no next move is the double-bind this whole module exists to break.
    #[must_use]
    pub fn model_message(&self) -> String {
        match self {
            Self::Granted(disposition) => format!(
                "The operator widened this turn to `{}`. Continue with the work they asked for.",
                disposition.as_str()
            ),
            Self::Denied => "The operator declined to widen this turn. Do the most useful thing \
                             you can within it — answer, or gather evidence — and do not ask again \
                             this turn."
                .to_string(),
            Self::NoOperator => "No operator is available this session, so this turn cannot be \
                                 widened. Do the most useful thing you can within it and state \
                                 plainly what you could not do."
                .to_string(),
        }
    }
}

/// Session-local state behind `request_disposition`.
///
/// Mirrors [`super::PlanModeControl`]: core owns the tool, the embedding
/// session owns the state, so one session (or one concurrent test) cannot
/// change another's. The asymmetry with `PlanModeControl` is deliberate and is
/// the security property — a model may enter Plan by itself because that only
/// attenuates, but it may not leave a read-only disposition without a human.
pub trait DispositionRequestControl: Send + Sync {
    /// The disposition the OPERATOR granted for this turn, if any.
    ///
    /// The dispatcher consults this before every tool call, so a grant takes
    /// effect immediately for later calls in the same model tool round.
    fn granted(&self) -> Option<PromptDisposition>;

    /// Record an operator-approved widening for this turn.
    ///
    /// Called only after [`PermissionGate::ask_question`] has returned an
    /// affirmative answer from a human. Implementations own session-local
    /// state and must never grant on their own.
    ///
    /// [`PermissionGate::ask_question`]: crate::PermissionGate::ask_question
    fn grant(&self, disposition: PromptDisposition) -> Result<(), String>;
}

/// The words that mean yes, as data rather than a hardcoded `match`.
///
/// Deliberately narrow, and deliberately NOT a general sentiment read: this
/// decides an authority widening, so anything that is not plainly a yes is a
/// no. A 9b model is not the reader here — the operator is — but the same
/// three-Cs rule applies, and a locale or house style that says "aye" is a
/// data change.
pub const AFFIRMATIVE_ANSWERS: &[&str] = &[
    "y",
    "yes",
    "yeah",
    "yep",
    "yup",
    "ok",
    "okay",
    "sure",
    "go",
    "go ahead",
    "do it",
    "allow",
    "grant",
    "approved",
    "approve",
    "permit",
    "please do",
];

/// Whether an operator's free-text answer is plainly a yes.
///
/// Fails CLOSED: an empty answer, a hedge, a question back, or anything not in
/// [`AFFIRMATIVE_ANSWERS`] is a refusal. Silence is never a grant — the
/// Authority Register's fail-closed rule, applied to a text box.
#[must_use]
pub fn answer_is_affirmative(answer: &str) -> bool {
    let normalized = answer
        .trim()
        .trim_end_matches(['.', '!', ','])
        .to_ascii_lowercase();
    AFFIRMATIVE_ANSWERS.contains(&normalized.as_str())
}

/// The question put to the operator. It names the model's reason and the
/// exact authority at stake, because an operator cannot review a request
/// phrased as "may I do more?".
#[must_use]
pub fn operator_question(justification: &str) -> String {
    format!(
        "The model asks to widen this turn to full execution authority.\n\
         Its reason: {justification}\n\
         Allow it for this turn? (yes / no)"
    )
}

/// The disposition a tool call actually runs under.
///
/// `granted` is the operator's answer; `validated` is what prompt intake
/// decided. The grant wins, with two exceptions that are the module's
/// invariants: `Ask` is terminal and never widens, and a grant may only widen
/// — an operator answer is not a route to a *narrower* disposition than intake
/// validated, because narrowing already has its own paths (`/mode`,
/// `enforce_read_only`, the plan clamp) and mixing them here would give one
/// seam two meanings.
#[must_use]
pub fn effective_disposition(
    validated: PromptDisposition,
    granted: Option<PromptDisposition>,
) -> PromptDisposition {
    let Some(granted) = granted else {
        return validated;
    };
    if validated == PromptDisposition::Ask {
        return validated;
    }
    if authority_rank(granted) > authority_rank(validated) {
        granted
    } else {
        validated
    }
}

/// A total order on how much a disposition may do, used only to prove a grant
/// widens rather than narrows.
///
/// This is deliberately NOT a general capability lattice — `tool_allowed`
/// remains the authority boundary, and this ranking never decides whether a
/// specific tool runs. `Ask` is lowest because it may run no tool at all.
fn authority_rank(disposition: PromptDisposition) -> u8 {
    match disposition {
        PromptDisposition::Ask => 0,
        // Explain, Research, and Plan are siblings, not a ladder: each has a
        // read-only catalog the others do not exactly contain. Ranking them
        // equal means a grant can never shuffle between them, only rise to Act.
        PromptDisposition::Explain | PromptDisposition::Research | PromptDisposition::Plan => 1,
        PromptDisposition::Act => 2,
    }
}

/// The `request_disposition` tool definition.
///
/// Advertised always, like `request_user_input`: a model must always be able
/// to ask, and it degrades honestly when no operator is there. The description
/// is written for a 9b local model — it says what to do, in what order, and
/// what NOT to do, because the failure this replaces was a small model
/// narrating its situation instead of acting on it.
#[must_use]
pub fn request_disposition_tool_definition() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "request_disposition",
            "description": "Ask the operator to let this turn do more than the harness \
                            allowed. The harness guessed what kind of turn this is from \
                            the operator's words, and it guesses wrong sometimes. If the \
                            work plainly needs an edit, a command, or another tool you \
                            cannot reach, call this with a short reason and continue from \
                            the answer. Do NOT write a message to the operator explaining \
                            that you are restricted — call this instead. Only the operator \
                            can widen the turn; this tool asks them, it does not grant \
                            anything itself. If no operator is available you will be told \
                            so, and should then do what you can and say plainly what you \
                            could not do.",
            // `additionalProperties: false` without a top-level `strict: true`
            // is a SILENT strictness downgrade on the Responses wire — the
            // provider treats the schema as advisory. `responses_wire_validation`
            // rejects that shape, and the flattener carries this marker up.
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {
                    "justification": {
                        "type": "string",
                        "description": "One sentence: what you need to do, and why the \
                                        current turn cannot do it."
                    }
                },
                "required": ["justification"],
                "additionalProperties": false
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_operator_grant_widens_a_read_only_turn() {
        assert_eq!(
            effective_disposition(PromptDisposition::Explain, Some(PromptDisposition::Act)),
            PromptDisposition::Act
        );
    }

    #[test]
    fn no_grant_changes_nothing() {
        for validated in [
            PromptDisposition::Ask,
            PromptDisposition::Act,
            PromptDisposition::Explain,
            PromptDisposition::Research,
            PromptDisposition::Plan,
        ] {
            assert_eq!(effective_disposition(validated, None), validated);
        }
    }

    /// `Ask` is terminal at the harness layer: its decisions are not locked, so
    /// no explanation buys execution authority. This is the arm most worth
    /// pinning — it is the one a plausible-sounding justification would attack.
    #[test]
    fn ask_never_widens_even_with_a_grant() {
        for granted in [
            PromptDisposition::Act,
            PromptDisposition::Explain,
            PromptDisposition::Research,
            PromptDisposition::Plan,
        ] {
            assert_eq!(
                effective_disposition(PromptDisposition::Ask, Some(granted)),
                PromptDisposition::Ask,
                "a pending clarification must not be answerable with a grant"
            );
        }
    }

    /// A grant only ever widens. Narrowing has its own seams; letting this one
    /// narrow too would give a single mechanism two meanings, which is how
    /// `color` became an I/O-ownership signal (#1312).
    #[test]
    fn a_grant_never_narrows_an_act_turn() {
        for granted in [
            PromptDisposition::Explain,
            PromptDisposition::Research,
            PromptDisposition::Plan,
        ] {
            assert_eq!(
                effective_disposition(PromptDisposition::Act, Some(granted)),
                PromptDisposition::Act
            );
        }
    }

    /// The read-only dispositions are siblings, not a ladder. A grant must not
    /// shuffle a Research turn into a Plan turn (or the reverse) — each has a
    /// catalog the other does not contain, so a sideways move would silently
    /// add tools nobody approved.
    #[test]
    fn a_grant_cannot_shuffle_between_read_only_siblings() {
        for (validated, granted) in [
            (PromptDisposition::Explain, PromptDisposition::Research),
            (PromptDisposition::Research, PromptDisposition::Plan),
            (PromptDisposition::Plan, PromptDisposition::Explain),
        ] {
            assert_eq!(
                effective_disposition(validated, Some(granted)),
                validated,
                "{validated:?} must not become {granted:?}"
            );
        }
    }

    #[test]
    fn every_verdict_tells_the_model_what_to_do_next() {
        let messages = [
            DispositionRequestVerdict::Granted(PromptDisposition::Act).model_message(),
            DispositionRequestVerdict::Denied.model_message(),
            DispositionRequestVerdict::NoOperator.model_message(),
        ];
        for message in messages {
            assert!(
                message.contains("Continue") || message.contains("Do the most useful thing"),
                "a verdict with no next move recreates the double-bind: {message}"
            );
        }
    }

    #[test]
    fn only_a_plain_yes_is_a_grant() {
        for yes in ["y", "yes", "Yes.", " OK ", "go ahead", "Do it!"] {
            assert!(answer_is_affirmative(yes), "{yes:?} should grant");
        }
    }

    /// Fails closed. Every one of these is a human who did NOT say yes, and an
    /// authority widening is the last place to guess generously.
    #[test]
    fn anything_short_of_a_yes_is_a_refusal() {
        for no in [
            "",
            "   ",
            "no",
            "not yet",
            "maybe",
            "why do you need it?",
            "yes if you only touch the test file",
            "no, explain first",
            "yesterday",
        ] {
            assert!(!answer_is_affirmative(no), "{no:?} must not grant");
        }
    }

    /// A conditional yes ("yes, but only the test file") is a refusal here on
    /// purpose: this seam grants a whole disposition, and it has no way to
    /// carry a condition. The operator can restate an unconditional yes, or
    /// answer the model's question directly.
    #[test]
    fn the_question_gives_the_operator_what_they_need_to_decide() {
        let question = operator_question("I need to edit src/main.rs to fix the parser");
        assert!(
            question.contains("I need to edit src/main.rs"),
            "{question}"
        );
        assert!(question.contains("full execution authority"), "{question}");
        assert!(question.contains("this turn"), "{question}");
    }

    #[test]
    fn the_tool_tells_the_model_to_ask_rather_than_narrate() {
        let definition = request_disposition_tool_definition();
        let description = definition["function"]["description"]
            .as_str()
            .expect("description");
        assert!(
            description.contains("Do NOT write a message to the operator explaining"),
            "the tool must displace the narration, not sit beside it"
        );
        assert!(
            description.contains("Only the operator can widen the turn"),
            "the model must not read this as a self-grant"
        );
        assert_eq!(
            definition["function"]["parameters"]["required"],
            serde_json::json!(["justification"]),
            "an unjustified widening request is not reviewable by the operator"
        );
        // A schema that forbids extra properties must say so strictly, or the
        // Responses wire silently relaxes it (`responses_wire_validation`).
        assert_eq!(
            definition["function"]["parameters"]["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(
            definition["function"]["strict"],
            serde_json::json!(true),
            "additionalProperties:false without strict:true is a silent downgrade"
        );
    }
}
