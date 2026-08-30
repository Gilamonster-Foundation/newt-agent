//! **The one terminal adapter** — render a `SurfaceInteraction` on a sealed
//! window and report what the operator did.
//!
//! Moved down from `newt-tui` by F0a (#1922). It had always been the ONE
//! place a `PromptWindow` becomes a rendered prompt and a read, but it sat in
//! `newt-tui` and was `pub(crate)` there, so the `newt-cli` command flows had
//! no route to the typed path. #1911 named that as the obstacle and left the
//! five sites it migrated holding raw question strings.
//!
//! **The route is not a new trait.** Everything this function needs already
//! lives here — [`crate::markup::plain`], [`crate::tty::modal`],
//! [`PromptWindow`], [`SurfaceInteraction`], [`HumanQuestionOutcome`] — and
//! `newt-core` already owns the rest of the `interaction_*` family. The
//! adapter was the only member living upstairs. Shared behaviour moves DOWN
//! into the minimal layer; that is the same rule that brought `Echo` here in
//! D1b-1 and `interaction_form` in D1b-2, and it is why no third `Console` is
//! needed to reach the typed path from `newt-cli`.
//!
//! What did NOT come down is the slash-command back-out. "A leading slash is
//! a TUI command, not an answer" is chat-surface policy — `newt dock approve`
//! has no chat prompt to send you back to — so it stays in `newt-tui` as a
//! wrapper around this.

use crate::interaction_surface::SurfaceInteraction;
use crate::markup::plain;
use crate::tty::{read_prompt_window_line, Echo, PromptLine, PromptWindow, MODAL_INPUT_GLYPH};
use crate::HumanQuestionOutcome;
use newt_interaction::{ControlKind, InteractionDefinition};

/// **The echo policy is DERIVED, never passed in** (D1b-1, #1892).
///
/// A caller that had to remember "this one is a secret" would eventually
/// forget, and the failure is silent and permanent: the key lands in the
/// scrollback. The definition already says which controls are secret, so it
/// decides — and the one terminal adapter cannot be asked to echo a secret
/// because nothing offers it the choice.
///
/// **Any** secret control masks the whole read, rather than only a definition
/// whose sole control is secret. The adapter reads ONE line for the whole
/// definition, so a form mixing a username and a password would otherwise
/// echo the line that contains both. Fail closed.
#[must_use]
pub fn echo_for(definition: &InteractionDefinition) -> Echo {
    if definition
        .controls
        .iter()
        .any(|control| matches!(control.kind, ControlKind::Secret))
    {
        Echo::Stars
    } else {
        Echo::Chars
    }
}

/// Render one semantic interaction on the terminal and report what the
/// operator did.
///
/// Runs on whichever thread owns the terminal, and renders through
/// `markup::plain`, the one plain projection (C0a/C0b/C0c).
///
/// Pure of session state on purpose: the Back/Exit CONTROL side effects
/// (cancelling a turn, requesting exit) belong to the caller, so this returns
/// the typed outcome and the caller applies them.
///
/// The four outcomes are four different facts and none may be mistaken for
/// another — the distinction #1908 built into `read_line_into` and #1911
/// found three `newt-cli` sites getting wrong:
///
/// * `Answer` — a submitted line, INCLUDING an explicitly empty one.
/// * `InputClosed` — EOF. The stream ended; nobody answered. **Not** an empty
///   answer, and never consent.
/// * `Cancelled` / `ExitRequested` — the operator pressed Esc or Ctrl-C.
/// * `InputFailed` — the read errored. No human, as opposed to a human who
///   said nothing.
#[must_use]
pub fn present_on_terminal(
    window: &PromptWindow,
    interaction: &SurfaceInteraction,
) -> HumanQuestionOutcome {
    let prompt = format!(
        "{}\n{MODAL_INPUT_GLYPH}",
        plain::render(&interaction.definition)
    );
    match read_prompt_window_line(window, &prompt, echo_for(&interaction.definition)) {
        Ok(PromptLine::Line(answer)) => HumanQuestionOutcome::Answer(answer),
        Ok(PromptLine::Eof) => HumanQuestionOutcome::InputClosed,
        Ok(PromptLine::Back) => HumanQuestionOutcome::Cancelled,
        Ok(PromptLine::Exit) => HumanQuestionOutcome::ExitRequested,
        Err(_) => HumanQuestionOutcome::InputFailed,
    }
}
/// Present a choice and resolve what the operator typed against the options
/// it offered.
///
/// `None` means **no usable answer**, and deliberately collapses four
/// different reasons into one refusal: the stream closed, the operator
/// cancelled, the read failed, or what they typed matched nothing offered.
/// Every caller of this is a confirm or a menu whose fail-closed arm is the
/// same in all four cases, and the ones that need to tell them apart call
/// [`present_on_terminal`] directly.
///
/// **EOF is not consent.** That is the whole reason this exists as one
/// function rather than five copies of `read a line, then resolve it`: #1911
/// found three `newt-cli` sites getting it wrong, one of them
/// (`dgx.rs:637`) fail-closed only by accident of `matches!` falling through.
/// Here the EOF arm cannot be forgotten, because it is not written per site.
///
/// This composes two things that already exist —
/// [`present_on_terminal`] and [`crate::interaction_form::resolve`], which
/// delegates to D0's one resolver. It adds no parsing and no formatting of
/// its own, so it is not a third anything: delete it and every caller can
/// still spell the two calls by hand.
#[must_use]
pub fn resolve_on_terminal(
    window: &PromptWindow,
    definition: &InteractionDefinition,
) -> Option<newt_interaction::OptionId> {
    let interaction = SurfaceInteraction::blocking(definition.clone());
    match present_on_terminal(window, &interaction) {
        HumanQuestionOutcome::Answer(line) => {
            crate::interaction_form::resolve(definition, line.trim())
        }
        HumanQuestionOutcome::InputClosed
        | HumanQuestionOutcome::Cancelled
        | HumanQuestionOutcome::ExitRequested
        | HumanQuestionOutcome::InputFailed
        | HumanQuestionOutcome::Unavailable => None,
    }
}

