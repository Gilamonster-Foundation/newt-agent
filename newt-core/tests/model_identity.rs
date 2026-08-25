//! **Model display names are labels, never evidence.**
//!
//! The architectural law this suite enforces (standing, Shawn):
//!
//! > No authoritative execution behavior may be selected through `contains`,
//! > `starts_with`, fuzzy matching, or parsing a display model ID. Structured
//! > metadata or an explicit declaration may determine identity; otherwise
//! > family is **Unknown** and family-specific policy does not apply.
//! > String-based recognition may only produce a visible **non-authoritative
//! > suggestion** requiring operator confirmation.
//!
//! Why it matters concretely: an operator may serve any artifact under any
//! alias. `ollama create my-helper -f ./Modelfile` over a Qwen3 GGUF produces
//! a model whose display id says nothing about it, and a fine-tune published
//! as `qwen-ish-experiment` may share no lineage with Qwen at all. A harness
//! that reads the label is wrong in **both** directions — it applies Qwen
//! stream-parsing to something that is not Qwen, and withholds it from
//! something that is.
//!
//! These are acceptance tests for the resolved-identity descriptor. Each
//! names the property it pins, in the vocabulary of the law.

use newt_core::model_identity::{FamilyEvidence, ResolvedModel};

// =========================================================================
// The two directions a label gets it wrong
// =========================================================================

/// **A label containing a family name grants nothing.** The alias is the
/// operator's to choose; it is not a declaration about lineage.
#[test]
fn an_alias_containing_qwen_does_not_activate_qwen_behaviour() {
    for alias in [
        "qwen-ish-experiment",
        "my-qwen3-finetune",
        "QWEN3-CODER-CLONE",
        "not-really-qwen3",
    ] {
        let model = ResolvedModel::unknown(alias);
        assert_eq!(
            model.family_policy_key(),
            None,
            "`{alias}` is a LABEL — it must not resolve a family"
        );
        assert_eq!(model.family_evidence(), FamilyEvidence::Unresolved);
        assert!(
            !model.family_evidence().is_authoritative(),
            "no structured evidence was supplied, so identity is Unknown"
        );
    }
}

/// **The converse, which name matching also gets wrong.** A genuine Qwen
/// artifact served under an unrelated alias resolves from its metadata.
#[test]
fn a_qwen_artifact_under_an_unrelated_alias_resolves_from_structured_evidence() {
    let model = ResolvedModel::declared(
        "ornith-1.0-35b",
        Some("qwen3"),
        FamilyEvidence::ProviderMetadata,
    )
    .with_metadata(Some("qwen3moe"), Some("sha256:9c1f3a"));
    assert_eq!(
        model.family_policy_key(),
        Some("qwen3"),
        "the provider declared the family; the alias is irrelevant"
    );
    assert_eq!(model.architecture(), Some("qwen3moe"));
    assert_eq!(model.artifact(), Some("sha256:9c1f3a"));
    assert_eq!(model.family_evidence(), FamilyEvidence::ProviderMetadata);
    assert!(model.family_evidence().is_authoritative());
}

// =========================================================================
// Precedence
// =========================================================================

/// **Explicit operator declaration outranks everything.** An operator who
/// writes a card has said what the thing IS; no discovered metadata overrides
/// them.
#[test]
fn an_operator_card_outranks_provider_metadata() {
    let discovered = ResolvedModel::declared("m", Some("qwen3"), FamilyEvidence::ProviderMetadata)
        .with_metadata(Some("qwen3moe"), Some("sha256:9c"));
    let declared = ResolvedModel::declared("m", Some("ornith"), FamilyEvidence::OperatorCard);
    let winner = ResolvedModel::resolve("m", [discovered, declared]);

    assert_eq!(winner.family_policy_key(), Some("ornith"));
    assert_eq!(winner.family_evidence(), FamilyEvidence::OperatorCard);

    // ...AND the loser's descriptive facts survive. The previous
    // wholesale-winner implementation silently dropped both, and this test
    // passed anyway because it only asserted the family — the review caught
    // what the test did not.
    assert_eq!(
        winner.architecture(),
        Some("qwen3moe"),
        "the operator declared the family; the provider still knows the architecture"
    );
    assert_eq!(winner.artifact(), Some("sha256:9c"));
}

