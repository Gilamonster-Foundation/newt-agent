//! **The wizard-field vocabulary** — the three prompt shapes every form in
//! this tree actually asks, built once (D1b-2, #1903).
//!
//! `crew_form` grew private `text_field` / `confirm_field` builders in D1a,
//! and the setup wizard needs the same two. Writing them a second time in
//! `newt-tui/src/setup/` is precisely the sprawl the reuse discipline names:
//! two spellings of "a free-text field with a default", drifting apart the
//! way `is_yes` drifted into three implementations. So they move DOWN into
//! the layer that already owns definition machinery, and both callers use
//! these.
//!
//! ## What is here, and what deliberately is not
//!
//! Building a definition, and nothing else. No I/O, no `PromptWindow`, no
//! resolution: an answer is resolved by
//! [`newt_interaction::binding::resolve_typed`], the one resolver D0 (#1878)
//! consolidated onto, and these builders hand it the options to resolve
//! against. A second parser here would put a row straight back into the
//! ratchet category D0 emptied.
//!
//! ## Defaults: what an advertised bracket may and may not decide
//!
//! A **field** default is the operator's own current value, displayed. Blank
//! keeps what is on screen; the machine decides nothing, so
//! [`text_field`]'s hint says `[current]` and Enter keeps it.
//!
//! A **decision** default is different. `markup::plain` renders a toggle as
//! `[y/n]`, never `[Y/n]`, because the epic's acceptance criterion is that a
//! surface never chooses a decision for the operator — and D1a found that was
//! not a style question: `is_yes(&ans, true)` plus a `StdinConsole` that
//! returned `""` at EOF meant a short pipe WROTE THE CREW FILE on running out
//! of input. So [`confirm`] offers `yes`/`no` as real options and advertises
//! no default; blank resolves to nothing and the caller re-asks.

use newt_interaction::{
    ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition, InteractionKind,
    OptionId, Requirement, SemanticRole,
};

/// The control id a single-field prompt answers.
///
/// One name across every form, so a responder never has to ask which field
/// it is answering when the definition offers exactly one.
pub const FIELD_CONTROL: &str = "field";

/// The `yes` option's stable id, for a caller mapping a resolution back.
pub const YES: &str = "yes";
/// The `no` option's stable id.
pub const NO: &str = "no";

/// One unlabelled control of `kind`.
///
/// Unlabelled on purpose: `markup::plain` gives a labelled control its own
/// `label:` line, and a single-field prompt has already said what it is
/// asking in the body. This is the shape `permissions::free_text_form` uses.
fn field(kind: ControlKind) -> Control {
    Control {
        id: ControlId::new(FIELD_CONTROL)
            .expect("`field` is a valid control id (non-empty, [A-Za-z0-9_-]); it is a const"),
        kind,
        label: String::new(),
        requirement: Requirement::Required,
    }
}

/// A free-text field: what is being asked, and an optional hint beneath it.
///
/// `hint` lands in `note`, which every surface places as a subordinate line —
/// the right home for `[current] — Enter keeps it` and for a retry reason.
/// An empty hint contributes no line at all.
#[must_use]
pub fn text_field(body: impl Into<String>, hint: impl Into<String>) -> InteractionDefinition {
    let hint = hint.into();
    InteractionDefinition {
        note: (!hint.is_empty()).then_some(hint),
        ..InteractionDefinition::new(
            InteractionKind::Prompt,
            body,
            vec![field(ControlKind::Text)],
        )
    }
}

/// A secret field: same shape, but the control carries no value and no
/// surface can render one.
///
/// Kept beside [`text_field`] rather than in the wizard, because "which kind
/// of control hides its input" is a property of the vocabulary, not of one
/// caller. `present_on_terminal` derives its `Echo` from this kind, and
/// `plan_presentation` derives the required `secret-input` capability from
/// it — so choosing this builder is the whole of what makes a prompt secret.
#[must_use]
pub fn secret_field(label: impl Into<String>) -> InteractionDefinition {
    InteractionDefinition::new(
        InteractionKind::Prompt,
        String::new(),
        vec![Control {
            label: label.into(),
            ..field(ControlKind::Secret)
        }],
    )
}

/// A yes/no decision, offering both and advertising neither as the default.
///
/// `yes_label` / `no_label` carry their accelerator inside the text, because
/// `markup::plain` brackets the FIRST occurrence of the key in the label —
/// `y` in `yes, write it` renders `[y]es, write it`. A label with no
/// occurrence of its key renders no bracket at all, which would advertise an
/// input the operator cannot see.
///
/// Resolution is `resolve_typed`'s, including its rules: canonical id and
/// key before any alias, and ambiguity refuses. The caller maps [`YES`] /
/// [`NO`] back to its own meaning and supplies its own behaviour for a blank
/// answer — nothing here reads a `SemanticRole` to decide that, because a
/// role is author-assigned (A3).
#[must_use]
pub fn confirm(
    body: impl Into<String>,
    hint: impl Into<String>,
    yes_label: &str,
    no_label: &str,
) -> InteractionDefinition {
    let option = |wire: &str, role, key: &str, text: &str, alias: &str| ChoiceOption {
        id: OptionId::new(wire).expect("`yes`/`no` are valid option ids; they are consts"),
        role,
        label: text.to_string(),
        key: key.to_string(),
        aliases: vec![alias.to_string()],
    };
    let hint = hint.into();
    InteractionDefinition {
        note: (!hint.is_empty()).then_some(hint),
        ..InteractionDefinition::new(
            InteractionKind::Confirm,
            body,
            vec![field(ControlKind::Choice {
                options: vec![
                    option(YES, SemanticRole::Allow, "y", yes_label, "Y"),
                    option(NO, SemanticRole::Cancel, "n", no_label, "N"),
                ],
            })],
        )
    }
}