/// Whether the operator affirmatively chose `yes`.
///
/// `blank` is what an EMPTY answer means, spelled at the call site so a
/// reader can see which way each prompt's default falls — the parameter D1b-3
/// introduced for the setup wizard, for the reason it found there: `[Y/n]`
/// and `[y/N]` were both written `is_yes(&ans, _)` and only one of them was
/// dangerous. **Blank may decline; it may never commit** unless the caller
/// says so explicitly and the prompt displays it.
///
/// Everything else is `false` — `no`, an unrecognised answer, EOF, a cancel,
/// a failed read. **EOF in particular can never be consent**, and it cannot
/// be forgotten here the way it was at three of the five sites #1911 found,
/// because the arm is written once rather than per caller.
#[must_use]
pub fn confirmed_on_terminal(
    window: &PromptWindow,
    definition: &InteractionDefinition,
    blank: bool,
) -> bool {
    let interaction = SurfaceInteraction::blocking(definition.clone());
    confirm_from_outcome(
        definition,
        &present_on_terminal(window, &interaction),
        blank,
    )
}

/// The confirm decision, as a pure function of the outcome.
///
/// Split from [`confirmed_on_terminal`] so the contract is testable without a
/// terminal — exhaustively, over every [`HumanQuestionOutcome`] variant
/// rather than the ones a test remembered to try. It replaces
/// `mcp_probe_cmd`'s `consent_given(bytes_read, input)`, which was the same
/// idea keyed on a byte count: `bytes_read > 0` was that site's way of saying
/// "EOF is not an answer", and the outcome enum says it in the type.
///
/// The match is exhaustive rather than `_ => false`, so a new outcome variant
/// has to be classified here instead of silently defaulting to "declined" —
/// which would be safe but would hide a case somebody needs to think about.
#[must_use]
fn confirm_from_outcome(
    definition: &InteractionDefinition,
    outcome: &HumanQuestionOutcome,
    blank: bool,
) -> bool {
    match outcome {
        HumanQuestionOutcome::Answer(line) => {
            let line = line.trim();
            if line.is_empty() {
                return blank;
            }
            crate::interaction_form::resolve(definition, line)
                .is_some_and(|picked| picked.as_str() == crate::interaction_form::YES)
        }
        // Not a human declining — a human who never answered. Every one of
        // these is refused, and none of them may be read as consent.
        HumanQuestionOutcome::InputClosed
        | HumanQuestionOutcome::Cancelled
        | HumanQuestionOutcome::ExitRequested
        | HumanQuestionOutcome::InputFailed
        | HumanQuestionOutcome::Unavailable => false,
    }
}

#[cfg(test)]
mod f0a_confirm {
    use super::confirm_from_outcome;
    use crate::interaction_form::confirm;
    use crate::HumanQuestionOutcome;

    fn q() -> newt_interaction::InteractionDefinition {
        confirm("proceed?", "", "yes, do it", "no, stop")
    }

