//! **The capability sidecar, exercised through its real API.**
//!
//! `ResolvedCapabilities::resolve` + `for_route` are the ONE owner of
//! card/inline capability semantics — the TUI's backend choice and headless
//! `solve` both consume exactly these. Tests here drive the public seam with
//! a real on-disk card catalog (a temp config's sibling `models/`, the
//! operator-explicit arm of the catalog rule) — no env vars, no hand-set
//! derived state. Every fixture is a VALID card (backend + matching serving
//! block): the catalog validates the fully resolved card. Core keeps every
//! outcome TYPED ([`CardApplicability`]); prose is a display-seam concern
//! and none is asserted here.

use newt_core::model_card::{
    Capability, CardApplicability, CardBindingSeed, ReasoningReplayScope, ResolvedCapabilities,
    ServingPrincipal,
};
use newt_core::BackendDestination;
use std::path::PathBuf;

/// The destination every [`backend`] fixture declares — the route the
/// same-destination tests decide at.
fn home() -> BackendDestination {
    BackendDestination::new(Some("http://127.0.0.1:11434".to_string()), None)
}

/// A temp "profile": a config file path whose sibling `models/` holds cards.
fn catalog_with(cards: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("profile.toml");
    std::fs::write(&config, "# profile\n").unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir_all(&models).unwrap();
    for (name, body) in cards {
        std::fs::write(models.join(format!("{name}.toml")), body).unwrap();
    }
    (dir, config)
}

fn backend(
    model: Option<&str>,
    card: Option<&str>,
    inline: Option<Capability>,
) -> newt_core::BackendConfig {
    newt_core::BackendConfig {
        name: "test".to_string(),
        endpoint: "http://127.0.0.1:11434".to_string(),
        model: model.map(str::to_string),
        card: card.map(str::to_string),
        capability: inline,
        ..Default::default()
    }
}

/// Resolve with the seed the backend's own (un-overlaid) declaration yields.
fn resolve(
    b: &newt_core::BackendConfig,
    config: Option<&std::path::Path>,
) -> Result<ResolvedCapabilities, String> {
    ResolvedCapabilities::resolve(b, &CardBindingSeed::from_backend(b), config)
}

const REASONER_CARD: &str = r#"
name = "team-reasoner"
backend = "vllm"

[vllm]
served_name = "team-reasoner"

[capability]
emits_leading_reasoning = true
reasoning_replay_scope = "current_user_turn"

[capability.chat_completions]
cognition = true
"#;

// =========================================================================
// Resolution
// =========================================================================

/// The named card's declarations flow, and the inline block overrides
/// FIELD-BY-FIELD — an explicit inline `false` beats the card's `true`,
/// while fields inline leaves unset still come from the card.
#[test]
fn card_supplies_and_inline_overrides_field_by_field() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let b = backend(
        Some("bound-model"),
        Some("team-reasoner"),
        Some(Capability {
            emits_leading_reasoning: Some(false), // operator says no…
            ..Default::default()
        }),
    );
    let caps = resolve(&b, Some(&config)).unwrap();
    let d = caps.for_route(&home(), ServingPrincipal::Instance);
    assert!(!d.emits_leading_reasoning(), "explicit inline false wins");
    assert_eq!(
        d.reasoning_replay_scope(),
        ReasoningReplayScope::CurrentUserTurn,
        "…while the card still supplies what inline left unset"
    );
    assert_eq!(d.chat_completions().cognition, Some(true));
}

