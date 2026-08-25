//! **Leading-reasoning comes from a declared capability, end to end.**
//!
//! Before this migration, `reasoning::emits_leading_reasoning(model)` asked
//! `model.contains("nemotron" | "deepseek-r1" | "qwen3")` and the answer
//! decided whether streamed text was suppressed as chain-of-thought or
//! printed into the reply. A display name is a label — an operator serves any
//! artifact under any alias — so that was wrong in both directions and silent
//! in both:
//!
//! * an unrelated model named `my-qwen3-finetune` had answer text eaten;
//! * a genuine Qwen3 served as `ornith-1.0` printed its raw reasoning.
//!
//! **Scope, stated honestly.** These are UNIT tests over
//! `BackendConfig::emits_leading_reasoning()` and the `TurnDriverConfig`
//! hand-off. They do NOT exercise `resolve_backend_choice`; the driver field
//! is assigned by hand here, so they prove the accessor's answer and the
//! field's plumbing — not that any production path fills it. The resolution
//! seam itself is covered in `newt-tui`
//! (`backend_choice_carries_declared_leading_reasoning`), and the private
//! `solve` hand-off by the wiring ratchet in `newt-cli`. Three layers,
//! because a test that constructs the destination proves the struct has a
//! field, and the defect review found was a field nobody filled.
//!
//! Where a capability may be DECLARED today, stated plainly because the PR's
//! story depends on it: inline, on the backend
//! (`[backends.<name>.capability]`). A named `card = "…"` pointer is NOT
//! consulted — see the note at the foot of this file.

use newt_core::model_card::Capability;
use newt_core::BackendKind;
use newt_core::TurnDriverConfig;

fn backend(model: &str, emits: Option<bool>) -> newt_core::BackendConfig {
    let mut b = newt_core::BackendConfig {
        name: "test".to_string(),
        endpoint: "http://127.0.0.1:11434".to_string(),
        ..Default::default()
    };
    b.model = Some(model.to_string());
    if emits.is_some() {
        b.capability = Some(Capability {
            emits_leading_reasoning: emits,
            thinking_default: None,
            reasoning_content_field: None,
            reasoning_replay_scope: None,
            chat_completions: None,
        });
    }
    b
}

/// **The alias grants nothing.** Every one of these names would have enabled
/// filtering under the old substring test; none may now.
#[test]
fn an_alias_never_enables_filtering_without_a_declaration() {
    for alias in [
        "qwen3:8b",
        "my-qwen3-finetune",
        "nemotron-3-nano:30b",
        "deepseek-r1:7b",
        "NEMOTRON-CLONE",
    ] {
        let b = backend(alias, None);
        assert!(
            !b.emits_leading_reasoning(),
            "`{alias}` is a label — with no declared capability the family is \
             Unknown and no family policy applies"
        );
    }
}

/// **The declaration decides, under any alias.** The converse failure: a
/// genuine reasoning model served under an unrelated name.
#[test]
fn a_declaration_enables_filtering_under_an_unrelated_alias() {
    let b = backend("ornith-1.0-35b", Some(true));
    assert!(
        b.emits_leading_reasoning(),
        "the operator declared it; the alias is irrelevant"
    );
}

/// An explicit `false` is a declaration, not an absence — and both resolve to
/// "do not filter", by different routes that must not be conflated.
#[test]
fn an_explicit_false_and_an_absence_both_disable_but_are_distinct() {
    assert!(!backend("m", Some(false)).emits_leading_reasoning());
    assert!(!backend("m", None).emits_leading_reasoning());
    assert_eq!(
        backend("m", Some(false))
            .capability
            .and_then(|c| c.emits_leading_reasoning),
        Some(false),
        "an operator saying no is recorded as no, not as never-asked"
    );
    assert_eq!(
        backend("m", None)
            .capability
            .and_then(|c| c.emits_leading_reasoning),
        None
    );
}

/// **The value survives the driver config**, which is what headless `solve`
/// and the TUI both hand to the turn loop. `TurnDriverConfig::new` defaults
/// it to `false`; a caller that forgets to thread it silently disables the
/// filter — which is exactly the defect independent review found in
/// `newt solve`.
#[test]
fn the_declared_value_reaches_the_driver_config() {
    let b = backend("ornith-1.0-35b", Some(true));
    let mut dc = TurnDriverConfig::new(&b.endpoint, "ornith-1.0-35b", BackendKind::Ollama, "/tmp");
    assert!(
        !dc.emits_leading_reasoning,
        "the constructor's conservative default is OFF — filtering wrongly drops \
         real answer text, so the unset case must fail toward the visible failure"
    );
    dc.emits_leading_reasoning = b.emits_leading_reasoning();
    assert!(
        dc.emits_leading_reasoning,
        "the declaration must survive the hand-off"
    );
}

// ---------------------------------------------------------------------------
// Known gap, stated rather than implied
// ---------------------------------------------------------------------------
//
// `BackendConfig.card` points at a named model card whose `[capability]`
// block ought to apply. It does NOT: `card` is only ever copied between
// configs, never resolved, and ALL THREE capability accessors
// (`chat_completions_capability`, `reasoning_replay_scope`, and now
// `emits_leading_reasoning`) read the inline `capability` field alone.
//
// That gap is pre-existing and uniform. Fixing it for one accessor would
// leave two behaving differently from their sibling, which is worse than a
// consistent gap — so it is its own roadmap step, and until it lands the
// working declaration site is inline on the backend.
//
// A test asserting that the capability survives adoption replacing the
// serving model was written here and REMOVED as fake: it changed `b.model`
// and read `b.capability`, two independent fields, so it could not fail —
// and would not start failing when card resolution lands, because a
// card-reading accessor still ignores a model swap unless a card is
// configured. A tripwire that cannot trip is worse than none: it advertises
// coverage of the exact hazard it does not cover. The real post-adoption and
// per-switch test belongs in the named-card seam PR, where capability
// actually becomes model-dependent and the assertion has something to bite.