/// Resolve a typed answer against a single-field definition's options.
///
/// A thin, honest convenience over [`newt_interaction::binding::resolve_typed`]
/// — it finds the one choice control and delegates. It resolves nothing
/// itself, so it is not a second parser: strip it and the rules are
/// unchanged.
///
/// `None` when the definition has no choice control, or when the answer
/// resolves to nothing. **What that means is the caller's**, deliberately:
/// options can come from untrusted markup and roles are author-assigned, so
/// reading one here to pick a failure mode would let the author choose it.
#[must_use]
pub fn resolve(definition: &InteractionDefinition, answer: &str) -> Option<OptionId> {
    definition
        .controls
        .iter()
        .find_map(|control| match &control.kind {
            ControlKind::Choice { options } => {
                newt_interaction::binding::resolve_typed(options, answer)
            }
            _ => None,
        })
}

#[cfg(test)]
mod d1b2 {
    use super::{confirm, resolve, secret_field, text_field, FIELD_CONTROL, NO, YES};
    use crate::markup::plain;
    use newt_interaction::{ControlKind, InteractionKind, Requirement};

    #[test]
    fn a_text_field_shows_its_question_and_its_hint() {
        let d = text_field("Ollama host", "[http://127.0.0.1:11434] — Enter keeps it");
        assert_eq!(
            plain::render(&d),
            "Ollama host\n[http://127.0.0.1:11434] — Enter keeps it"
        );
        assert_eq!(d.kind, InteractionKind::Prompt);
        assert!(matches!(d.controls[0].kind, ControlKind::Text));
        assert_eq!(d.controls[0].id.as_str(), FIELD_CONTROL);
    }

    /// An empty hint contributes no line — so a prompt with nothing to add
    /// is not padded with a blank row.
    #[test]
    fn an_empty_hint_adds_no_line() {
        assert_eq!(
            plain::render(&text_field("Endpoint URL", "")),
            "Endpoint URL"
        );
        assert!(text_field("Endpoint URL", "").note.is_none());
    }

    /// **No advertised default.** `markup::plain` renders a toggle `[y/n]`
    /// and never `[Y/n]`, because a rendered default is how a surface chooses
    /// one by accident. A confirm built here must not reintroduce that.
    #[test]
    fn a_confirm_advertises_no_default() {
        let shown = plain::render(&confirm("Use it?", "", "yes, use it", "no, paste a key"));
        assert!(shown.contains("[y]es, use it"), "{shown}");
        assert!(shown.contains("[n]o, paste a key"), "{shown}");
        for advertised in ["[Y/n]", "[y/N]", "[Y]", "[N]"] {
            assert!(!shown.contains(advertised), "{advertised} in {shown}");
        }
    }

    /// Resolution goes through `resolve_typed`, including its refusals.
    #[test]
    fn a_confirm_resolves_yes_and_no_and_refuses_the_rest() {
        let d = confirm("Use it?", "", "yes, use it", "no, thanks");
        for (input, want) in [("y", YES), ("Y", YES), ("yes", YES), ("n", NO), ("no", NO)] {
            assert_eq!(
                resolve(&d, input)
                    .as_ref()
                    .map(newt_interaction::OptionId::as_str),
                Some(want),
                "{input:?}"
            );
        }
        // Blank and garbage decide NOTHING; the caller re-asks.
        for input in ["", " ", "maybe", "sure", "1"] {
            assert!(resolve(&d, input).is_none(), "{input:?} must not resolve");
        }
    }

    /// **The anti-vacuous twin for `resolve`.** Every assertion above that
    /// matters is either "resolves to X" or "resolves to nothing", and the
    /// second shape passes for a `resolve` that always returns `None` —
    /// which would make every confirm in the tree unanswerable. The first
    /// shape above is that twin; this pins the other direction, that
    /// `resolve` finds nothing when there is no choice control at all rather
    /// than reaching into some other definition's options.
    #[test]
    fn resolve_finds_nothing_in_a_definition_with_no_choice() {
        assert!(resolve(&text_field("Endpoint URL", ""), "y").is_none());
        assert!(resolve(&secret_field("API key"), "y").is_none());
        // ...and it DOES find something when a choice is present, so the two
        // negatives above are real.
        assert!(resolve(&confirm("Use it?", "", "yes", "no"), "y").is_some());
    }

    /// A secret field says it is one, and carries nowhere to put a value.
    #[test]
    fn a_secret_field_shows_its_label_and_no_value() {
        let d = secret_field("API key");
        assert_eq!(plain::render(&d), "API key: (secret, not echoed)");
        assert!(matches!(d.controls[0].kind, ControlKind::Secret));
        assert_eq!(d.controls[0].requirement, Requirement::Required);
        // The capability is DERIVED from the kind, never hand-written here.
        assert!(d.features.is_empty());
    }
}
