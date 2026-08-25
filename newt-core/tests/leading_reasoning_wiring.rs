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
//! Where a capability may be declared: inline on the backend
//! (`[backends.<name>.capability]`), or via a named `card = "…"` binding —
//! resolved by `ResolvedCapabilities` (see `resolved_capabilities.rs` for
//! that seam's own tests). The unit tests HERE cover the inline accessor and
//! the driver hand-off only.

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
// The named-card gap this file used to document is CLOSED: `BackendConfig.card`
// now resolves through `ResolvedCapabilities` (one exact-name resolver shared
// with `dgx card`), decided per serving principal. The inline accessors below
// remain inline-only BY DESIGN — they are the conservative floor, and the
// sidecar is the card-aware surface every lane consumes.