/// The full ordering, pinned as a single fact so a future edit cannot
/// reshuffle it silently: operator card > resolved card > artifact metadata
/// > provider metadata > unresolved.
#[test]
fn evidence_precedence_is_total_and_ordered() {
    let order = [
        FamilyEvidence::OperatorCard,
        FamilyEvidence::ResolvedCard,
        FamilyEvidence::ArtifactMetadata,
        FamilyEvidence::ProviderMetadata,
        FamilyEvidence::Unresolved,
    ];
    for (i, stronger) in order.iter().enumerate() {
        for weaker in order.iter().skip(i + 1) {
            assert!(
                stronger.outranks(*weaker),
                "{stronger:?} must outrank {weaker:?}"
            );
            assert!(!weaker.outranks(*stronger));
        }
        assert!(!stronger.outranks(*stronger), "outranks is irreflexive");
    }
}

/// Choosing among candidates never invents evidence: with nothing but
/// unresolved inputs, the result is unresolved.
#[test]
fn choosing_among_unresolved_candidates_stays_unresolved() {
    let winner = ResolvedModel::resolve(
        "a",
        [ResolvedModel::unknown("a"), ResolvedModel::unknown("a")],
    );
    assert_eq!(winner.family_evidence(), FamilyEvidence::Unresolved);
    assert_eq!(winner.family_policy_key(), None);
    assert_eq!(
        winner.id(),
        "a",
        "the id is always known — never fabricated"
    );
}

/// Resolving with NO candidates yields the labelled Unknown, not an identity
/// with an empty id. The previous version returned `unknown("")` — a model
/// that does not exist — because the id was inferred from the winner instead
/// of being supplied.
#[test]
fn resolving_without_candidates_keeps_the_id() {
    let m = ResolvedModel::resolve("some-model", []);
    assert_eq!(m.id(), "some-model");
    assert_eq!(m.family_policy_key(), None);
    assert_eq!(m.family_evidence(), FamilyEvidence::Unresolved);
}

/// Evidence is only as good as what it carried: claiming provider metadata
/// while supplying no family resolves nothing. This makes the module's one
/// invariant — a family is present exactly when its evidence is
/// authoritative — hold by construction rather than by discipline.
#[test]
fn claimed_evidence_without_a_family_downgrades_to_unresolved() {
    for blank in [None, Some(""), Some("   ")] {
        let m = ResolvedModel::declared("m", blank, FamilyEvidence::ProviderMetadata);
        assert_eq!(m.family_policy_key(), None);
        assert_eq!(
            m.family_evidence(),
            FamilyEvidence::Unresolved,
            "a claim with nothing behind it is not evidence"
        );
    }
}

// =========================================================================
// The Unknown floor
// =========================================================================

/// **A generic OpenAI-compatible endpoint exposes only an id, so family is
/// Unknown.** This is the common case for llama.cpp and vLLM behind a plain
/// `/v1/models`, and it must not be papered over.
#[test]
fn a_generic_openai_id_remains_unknown() {
    // A generic `/v1/models` entry exposes an id and nothing else.
    let model = ResolvedModel::unknown("gpt-4o-mini");
    assert_eq!(model.family_policy_key(), None);
    assert_eq!(model.family_evidence(), FamilyEvidence::Unresolved);
    assert_eq!(model.id(), "gpt-4o-mini", "the id is kept — as a label");
}

/// **Unknown applies no family policy.** The point of the Unknown floor is
/// that it is inert, not that it is a default family.
#[test]
fn unknown_family_applies_no_family_policy() {
    let model = ResolvedModel::unknown("anything at all");
    assert_eq!(model.family_policy_key(), None);
    assert!(!model.is_family("qwen3"));
    assert!(!model.is_family(""));
}

// =========================================================================
// String recognition is a SUGGESTION, never authority
// =========================================================================

/// A name-derived hint is available, clearly marked, and **cannot** be
/// mistaken for resolved identity: it produces a suggestion value, not a
/// `ResolvedModel`, so there is no path from a substring to authority.
#[test]
fn name_recognition_yields_a_suggestion_that_is_not_identity() {
    let hint = newt_core::model_identity::suggest_family_from_name(
        "my-qwen3-finetune",
        &["qwen3".to_string(), "gemma".to_string()],
    )
    .expect("a hint is available for an operator to confirm");
    assert_eq!(hint.family, "qwen3");
    assert_eq!(hint.matched_in, "my-qwen3-finetune");
    assert!(
        !hint.is_authoritative(),
        "a suggestion is never authoritative — it requires operator confirmation"
    );

    // And the suggestion does not leak into identity.
    let model = ResolvedModel::unknown("my-qwen3-finetune");
    assert_eq!(model.family_policy_key(), None);
}

