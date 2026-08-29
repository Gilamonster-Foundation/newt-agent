//! **The kind and the controls may not disagree** (#1912).
//!
//! `InteractionKind` is serialized and bound into the definition's identity, so
//! two constructors that express the same interaction under different kinds
//! give it two `ContentId`s and hand any consumer switching on the kind an
//! answer that disagrees with one switching on the shape. The tree had exactly
//! that: `agentic::tools`'s mutation confirm as `Choice`,
//! `interaction_form::confirm` as `Confirm`, plus two PRODUCTION builders
//! (`interaction_adapter`, `newt-tui`'s permission builder) hardcoding `Choice`
//! for an action list whose length they do not know.
//!
//! C0c was the first slice to try to vary behaviour by the kind and had to go
//! unconditional instead. Nothing was checking, which is why it survived A2
//! review — a doc comment saying "use Confirm for yes/no" is not a guard.
//!
//! The guard is a `debug_assert` in `InteractionDefinition::new`, so it fires
//! in every debug build for every definition anyone constructs, including ones
//! written after this. These tests prove it fires — an unverified guard is the
//! same as no guard, one indirection further away.

use newt_interaction::{
    controls_are_decision_shaped, ChoiceOption, Control, ControlId, ControlKind,
    InteractionDefinition, InteractionKind, OptionId, Requirement, SemanticRole,
};

fn option(id: &str, role: SemanticRole) -> ChoiceOption {
    ChoiceOption {
        id: OptionId::new(id).unwrap(),
        role,
        label: id.to_string(),
        key: String::new(),
        aliases: Vec::new(),
    }
}

fn control(kind: ControlKind) -> Control {
    Control {
        id: ControlId::new("decision").unwrap(),
        kind,
        label: String::new(),
        requirement: Requirement::Required,
    }
}

fn choice(options: Vec<ChoiceOption>) -> Vec<Control> {
    vec![control(ControlKind::Choice { options })]
}

/// The predicate, over the product. It reports what a SHAPE can prove, and
/// keys on roles rather than on the option count — the distinction that keeps
/// a two-way pick from reading as a decision.
#[test]
fn only_a_two_option_grant_refuse_pair_is_decision_shaped() {
    let allow_deny = choice(vec![
        option("y", SemanticRole::Allow),
        option("n", SemanticRole::Deny),
    ]);
    assert!(controls_are_decision_shaped(&allow_deny));

    // Order does not matter, and Cancel refuses just as Deny does.
    assert!(controls_are_decision_shaped(&choice(vec![
        option("n", SemanticRole::Cancel),
        option("y", SemanticRole::Allow),
    ])));

    // A two-way PICK is not a decision. This is the case that makes the rule
    // key on roles: by option count alone it is indistinguishable.
    assert!(!controls_are_decision_shaped(&choice(vec![
        option("python", SemanticRole::Value),
        option("rust", SemanticRole::Value),
    ])));
    // Three options is a set to pick from, whatever the roles say.
    assert!(!controls_are_decision_shaped(&choice(vec![
        option("y", SemanticRole::Allow),
        option("n", SemanticRole::Deny),
        option("c", SemanticRole::Cancel),
    ])));
    // A lone toggle IS a yes/no, and is deliberately NOT reported here: it is
    // equally a one-field form, and nothing in the shape says which. `Confirm`
    // admits it; the guard never demands it.
    assert!(!controls_are_decision_shaped(&[control(
        ControlKind::Toggle
    )]));
    assert!(!controls_are_decision_shaped(&[]));
}

/// **The guard fires.** A second constructor cannot reintroduce the ambiguity
/// without a test going red the first time it runs.
#[test]
#[should_panic(expected = "is InteractionKind::Confirm")]
fn a_decision_declared_as_a_choice_is_refused() {
    let _ = InteractionDefinition::new(
        InteractionKind::Choice,
        "delete everything?",
        choice(vec![
            option("y", SemanticRole::Allow),
            option("n", SemanticRole::Deny),
        ]),
    );
}

/// …and the other direction: a `Confirm` carrying a set to pick from.
#[test]
#[should_panic(expected = "carries a binary decision")]
fn a_three_way_pick_declared_as_a_confirm_is_refused() {
    let _ = InteractionDefinition::new(
        InteractionKind::Confirm,
        "which one?",
        choice(vec![
            option("a", SemanticRole::Allow),
            option("b", SemanticRole::Deny),
            option("c", SemanticRole::Cancel),
        ]),
    );
}

/// **The twin the two above need.** A guard that refused everything would
/// satisfy both `should_panic`s and be useless. These are the shapes that must
/// still construct, and they name every arm the guard deliberately permits.
#[test]
fn the_shapes_the_guard_permits_still_construct() {
    // The decision, correctly labelled.
    let _ = InteractionDefinition::new(
        InteractionKind::Confirm,
        "delete everything?",
        choice(vec![
            option("y", SemanticRole::Allow),
            option("n", SemanticRole::Deny),
        ]),
    );
    // A genuine pick from a displayed set.
    let _ = InteractionDefinition::new(
        InteractionKind::Choice,
        "which one?",
        choice(vec![
            option("a", SemanticRole::Allow),
            option("b", SemanticRole::Deny),
            option("c", SemanticRole::Cancel),
        ]),
    );
    // A toggle-carried yes/no — the shape `Choice` cannot describe, and the
    // reason `Confirm` is not redundant with it.
    let _ = InteractionDefinition::new(
        InteractionKind::Confirm,
        "delete?",
        vec![control(ControlKind::Toggle)],
    );
    // …and the same toggle as a one-field form, which is equally legitimate.
    let _ = InteractionDefinition::new(
        InteractionKind::Form,
        "remember this?",
        vec![control(ControlKind::Toggle)],
    );
    // A control-less definition claims no shape at all.
    let _ = InteractionDefinition::new(InteractionKind::Confirm, "body", Vec::new());
}
