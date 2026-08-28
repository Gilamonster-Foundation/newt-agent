//! **The legacy `Question<A>` as an `InteractionDefinition`** (A2.2, #1828).
//!
//! One lossless mapping between the prompt model already in production and
//! the renderer-neutral model A2 defines. Nothing here is wired into a
//! production path: A2.2's job is to PROVE the new model can express the
//! old one; switching the terminal and web surfaces onto it is B0's slice,
//! with its own deletion gate. `no_production_path_uses_the_adapter_yet`
//! holds that line mechanically.
//!
//! # The shape
//!
//! A `Question<A>` is a prompt plus `Vec<Action>` — ONE question offering N
//! mutually-exclusive actions, which A0's frozen golden renders as a single
//! line: `[a]llow once   [s]ession allow   [d]eny (default)`. Under A2.1's
//! model that is exactly ONE [`Control`] of kind
//! [`ControlKind::Choice`], with one [`ChoiceOption`] per action and the
//! action's meaning carried on the option.
//!
//! That the mapping falls out with no second id space and no second control
//! is the evidence for round 3's redesign, not a coincidence: the old model
//! had been describing one field with many options all along, and the
//! flat-controls shape it replaced could not have expressed this without
//! making `requirement` incoherent.
//!
//! # What "lossless" costs
//!
//! Everything a `Question` carries has a home:
//!
//! | `Question<A>` | `InteractionDefinition` |
//! |---|---|
//! | `markdown` | `markdown` |
//! | `note` | `note` |
//! | `actions[i].value` | the option's `id` (the action's WIRE name) |
//! | `actions[i].key` | the option's `key` |
//! | `actions[i].label` | the option's `label` |
//! | `actions[i].aliases` | the option's `aliases` |
//!
//! An option id is the action's wire name (`allow_once`), not its hotkey:
//! the wire name is the stable identity, the key is a presentation affordance
//! that differs per surface. Both survive because both are carried.

use newt_interaction::{
    ChoiceOption, Control, ControlId, ControlKind, InteractionDefinition, InteractionKind,
    OptionId, ProtocolError, Requirement, SemanticRole,
};

use crate::tty::{Action, Question};
use crate::PermissionAction;

/// The single control every adapted question carries.
///
/// A stable, reserved name: the definition has exactly one field, and B0
/// needs to address it without guessing.
pub const DECISION_CONTROL: &str = "decision";

/// What an action MEANS, for the option that carries it.
///
/// Derived from the action rather than declared, because the legacy type
/// has no role field — the meaning was encoded in the variant all along.
///
/// Public since B0a (#1841): `newt-tui` builds the definition directly and
/// needs the same mapping. Exposing it keeps ONE action→role table; a
/// second copy in the policy layer is the duplication this epic deletes.
#[must_use]
pub fn role_of(action: PermissionAction) -> SemanticRole {
    match action {
        PermissionAction::AllowOnce
        | PermissionAction::AllowSession
        | PermissionAction::AllowPermanent => SemanticRole::Allow,
        PermissionAction::Deny | PermissionAction::DenyAlways | PermissionAction::DenyPermanent => {
            SemanticRole::Deny
        }
        PermissionAction::Back => SemanticRole::Cancel,
        PermissionAction::Exit => SemanticRole::Exit,
    }
}

/// Recover the action a wire name denotes.
///
/// Public since B0b-1 (#1842): the interaction gate resolves an accepted
/// option back to the action it authorizes, and a second copy of this
/// table would be the duplication this epic deletes.
#[must_use]
pub fn action_for_option(wire: &str) -> Option<PermissionAction> {
    [
        PermissionAction::AllowOnce,
        PermissionAction::AllowSession,
        PermissionAction::AllowPermanent,
        PermissionAction::Deny,
        PermissionAction::DenyAlways,
        PermissionAction::DenyPermanent,
        PermissionAction::Back,
        PermissionAction::Exit,
    ]
    .into_iter()
    .find(|candidate| candidate.as_str() == wire)
}

/// Express a permission question as an interaction definition.
///
/// # Errors
///
/// [`ProtocolError::InvalidId`] when an action's wire name is not a valid
/// option id. Every `PermissionAction` wire name is, so this can only fire
/// if that frozen set gains a name outside `[A-Za-z0-9_-]`.
pub fn question_to_definition(
    question: &Question<PermissionAction>,
) -> Result<InteractionDefinition, ProtocolError> {
    let mut options = Vec::with_capacity(question.actions.len());
    for action in &question.actions {
        options.push(ChoiceOption {
            id: OptionId::new(action.value.as_str())?,
            role: role_of(action.value),
            label: action.label.clone(),
            key: action.key.clone(),
            aliases: action.aliases.clone(),
        });
    }

    let mut definition = InteractionDefinition::new(
        InteractionKind::Choice,
        question.markdown.clone(),
        vec![Control {
            id: ControlId::new(DECISION_CONTROL)?,
            kind: ControlKind::Choice { options },
            label: String::new(),
            // A permission prompt must be answered: an unanswered one denies
            // by default, which is a decision, not an absence.
            requirement: Requirement::Required,
        }],
    );
    definition.note = question.note.clone();
    Ok(definition)
}

/// Recover the permission question a definition expresses.
///
/// # Errors
///
/// [`ProtocolError::InvalidId`] when the definition is not one this adapter
/// produced: no decision control, a control of another kind, or an option
/// naming an action this build does not know. Fail closed rather than
/// returning a question missing an action — a prompt that silently lost an
/// option would offer the operator a choice its author did not write.
pub fn definition_to_question(
    definition: &InteractionDefinition,
) -> Result<Question<PermissionAction>, ProtocolError> {
    let invalid = |reason: String| ProtocolError::InvalidId {
        kind: "adapted definition",
        reason,
    };

    let [control] = definition.controls.as_slice() else {
        return Err(invalid(format!(
            "expected exactly one control, found {}",
            definition.controls.len()
        )));
    };
    if control.id.as_str() != DECISION_CONTROL {
        return Err(invalid(format!(
            "expected the `{DECISION_CONTROL}` control, found `{}`",
            control.id.as_str()
        )));
    }
    let ControlKind::Choice { options } = &control.kind else {
        return Err(invalid("the decision control is not a choice".to_string()));
    };

    let mut actions = Vec::with_capacity(options.len());
    for option in options {
        let Some(value) = action_for_option(option.id.as_str()) else {
            return Err(invalid(format!(
                "`{}` is not a permission action this build knows",
                option.id.as_str()
            )));
        };
        actions.push(Action {
            value,
            key: option.key.clone(),
            label: option.label.clone(),
            aliases: option.aliases.clone(),
        });
    }

    Ok(Question {
        markdown: definition.markdown.clone(),
        actions,
        note: definition.note.clone(),
    })
}