/// A NESTED explicit inline `Some(false)` beats the card's nested `true` —
/// the deep merge honors the operator's no at every level.
#[test]
fn nested_inline_false_overrides_a_cards_nested_true() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let b = backend(
        Some("bound-model"),
        Some("team-reasoner"),
        Some(Capability {
            chat_completions: Some(newt_core::model_card::ChatCompletionsCapability {
                cognition: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }),
    );
    let caps = resolve(&b, Some(&config)).unwrap();
    let d = caps.for_route(&home(), ServingPrincipal::Instance);
    assert_eq!(
        d.chat_completions().cognition,
        Some(false),
        "the nested inline false wins over the card's nested true"
    );
    assert!(
        d.emits_leading_reasoning(),
        "sibling card fields still flow"
    );
}

/// Every capability field flows from the card through the decision — the
/// FULL `Capability` seam, not just the flag accessors.
#[test]
fn every_capability_field_flows_from_the_card() {
    let full = r#"
name = "full-card"
backend = "vllm"

[vllm]
served_name = "full-card"

[capability]
emits_leading_reasoning = true
thinking_default = true
reasoning_content_field = "reasoning_content"
reasoning_replay_scope = "full_history"

[capability.chat_completions]
cognition = true
chat_template_kwargs = true
parallel_tool_calls = false
bounded_reasoning_continuation = true
"#;
    let (_dir, config) = catalog_with(&[("full-card", full)]);
    let b = backend(Some("m"), Some("full-card"), None);
    let caps = resolve(&b, Some(&config)).unwrap();
    let d = caps.for_route(&home(), ServingPrincipal::Instance);
    let effective = d.effective().expect("card layer applies");
    assert_eq!(effective.emits_leading_reasoning, Some(true));
    assert_eq!(effective.thinking_default, Some(true));
    assert_eq!(
        effective.reasoning_content_field.as_deref(),
        Some("reasoning_content")
    );
    assert_eq!(
        d.reasoning_replay_scope(),
        ReasoningReplayScope::FullHistory
    );
    let chat = d.chat_completions();
    assert_eq!(chat.cognition, Some(true));
    assert_eq!(chat.chat_template_kwargs, Some(true));
    assert_eq!(chat.parallel_tool_calls, Some(false));
    assert_eq!(chat.bounded_reasoning_continuation, Some(true));
}

/// An unknown named card is a hard error naming the backend, the card, and
/// the searched dir — never a silent no-card.
#[test]
fn an_unknown_named_card_is_a_hard_error() {
    let (_dir, config) = catalog_with(&[]);
    let b = backend(Some("m"), Some("no-such-card"), None);
    let err = resolve(&b, Some(&config)).unwrap_err();
    assert!(err.contains("test"), "names the backend: {err}");
    assert!(err.contains("no-such-card"), "names the card: {err}");
    assert!(err.contains("models"), "names the searched dir: {err}");
}

/// A card that EXISTS but does not parse is distinguished from a typo — the
/// operator gets sent to the file, not to a name that is already right.
#[test]
fn a_malformed_referenced_card_says_so() {
    let (_dir, config) = catalog_with(&[("broken", "not [ valid toml")]);
    let b = backend(Some("m"), Some("broken"), None);
    let err = resolve(&b, Some(&config)).unwrap_err();
    assert!(
        err.contains("EXISTS") && err.contains("did not parse"),
        "the diagnosis must point at the file, not the name: {err}"
    );
}

/// A referenced card that parses but is INVALID (here: no backend at all)
/// is a hard error saying so — the catalog validates the resolved card.
#[test]
fn an_invalid_referenced_card_is_a_hard_error() {
    let (_dir, config) = catalog_with(&[(
        "caps-only",
        "name = \"caps-only\"\n\n[capability]\nemits_leading_reasoning = true\n",
    )]);
    let b = backend(Some("m"), Some("caps-only"), None);
    let err = resolve(&b, Some(&config)).unwrap_err();
    assert!(err.contains("invalid"), "says invalid, not absent: {err}");
}

/// A known VALID card WITHOUT a `[capability]` block is fine (serving/
/// tuning-only) and contributes NO layer: no binding is reported, so nothing
/// downstream offers to disable declarations that never applied.
#[test]
fn a_capability_less_card_is_valid_and_contributes_nothing() {
    let (_dir, config) = catalog_with(&[(
        "serving-only",
        "name = \"serving-only\"\nbackend = \"vllm\"\n\n[vllm]\nreasoning_parser = \"qwen3\"\n",
    )]);
    let inline = Capability {
        emits_leading_reasoning: Some(true),
        ..Default::default()
    };
    let b = backend(Some("m"), Some("serving-only"), Some(inline));
    let caps = resolve(&b, Some(&config)).unwrap();
    assert_eq!(caps.card(), None, "no capability ⇒ no binding to report");
    let d = caps.for_route(&home(), ServingPrincipal::MultiplexerModel("other-model"));
    assert_eq!(
        d.applicability(),
        &CardApplicability::None,
        "no binding ⇒ nothing to report inactive, whatever the principal"
    );
    assert!(
        d.emits_leading_reasoning(),
        "inline remains the whole story"
    );
}

/// No card and no inline: the conservative floor, for every principal.
#[test]
fn absence_stays_conservative() {
    let b = backend(Some("m"), None, None);
    let caps = resolve(&b, None).unwrap();
    for p in [
        ServingPrincipal::Instance,
        ServingPrincipal::MultiplexerModel("m"),
        ServingPrincipal::SelectedModel("m"),
        ServingPrincipal::Unknown,
    ] {
        let d = caps.for_route(&home(), p);
        assert!(!d.emits_leading_reasoning());
        assert_eq!(d.reasoning_replay_scope(), ReasoningReplayScope::Never);
        assert_eq!(d.applicability(), &CardApplicability::None);
    }
}

/// The seed is the binding evidence — a backend whose `model` field was
/// OVERLAID (CLI/session override) does not rebind the card: the seed's
/// pre-overlay declaration decides association.
#[test]
fn the_seed_outlives_a_backend_model_overlay() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    // The backend as OVERRIDDEN (model=B), but the seed carries the
    // pre-overlay declaration (bound A).
    let b = backend(Some("override-b"), Some("team-reasoner"), None);
    let seed = CardBindingSeed {
        card: Some("team-reasoner".into()),
        bound_model: Some("declared-a".into()),
        bound_destination: home(),
    };
    let caps = ResolvedCapabilities::resolve(&b, &seed, Some(&config)).unwrap();
    // The exact declared model keeps the binding; the overlay value does not.
    assert!(caps
        .for_route(&home(), ServingPrincipal::MultiplexerModel("declared-a"))
        .emits_leading_reasoning());
    let d = caps.for_route(&home(), ServingPrincipal::MultiplexerModel("override-b"));
    assert!(!d.emits_leading_reasoning());
    assert!(
        matches!(d.applicability(), CardApplicability::InactiveModel { .. }),
        "the overlay is a visible retarget, never a silent rebind"
    );
}

