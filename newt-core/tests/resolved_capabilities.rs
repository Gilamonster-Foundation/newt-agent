//! **The capability sidecar, exercised through its real API.**
//!
//! `ResolvedCapabilities::resolve` + `for_principal` are the ONE owner of
//! card/inline capability semantics — the TUI's backend choice and headless
//! `solve` both consume exactly these. Tests here drive the public seam with
//! a real on-disk card catalog (a temp config's sibling `models/`, the
//! operator-explicit arm of the catalog rule) — no env vars, no hand-set
//! derived state.

use newt_core::model_card::{
    Capability, ReasoningReplayScope, ResolvedCapabilities, ServingPrincipal,
};
use std::path::PathBuf;

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
    let mut b = newt_core::BackendConfig {
        name: "test".to_string(),
        endpoint: "http://127.0.0.1:11434".to_string(),
        ..Default::default()
    };
    b.model = model.map(str::to_string);
    b.card = card.map(str::to_string);
    b.capability = inline;
    b
}

const REASONER_CARD: &str = r#"
name = "team-reasoner"

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
    let caps = ResolvedCapabilities::resolve(&b, Some(&config)).unwrap();
    let d = caps.for_principal(ServingPrincipal::Instance);
    assert!(!d.emits_leading_reasoning(), "explicit inline false wins");
    assert_eq!(
        d.reasoning_replay_scope(),
        ReasoningReplayScope::CurrentUserTurn,
        "…while the card still supplies what inline left unset"
    );
    assert_eq!(d.chat_completions().cognition, Some(true));
}

/// An unknown named card is a hard error naming the backend, the card, and
/// the searched dir — never a silent no-card.
#[test]
fn an_unknown_named_card_is_a_hard_error() {
    let (_dir, config) = catalog_with(&[]);
    let b = backend(Some("m"), Some("no-such-card"), None);
    let err = ResolvedCapabilities::resolve(&b, Some(&config)).unwrap_err();
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
    let err = ResolvedCapabilities::resolve(&b, Some(&config)).unwrap_err();
    assert!(
        err.contains("EXISTS") && err.contains("did not parse"),
        "the diagnosis must point at the file, not the name: {err}"
    );
}

/// A known card WITHOUT a `[capability]` block is valid (serving/tuning-only)
/// and contributes NO layer: no binding is reported, so nothing downstream
/// offers to disable declarations that never applied.
#[test]
fn a_capability_less_card_is_valid_and_contributes_nothing() {
    let (_dir, config) = catalog_with(&[(
        "serving-only",
        "name = \"serving-only\"\n\n[vllm]\nreasoning_parser = \"qwen3\"\n",
    )]);
    let inline = Capability {
        emits_leading_reasoning: Some(true),
        ..Default::default()
    };
    let b = backend(Some("m"), Some("serving-only"), Some(inline));
    let caps = ResolvedCapabilities::resolve(&b, Some(&config)).unwrap();
    assert_eq!(caps.card(), None, "no capability ⇒ no binding to report");
    let d = caps.for_principal(ServingPrincipal::MultiplexerModel("other-model"));
    assert!(
        d.retarget_notice.is_none(),
        "no binding ⇒ no retarget notice, whatever the principal"
    );
    assert!(
        d.emits_leading_reasoning(),
        "inline remains the whole story"
    );
}

/// No card and no inline: the conservative floor.
#[test]
fn absence_stays_conservative() {
    let b = backend(Some("m"), None, None);
    let caps = ResolvedCapabilities::resolve(&b, None).unwrap();
    for p in [
        ServingPrincipal::Instance,
        ServingPrincipal::MultiplexerModel("m"),
        ServingPrincipal::Unknown,
    ] {
        let d = caps.for_principal(p);
        assert!(!d.emits_leading_reasoning());
        assert_eq!(d.reasoning_replay_scope(), ReasoningReplayScope::Never);
        assert!(d.retarget_notice.is_none());
    }
}

// =========================================================================
// The principal decision
// =========================================================================