    /// **EOF is never consent, and neither is any other non-answer.**
    ///
    /// This replaces `mcp_probe_cmd::consent_given`'s test, which asserted
    /// the same contract against a byte count. #1911 found three of five
    /// `newt-cli` sites getting this wrong — `dgx.rs:637` fail-closed only by
    /// accident of `matches!` falling through — because each site wrote the
    /// EOF arm itself. It is written once now, and this covers every variant
    /// rather than the ones a caller thought of.
    #[test]
    fn no_outcome_but_an_affirmative_answer_confirms() {
        for outcome in [
            HumanQuestionOutcome::InputClosed,
            HumanQuestionOutcome::Cancelled,
            HumanQuestionOutcome::ExitRequested,
            HumanQuestionOutcome::InputFailed,
            HumanQuestionOutcome::Unavailable,
        ] {
            // Not even when the prompt's blank default is YES: a stream that
            // ended did not press Enter.
            assert!(!confirm_from_outcome(&q(), &outcome, true), "{outcome:?}");
            assert!(!confirm_from_outcome(&q(), &outcome, false), "{outcome:?}");
        }
        // `no`, and anything unrecognised, decline too.
        for typed in ["n", "no", "maybe", "1", "yes please"] {
            let a = HumanQuestionOutcome::Answer(typed.to_string());
            assert!(!confirm_from_outcome(&q(), &a, true), "{typed:?}");
        }
    }

    /// **The anti-vacuous twin.** Everything above is a refusal, which a
    /// function returning `false` unconditionally would also satisfy — and
    /// that function would make every confirm in the tree unanswerable.
    #[test]
    fn an_affirmative_answer_does_confirm() {
        for typed in ["y", "Y", "yes", "  y  "] {
            let a = HumanQuestionOutcome::Answer(typed.to_string());
            assert!(confirm_from_outcome(&q(), &a, false), "{typed:?}");
        }
    }

    /// Blank is the one case the CALLER decides, spelled at each call site.
    /// `[Y/n]` passes `true`, `[y/N]` passes `false`, and the prompt shows
    /// which — blank may decline, and may only consent where a default is
    /// displayed and a human pressed Enter to accept it.
    #[test]
    fn blank_means_what_the_call_site_says_and_nothing_else() {
        for blank in [true, false] {
            for typed in ["", "   ", "\t"] {
                let a = HumanQuestionOutcome::Answer(typed.to_string());
                assert_eq!(confirm_from_outcome(&q(), &a, blank), blank, "{typed:?}");
            }
        }
    }
}

#[cfg(test)]
mod d1b_echo {
    use super::echo_for;
    use crate::tty::Echo;
    use newt_interaction::{
        ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition, InteractionKind,
        OptionId, Requirement, SemanticRole,
    };

    fn control(kind: ControlKind) -> Control {
        Control {
            id: ControlId::new("field").expect("valid"),
            kind,
            label: "API key".to_string(),
            requirement: Requirement::Required,
        }
    }

    fn form(controls: Vec<Control>) -> InteractionDefinition {
        InteractionDefinition::new(InteractionKind::Form, "credentials", controls)
    }

    /// A secret control masks the read, and nothing else does.
    #[test]
    fn a_secret_control_masks_the_read() {
        assert_eq!(
            echo_for(&form(vec![control(ControlKind::Secret)])),
            Echo::Stars
        );
    }

    /// **The anti-vacuous twin.** If `echo_for` returned `Stars`
    /// unconditionally the test above would pass while every ordinary prompt
    /// silently stopped echoing — a prompt that shows nothing reads as a hung
    /// terminal, which is the defect `Echo::Stars` documents avoiding.
    #[test]
    fn an_ordinary_control_still_echoes() {
        for kind in [
            ControlKind::Text,
            ControlKind::Toggle,
            ControlKind::Choice {
                options: vec![ChoiceOption {
                    id: OptionId::new("yes").expect("valid"),
                    role: SemanticRole::Allow,
                    label: "yes".to_string(),
                    key: "y".to_string(),
                    aliases: vec![],
                }],
            },
        ] {
            assert_eq!(
                echo_for(&form(vec![control(kind.clone())])),
                Echo::Chars,
                "{kind:?} is not a secret and must echo"
            );
        }
        // A form with NO controls has nothing to hide.
        assert_eq!(echo_for(&form(vec![])), Echo::Chars);
    }

    /// **Fail closed on a mixed form.** The adapter reads ONE line for the
    /// whole definition, so a form pairing a username with a password would
    /// echo the line carrying both if the policy keyed on "the only control".
    #[test]
    fn a_secret_anywhere_masks_the_whole_form() {
        let mixed = form(vec![
            control(ControlKind::Text),
            control(ControlKind::Secret),
        ]);
        assert_eq!(echo_for(&mixed), Echo::Stars, "secret last");
        let mixed = form(vec![
            control(ControlKind::Secret),
            control(ControlKind::Text),
        ]);
        assert_eq!(echo_for(&mixed), Echo::Stars, "secret first");
    }
}