// =========================================================================
// The principal decision
// =========================================================================

fn bound_caps() -> (tempfile::TempDir, ResolvedCapabilities) {
    let (dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let b = backend(Some("bound-model"), Some("team-reasoner"), None);
    let caps = resolve(&b, Some(&config)).unwrap();
    (dir, caps)
}

/// **Instance: the binding holds whatever the served label says.** One
/// artifact is served; the operator's binding names it; the display id is an
/// alias (`requested_ignored` included).
#[test]
fn instance_preserves_the_binding_under_any_alias() {
    let (_dir, caps) = bound_caps();
    let d = caps.for_route(&home(), ServingPrincipal::Instance);
    assert!(d.emits_leading_reasoning());
    assert!(matches!(
        d.applicability(),
        CardApplicability::Active { card } if card == "team-reasoner"
    ));
}

/// **Multiplexer, exact bound model: the binding holds.** Exact string
/// equality of two supplied identifiers inside the typed arm — association,
/// never inference.
#[test]
fn multiplexer_with_the_exact_bound_model_keeps_the_binding() {
    let (_dir, caps) = bound_caps();
    let d = caps.for_route(&home(), ServingPrincipal::MultiplexerModel("bound-model"));
    assert!(d.emits_leading_reasoning());
    assert!(matches!(
        d.applicability(),
        CardApplicability::Active { .. }
    ));
}

/// Near-collisions never associate: prefix, suffix, case change, trailing
/// whitespace — every one is a different identifier, typed Inactive.
#[test]
fn near_collisions_never_associate() {
    let (_dir, caps) = bound_caps();
    for near in [
        "bound-model2", // suffix
        "bound-mode",   // prefix
        "Bound-Model",  // case
        "bound-model ", // trailing space
        " bound-model", // leading space
    ] {
        let d = caps.for_route(&home(), ServingPrincipal::MultiplexerModel(near));
        assert!(
            !d.emits_leading_reasoning(),
            "`{near}` must not associate with `bound-model`"
        );
        assert!(
            matches!(d.applicability(), CardApplicability::InactiveModel { .. }),
            "`{near}` is a visible retarget"
        );
    }
}

/// **Multiplexer, different final model: inline-only plus a typed, visible
/// Inactive.** A warm pick, a fallback, or an explicit override landed the
/// session on a principal the card was never bound against — behavior must
/// not carry, and the operator must SEE that it did not.
#[test]
fn multiplexer_retarget_drops_the_card_layer_visibly() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let inline = Capability {
        reasoning_replay_scope: Some(ReasoningReplayScope::CurrentUserTurn),
        ..Default::default()
    };
    let b = backend(Some("bound-model"), Some("team-reasoner"), Some(inline));
    let caps = resolve(&b, Some(&config)).unwrap();
    let d = caps.for_route(&home(), ServingPrincipal::MultiplexerModel("warm-pick"));
    assert!(
        !d.emits_leading_reasoning(),
        "card-derived fields are off for a model the card was not bound to"
    );
    assert_eq!(
        d.reasoning_replay_scope(),
        ReasoningReplayScope::CurrentUserTurn,
        "inline backend-scoped fields SURVIVE the retarget"
    );
    let CardApplicability::InactiveModel {
        card,
        bound_model,
        active_model,
    } = d.applicability()
    else {
        panic!(
            "the retarget must be typed Inactive, got {:?}",
            d.applicability()
        );
    };
    assert_eq!(card, "team-reasoner");
    assert_eq!(bound_model.as_deref(), Some("bound-model"));
    assert_eq!(active_model, "warm-pick");
}