/// Confirming a suggestion is an explicit operator act, and what it produces
/// is operator-card evidence — the confirmation, not the substring, is what
/// carries the authority.
#[test]
fn a_confirmed_suggestion_becomes_operator_evidence() {
    let hint = newt_core::model_identity::suggest_family_from_name(
        "my-qwen3-finetune",
        &["qwen3".to_string()],
    )
    .expect("hint");
    let model = hint.confirm("my-qwen3-finetune");
    assert_eq!(model.family_policy_key(), Some("qwen3"));
    assert_eq!(
        model.family_evidence(),
        FamilyEvidence::OperatorCard,
        "the operator's confirmation is the evidence, not the name"
    );
}

/// No hint at all when nothing matches — absence is reported as absence.
#[test]
fn name_recognition_reports_no_hint_rather_than_guessing() {
    assert!(newt_core::model_identity::suggest_family_from_name(
        "ornith-1.0-35b",
        &["qwen3".to_string(), "gemma".to_string()],
    )
    .is_none());
}

// =========================================================================
// The first migrated selector: leading-reasoning stream filtering (#384/#528)
// =========================================================================
//
// This is the behavior the law protects, end to end. The flag decides whether
// streamed text is suppressed as chain-of-thought or printed into the reply,
// so getting it from a label is wrong in both directions AND silent.

use newt_core::model_card::{Capability, ModelCard};

fn card_with(emits: Option<bool>) -> ModelCard {
    let mut card = ModelCard {
        name: "irrelevant-label".to_string(),
        backend: None,
        footprint_gib: None,
        gated: None,
        family: None,
        vllm: None,
        ollama: None,
        tuning: None,
        capability: None,
    };
    if emits.is_some() {
        card.capability = Some(Capability {
            emits_leading_reasoning: emits,
            thinking_default: None,
            reasoning_content_field: None,
            reasoning_replay_scope: None,
            chat_completions: None,
        });
    }
    card
}

/// **An alias containing a reasoning-family token does not enable filtering.**
/// Before this change `model.contains("qwen3")` made this true, and any
/// unrelated model so named had its answer text silently eaten.
#[test]
fn an_alias_alone_never_enables_leading_reasoning_filtering() {
    for alias in ["my-qwen3-finetune", "nemotron-ish", "deepseek-r1-clone"] {
        let card = card_with(None);
        assert_eq!(
            card.capability
                .as_ref()
                .and_then(|c| c.emits_leading_reasoning),
            None,
            "`{alias}`: no capability was declared, so nothing is known"
        );
    }
}

/// **A declared capability decides, whatever the model is called.** The
/// converse failure: a genuine reasoning model served under an unrelated
/// alias previously printed its raw chain-of-thought into the answer.
#[test]
fn a_declared_capability_enables_filtering_under_any_alias() {
    let card = card_with(Some(true));
    assert_eq!(
        card.capability
            .as_ref()
            .and_then(|c| c.emits_leading_reasoning),
        Some(true),
        "the declaration is the evidence; `{}` is just a label",
        card.name
    );
}

/// **Unknown means off, and that direction is chosen deliberately.** The two
/// failure modes are not symmetric: filtering when we should not DROPS real
/// answer text with no trace, while not filtering when we should shows
/// reasoning the operator can see and correct. Fail toward the visible one.
#[test]
fn unknown_capability_leaves_filtering_off() {
    let card = card_with(None);
    let resolved = card
        .capability
        .as_ref()
        .and_then(|c| c.emits_leading_reasoning)
        .unwrap_or(false);
    assert!(
        !resolved,
        "no declaration ⇒ no family policy ⇒ the filter stays off"
    );
}

/// An explicit `false` is a real declaration, distinct from absence — an
/// operator may state that a model does NOT do this, and that must not be
/// confused with never having been asked.
#[test]
fn an_explicit_false_is_a_declaration_not_an_absence() {
    let declared = card_with(Some(false));
    let absent = card_with(None);
    assert_eq!(
        declared
            .capability
            .as_ref()
            .and_then(|c| c.emits_leading_reasoning),
        Some(false)
    );
    assert_eq!(
        absent
            .capability
            .as_ref()
            .and_then(|c| c.emits_leading_reasoning),
        None
    );
}