fn bound_caps() -> (tempfile::TempDir, ResolvedCapabilities) {
    let (dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let b = backend(Some("bound-model"), Some("team-reasoner"), None);
    let caps = ResolvedCapabilities::resolve(&b, Some(&config)).unwrap();
    (dir, caps)
}

/// **Instance: the binding holds whatever the served label says.** One
/// artifact is served; the operator's binding names it; the display id is an
/// alias (`requested_ignored` included).
#[test]
fn instance_preserves_the_binding_under_any_alias() {
    let (_dir, caps) = bound_caps();
    let d = caps.for_principal(ServingPrincipal::Instance);
    assert!(d.emits_leading_reasoning());
    assert!(d.retarget_notice.is_none());
}

/// **Multiplexer, exact bound model: the binding holds.** Exact string
/// equality of two supplied identifiers inside the typed arm — association,
/// never inference.
#[test]
fn multiplexer_with_the_exact_bound_model_keeps_the_binding() {
    let (_dir, caps) = bound_caps();
    let d = caps.for_principal(ServingPrincipal::MultiplexerModel("bound-model"));
    assert!(d.emits_leading_reasoning());
    assert!(d.retarget_notice.is_none());
}

/// **Multiplexer, different final model: inline-only plus a visible notice.**
/// A warm pick, a fallback, or an explicit override landed the session on a
/// principal the card was never bound against — behavior must not carry, and
/// the operator must SEE that it did not.
#[test]
fn multiplexer_retarget_drops_the_card_layer_visibly() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let inline = Capability {
        reasoning_replay_scope: Some(ReasoningReplayScope::CurrentUserTurn),
        ..Default::default()
    };
    let b = backend(Some("bound-model"), Some("team-reasoner"), Some(inline));
    let caps = ResolvedCapabilities::resolve(&b, Some(&config)).unwrap();
    let d = caps.for_principal(ServingPrincipal::MultiplexerModel("warm-pick"));
    assert!(
        !d.emits_leading_reasoning(),
        "card-derived fields are off for a model the card was not bound to"
    );
    assert_eq!(
        d.reasoning_replay_scope(),
        ReasoningReplayScope::CurrentUserTurn,
        "inline backend-scoped fields SURVIVE the retarget"
    );
    let notice = d.retarget_notice.expect("the retarget must be visible");
    assert!(notice.contains("team-reasoner") && notice.contains("warm-pick"));
}

/// **A card bound to no declared model never applies on a multiplexer** —
/// there is nothing to associate the pick with — and the notice says so.
#[test]
fn an_unbound_card_on_a_multiplexer_is_inactive_with_a_notice() {
    let (_dir, config) = catalog_with(&[("team-reasoner", REASONER_CARD)]);
    let b = backend(None, Some("team-reasoner"), None);
    let caps = ResolvedCapabilities::resolve(&b, Some(&config)).unwrap();
    let d = caps.for_principal(ServingPrincipal::MultiplexerModel("whatever"));
    assert!(!d.emits_leading_reasoning());
    assert!(d.retarget_notice.is_some());
    // …but the same unbound card on an INSTANCE applies: one artifact.
    let d = caps.for_principal(ServingPrincipal::Instance);
    assert!(d.emits_leading_reasoning());
}

/// **Unknown serving defers**: inline-only, no notice — a half-initialized
/// choice must not report a retarget it cannot yet know about.
#[test]
fn unknown_serving_defers_without_a_notice() {
    let (_dir, caps) = bound_caps();
    let d = caps.for_principal(ServingPrincipal::Unknown);
    assert!(!d.emits_leading_reasoning());
    assert!(d.retarget_notice.is_none());
}

/// **The decision is pure and stateless**: deciding for a retargeted
/// principal and then for the bound one again re-enables nothing and loses
/// nothing — the flaw the destructive-suppression draft had (a rebuilt
/// choice re-enabled a suppressed card) cannot exist here by construction.
#[test]
fn the_decision_is_stateless_across_switches() {
    let (_dir, caps) = bound_caps();
    let away = caps.for_principal(ServingPrincipal::MultiplexerModel("elsewhere"));
    assert!(!away.emits_leading_reasoning());
    let back = caps.for_principal(ServingPrincipal::MultiplexerModel("bound-model"));
    assert!(
        back.emits_leading_reasoning(),
        "switching BACK to the bound model restores the card layer — the \
         layers were never mutated"
    );
}