/// **SelectedModel associates exactly like the multiplexer arm**: the
/// serving axis is unknown but the model identity was operator-SELECTED, so
/// exact association is justified — and only exact.
#[test]
fn selected_model_associates_exactly_like_a_multiplexer() {
    let (_dir, caps) = bound_caps();
    assert!(caps
        .for_route(&home(), ServingPrincipal::SelectedModel("bound-model"))
        .emits_leading_reasoning());
    let d = caps.for_route(&home(), ServingPrincipal::SelectedModel("other"));
    assert!(!d.emits_leading_reasoning());
    assert!(matches!(
        d.applicability(),
        CardApplicability::InactiveModel { .. }
    ));
}

/// **A card bound to no declared model never applies on a multiplexer** —
/// there is nothing to associate the pick with — and the status says so.
#[test]
fn an_unbound_card_on_a_multiplexer_is_inactive_with_a_notice() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let b = backend(None, Some("team-reasoner"), None);
    let caps = resolve(&b, Some(&config)).unwrap();
    let d = caps.for_route(&home(), ServingPrincipal::MultiplexerModel("whatever"));
    assert!(!d.emits_leading_reasoning());
    assert!(matches!(
        d.applicability(),
        CardApplicability::InactiveModel {
            bound_model: None,
            ..
        }
    ));
    // …but the same unbound card on an INSTANCE applies: one artifact.
    let d = caps.for_route(&home(), ServingPrincipal::Instance);
    assert!(d.emits_leading_reasoning());
}

/// **Unknown serving is typed Undecided, never silent**: inline-only, and
/// the binding's pending state is visible to consumers — headless refuses to
/// run on it, the TUI renders the transition. (This replaced the earlier
/// "defers without a notice" contract: a configured card silently never
/// applying was exactly the failure mode review rejected.)
#[test]
fn unknown_serving_is_typed_undecided_not_silent() {
    let (_dir, caps) = bound_caps();
    let d = caps.for_route(&home(), ServingPrincipal::Unknown);
    assert!(!d.emits_leading_reasoning(), "inline-only while undecided");
    assert!(matches!(
        d.applicability(),
        CardApplicability::Undecided { card } if card == "team-reasoner"
    ));
}

/// **The decision is pure and stateless**: deciding for a retargeted
/// principal and then for the bound one again re-enables nothing and loses
/// nothing — a rebuilt choice cannot re-enable a suppressed card by
/// construction, because nothing was ever mutated.
#[test]
fn the_decision_is_stateless_across_switches() {
    let (_dir, caps) = bound_caps();
    let away = caps.for_route(&home(), ServingPrincipal::MultiplexerModel("elsewhere"));
    assert!(!away.emits_leading_reasoning());
    let back = caps.for_route(&home(), ServingPrincipal::MultiplexerModel("bound-model"));
    assert!(
        back.emits_leading_reasoning(),
        "switching BACK to the bound model restores the card layer — the \
         layers were never mutated"
    );
}

// =========================================================================
// The destination gate
// =========================================================================

/// **A route pointed at a DIFFERENT destination is typed
/// InactiveDestination** — the binding is evidence about the server it was
/// bound at; evidence stays intact and visible, inline fields survive.
#[test]
fn a_moved_destination_is_typed_inactive_destination() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let inline = Capability {
        reasoning_replay_scope: Some(ReasoningReplayScope::CurrentUserTurn),
        ..Default::default()
    };
    let b = backend(Some("bound-model"), Some("team-reasoner"), Some(inline));
    let caps = resolve(&b, Some(&config)).unwrap();
    let elsewhere = BackendDestination::new(Some("http://10.0.0.9:8000".to_string()), None);
    // Even the EXACT bound model does not carry the card across destinations.
    let d = caps.for_route(
        &elsewhere,
        ServingPrincipal::MultiplexerModel("bound-model"),
    );
    assert!(
        !d.emits_leading_reasoning(),
        "card layer off across destinations"
    );
    assert_eq!(
        d.reasoning_replay_scope(),
        ReasoningReplayScope::CurrentUserTurn,
        "inline backend-scoped fields SURVIVE the retarget"
    );
    let CardApplicability::InactiveDestination {
        card,
        bound_destination,
        active_destination,
    } = d.applicability()
    else {
        panic!("expected InactiveDestination, got {:?}", d.applicability());
    };
    assert_eq!(card, "team-reasoner");
    assert_eq!(bound_destination, &home());
    assert_eq!(active_destination, &elsewhere);
}

