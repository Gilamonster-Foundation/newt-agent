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
