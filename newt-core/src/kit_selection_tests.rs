//! Capture-shape controls; these do not claim a model phase executed.
use super::*;
use crate::config::PickVia;

fn pick() -> ProfilePick {
    ProfilePick {
        name: "review".into(),
        via: PickVia::Profile,
    }
}

#[test]
fn captured_self_review_defaults_without_a_knob_table() {
    let role = TechniqueSource::Role {
        name: "reviewer".into(),
    };
    let captured = CapturedTechniques::capture(None, [(role, vec!["self_review".into()])]).unwrap();
    assert_eq!(captured.self_review().unwrap().max_rounds, 3);
}

#[test]
fn captured_self_review_zero_does_not_deselect() {
    let profile: ProfileConfig = toml::from_str("[self_review]\nmax_rounds = 0").unwrap();
    let persona = TechniqueSource::Persona {
        name: "reviewer".into(),
    };
    let captured = CapturedTechniques::capture(
        Some((&pick(), &profile)),
        [(persona, vec!["self_review".into()])],
    )
    .unwrap();
    assert!(captured.enables("self_review"));
    assert_eq!(captured.self_review().unwrap().max_rounds, 0);
}

#[test]
fn captured_self_review_deduplicates_but_preserves_provenance() {
    let profile: ProfileConfig =
        toml::from_str("techniques = [\"self_review\", \"self_review\"]").unwrap();
    let persona = TechniqueSource::Persona {
        name: "reviewer".into(),
    };
    let step = TechniqueSource::PlanStep {
        id: "inspect".into(),
    };
    let captured = CapturedTechniques::capture(
        Some((&pick(), &profile)),
        [
            (persona.clone(), vec!["self_review".into()]),
            (persona, vec!["self_review".into()]),
            (step, vec!["self_review".into()]),
        ],
    )
    .unwrap();
    assert_eq!(captured.sources("self_review").len(), 3);
    assert_eq!(
        serde_json::to_value(&captured).unwrap()["selections"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn captured_self_review_retains_original_knobs_after_profile_edit() {
    let mut profile: ProfileConfig =
        toml::from_str("techniques = [\"self_review\"]\n[self_review]\nmax_rounds = 2").unwrap();
    let captured = CapturedTechniques::capture(Some((&pick(), &profile)), []).unwrap();
    profile.self_review = Some(SelfReviewKnobs { max_rounds: 9 });
    profile.techniques.clear();
    assert!(captured.enables("self_review"));
    assert_eq!(captured.self_review().unwrap().max_rounds, 2);
}

#[test]
fn captured_self_review_absent_selection_stays_absent() {
    let captured = CapturedTechniques::capture(None, []).unwrap();
    assert_eq!(captured.self_review(), None);
    assert!(!captured.enables("self_review"));
}

#[test]
fn captured_self_review_rejects_constructed_unknown_selector() {
    let source = TechniqueSource::PlanStep {
        id: "inspect".into(),
    };
    let error =
        CapturedTechniques::capture(None, [(source, vec!["not_a_technique".into()])]).unwrap_err();
    assert!(error.contains("not_a_technique"));
}

#[test]
fn captured_self_review_overflow_rejects_loading() {
    let invalid = "[self_review]\nmax_rounds = 4294967296";
    assert!(toml::from_str::<ProfileConfig>(invalid).is_err());
    let largest: ProfileConfig = toml::from_str("[self_review]\nmax_rounds = 4294967295").unwrap();
    assert_eq!(largest.self_review_knobs().max_rounds, u32::MAX);
}

/// Existing technique selection alone must not activate independent phase
/// admission, failed-operation recovery, or a self-review request.
#[test]
fn captured_self_review_legacy_only_profile_does_not_activate_review() {
    let profile: ProfileConfig =
        toml::from_str("techniques = [\"knowledge_base\", \"verify_gate\", \"retry\"]").unwrap();
    let captured = CapturedTechniques::capture(Some((&pick(), &profile)), []).unwrap();
    assert!(!captured.is_empty());
    assert_eq!(captured.self_review(), None);
    assert!(!captured.enables("self_review"));
}

#[test]
fn captured_self_review_child_inherits_inactive_parent_knobs_and_real_context() {
    let profile: ProfileConfig = toml::from_str("[self_review]\nmax_rounds = 1").unwrap();
    let pick = crate::config::ProfilePick {
        name: "parent".into(),
        via: crate::config::PickVia::Profile,
    };
    let parent = CapturedTechniques::capture(Some((&pick, &profile)), []).unwrap();
    assert!(parent.has_context());
    assert_eq!(parent.profile(), Some(&pick));
    assert!(parent.self_review().is_none());
    let child = parent
        .for_child(
            None,
            [(
                TechniqueSource::Role {
                    name: "reviewer".into(),
                },
                vec!["self_review".into()],
            )],
            Some("canonical-step"),
        )
        .unwrap();
    assert_eq!(child.self_review().unwrap().max_rounds, 1);
    assert_eq!(child.plan_step(), Some("canonical-step"));
    assert_eq!(
        child.sources("self_review"),
        &[TechniqueSource::Role {
            name: "reviewer".into()
        }]
    );
    assert!(parent.self_review().is_none());
}

#[test]
fn captured_self_review_inherited_profile_does_not_invent_plan_selection() {
    let profile: ProfileConfig =
        toml::from_str("techniques = [\"self_review\"]\n[self_review]\nmax_rounds = 2").unwrap();
    let pick = crate::config::ProfilePick {
        name: "parent".into(),
        via: crate::config::PickVia::Profile,
    };
    let parent = CapturedTechniques::capture(Some((&pick, &profile)), []).unwrap();
    let child = parent
        .for_child(None, [], Some("actual-plan-step"))
        .unwrap();
    assert_eq!(child.self_review().unwrap().max_rounds, 2);
    assert_eq!(child.plan_step(), Some("actual-plan-step"));
    assert_eq!(child.sources("self_review"), parent.sources("self_review"));
    assert!(!child
        .sources("self_review")
        .iter()
        .any(|source| matches!(source, TechniqueSource::PlanStep { .. })));
}

#[test]
fn captured_self_review_each_child_keeps_its_own_resolved_profile_knobs() {
    let parent = CapturedTechniques::default();
    let first: ProfileConfig =
        toml::from_str("techniques = [\"self_review\"]\n[self_review]\nmax_rounds = 1").unwrap();
    let second: ProfileConfig =
        toml::from_str("techniques = [\"self_review\"]\n[self_review]\nmax_rounds = 5").unwrap();
    let pick = |name: &str| crate::config::ProfilePick {
        name: name.into(),
        via: crate::config::PickVia::Profile,
    };
    let a = parent
        .for_child(Some((&pick("navigator"), &first)), [], Some("actual-step"))
        .unwrap();
    let b = parent
        .for_child(Some((&pick("planner"), &second)), [], Some("actual-step"))
        .unwrap();
    assert_eq!(a.self_review().unwrap().max_rounds, 1);
    assert_eq!(b.self_review().unwrap().max_rounds, 5);
    assert_ne!(a.sources("self_review"), b.sources("self_review"));
    assert!(!parent.has_context());
}

#[test]
fn captured_self_review_canonical_context_alone_never_activates_review() {
    let parent = CapturedTechniques::default();
    let child = parent
        .for_child(None, [], Some("actual-plan-step"))
        .unwrap();
    assert!(child.has_context());
    assert!(child.is_empty());
    assert!(child.self_review().is_none());
    assert_eq!(child.plan_step(), Some("actual-plan-step"));
    assert!(!parent.has_context());
}