/// Destination near-collisions never associate: trailing slash, trailing
/// space, scheme case, truncation — comparison is exact string equality
/// (empty-to-None is the ONLY normalization), so each is a different
/// destination, typed InactiveDestination even for an Instance principal.
#[test]
fn destination_near_collisions_never_associate() {
    let (_dir, caps) = bound_caps();
    for near in [
        "http://127.0.0.1:11434/", // trailing slash
        "http://127.0.0.1:11434 ", // trailing space
        "HTTP://127.0.0.1:11434",  // scheme case
        "http://127.0.0.1:1143",   // truncation
    ] {
        let there = BackendDestination::new(Some(near.to_string()), None);
        let d = caps.for_route(&there, ServingPrincipal::Instance);
        assert!(
            !d.emits_leading_reasoning(),
            "`{near}` must not associate with the bound destination"
        );
        assert!(
            matches!(
                d.applicability(),
                CardApplicability::InactiveDestination { .. }
            ),
            "`{near}` is a different destination"
        );
    }
}

/// An embedded (model_path) destination associates exactly, too: same
/// artifact path applies; a different artifact path is InactiveDestination.
#[test]
fn an_embedded_model_path_destination_associates_exactly() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let b = newt_core::BackendConfig {
        name: "embedded".to_string(),
        model: Some("bound-model".to_string()),
        card: Some("team-reasoner".to_string()),
        model_path: Some("/models/a.gguf".to_string()),
        ..Default::default()
    };
    let caps = resolve(&b, Some(&config)).unwrap();
    let bound_at = BackendDestination::new(None, Some("/models/a.gguf".to_string()));
    assert!(caps
        .for_route(&bound_at, ServingPrincipal::Instance)
        .emits_leading_reasoning());
    let other = BackendDestination::new(None, Some("/models/b.gguf".to_string()));
    let d = caps.for_route(&other, ServingPrincipal::Instance);
    assert!(!d.emits_leading_reasoning());
    assert!(matches!(
        d.applicability(),
        CardApplicability::InactiveDestination { .. }
    ));
}

// =========================================================================
// Hollow identities never activate (L/M)
// =========================================================================

/// L: an empty or whitespace model string is NO identity — a hand-built
/// seed with `bound_model = Some("")` must not activate against an
/// empty-principal "match", and an empty principal is typed Undecided,
/// never an InactiveModel report against `""`.
#[test]
fn an_empty_model_identity_never_activates_a_card() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let b = backend(Some(""), Some("team-reasoner"), None);
    let seed = CardBindingSeed {
        card: Some("team-reasoner".into()),
        bound_model: Some("".into()),
        bound_destination: home(),
    };
    let caps = ResolvedCapabilities::resolve(&b, &seed, Some(&config)).unwrap();
    for hollow in ["", "   "] {
        let d = caps.for_route(&home(), ServingPrincipal::MultiplexerModel(hollow));
        assert!(
            !d.emits_leading_reasoning(),
            "`{hollow:?}` must not associate — two absences agreeing is not identity"
        );
        assert!(
            matches!(d.applicability(), CardApplicability::Undecided { .. }),
            "`{hollow:?}` is no identity to report a mismatch against: {:?}",
            d.applicability()
        );
        let d = caps.for_route(&home(), ServingPrincipal::SelectedModel(hollow));
        assert!(!d.emits_leading_reasoning());
    }
    // A declaration-built seed normalizes the empty model away entirely.
    let normalized = CardBindingSeed::from_backend(&b);
    assert_eq!(normalized.bound_model, None, "effective-model rule applies");
}

/// M: a card binding without a CONCRETE destination (endpoint XOR
/// model_path) never activates — a hollow seed matching a hollow route is
/// not an exact association, even for Instance. Inline-only, typed
/// Undecided.
#[test]
fn a_hollow_destination_binding_never_activates_a_card() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    // A destination-less backend declaring a card: the seed's bound
    // destination is hollow.
    let b = newt_core::BackendConfig {
        name: "hollow".to_string(),
        model: Some("bound-model".to_string()),
        card: Some("team-reasoner".to_string()),
        ..Default::default()
    };
    let caps = resolve(&b, Some(&config)).unwrap();
    let nowhere = BackendDestination::default();
    // Hollow bound + hollow active: NOT activation — not even for Instance.
    let d = caps.for_route(&nowhere, ServingPrincipal::Instance);
    assert!(
        !d.emits_leading_reasoning(),
        "no concrete destination, no card"
    );
    assert!(matches!(
        d.applicability(),
        CardApplicability::Undecided { .. }
    ));
    // Hollow bound + a concrete active route: still undecided, inline-only.
    let d = caps.for_route(&home(), ServingPrincipal::Instance);
    assert!(!d.emits_leading_reasoning());
    assert!(matches!(
        d.applicability(),
        CardApplicability::Undecided { .. }
    ));
    // And a composite active destination is not concrete either.
    let both = BackendDestination {
        endpoint: Some("http://h:1".to_string()),
        model_path: Some("/m.gguf".to_string()),
    };
    let (_dir2, caps) = bound_caps();
    let d = caps.for_route(&both, ServingPrincipal::Instance);
    assert!(!d.emits_leading_reasoning());
    assert!(matches!(
        d.applicability(),
        CardApplicability::Undecided { .. }
    ));
}

/// N: the public fields admit hand-built literals holding `Some("")` that
/// `new()` would have normalized away — concreteness checks CONTENT, so
/// two empty-string destinations agreeing (plus Instance) still activate
/// NOTHING.
#[test]
fn empty_string_destination_literals_are_not_concrete() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let empty_endpoint = BackendDestination {
        endpoint: Some(String::new()),
        model_path: None,
    };
    let empty_path = BackendDestination {
        endpoint: None,
        model_path: Some(String::new()),
    };
    let b = backend(Some("bound-model"), Some("team-reasoner"), None);
    for bound in [empty_endpoint.clone(), empty_path.clone()] {
        let seed = CardBindingSeed {
            card: Some("team-reasoner".into()),
            bound_model: Some("bound-model".into()),
            bound_destination: bound.clone(),
        };
        let caps = ResolvedCapabilities::resolve(&b, &seed, Some(&config)).unwrap();
        // The "matching" hollow route: two absences agreeing, not identity.
        let d = caps.for_route(&bound, ServingPrincipal::Instance);
        assert!(
            !d.emits_leading_reasoning(),
            "{bound:?}: empty-string destinations must not activate"
        );
        assert!(matches!(
            d.applicability(),
            CardApplicability::Undecided { .. }
        ));
    }
    // One nonempty axis beside an EMPTY one is still concrete (equivalent
    // to the normalized form) — the guard rejects absences, not decor.
    let normalized_equivalent = BackendDestination {
        endpoint: Some("http://127.0.0.1:11434".to_string()),
        model_path: Some(String::new()),
    };
    let caps = resolve(&b, Some(&config)).unwrap();
    let d = caps.for_route(&normalized_equivalent, ServingPrincipal::Instance);
    assert!(
        matches!(
            d.applicability(),
            CardApplicability::InactiveDestination { .. }
        ),
        "concrete but UNEQUAL (Some(\"\") != None field-wise) stays a typed \
         destination mismatch, never an activation and never Undecided: {:?}",
        d.applicability()
    );
}

/// P: the card pointer's EXACT identity reaches the catalog — edge
/// whitespace and case near-collisions are typed hard errors, never a
/// silent bind to the trimmed/folded exact card.
#[test]
fn card_pointer_near_collisions_never_silently_bind() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    for near in ["team-reasoner ", " team-reasoner", "Team-Reasoner"] {
        let b = backend(Some("bound-model"), Some(near), None);
        let err = resolve(&b, Some(&config))
            .expect_err("a near-collision pointer must not bind the exact card");
        assert!(
            err.contains("test"),
            "`{near}`: the error names the backend: {err}"
        );
    }
    // The exact pointer still binds.
    let b = backend(Some("bound-model"), Some("team-reasoner"), None);
    let caps = resolve(&b, Some(&config)).unwrap();
    assert!(caps
        .for_route(&home(), ServingPrincipal::Instance)
        .emits_leading_reasoning());
}
