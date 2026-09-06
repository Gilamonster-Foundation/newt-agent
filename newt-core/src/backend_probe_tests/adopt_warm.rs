use super::*;

fn served_warm(models: &[&str], warm: &[&str]) -> Served {
    Served {
        models: models.iter().map(|m| m.to_string()).collect(),
        warm: warm.iter().map(|m| m.to_string()).collect(),
    }
}

fn managed_mux(model: Option<&str>, mode: ManagedMode) -> BackendConfig {
    BackendConfig {
        managed: Some(mode),
        ..openai_backend(model, Some(Serving::Multiplexer))
    }
}
// ── ManagedMode::Shared adopt-warm (ADR docs/decisions/managed_backend.md) ──

#[test]
fn shared_adopts_warm_when_nothing_is_pinned() {
    // A cooperative guest with no pin uses whatever model is already loaded.
    let b = managed_mux(None, ManagedMode::Shared);
    let a = adopt(&b, &served_warm(&["cold", "warm-y"], &["warm-y"]), None);
    assert_eq!(a.model.as_deref(), Some("warm-y"));
    assert!(a.adopted_warm);
    assert_eq!(a.pin_conflict, None);
}

#[test]
fn shared_adopts_warm_over_a_conflicting_pin_and_offers_the_force_choice() {
    // Pinned "mine" but "warm-y" is loaded: adopt the warm one (never evict
    // another agent's model silently) and hand the pin back as a force choice.
    let b = managed_mux(Some("mine"), ManagedMode::Shared);
    let a = adopt(&b, &served_warm(&["mine", "warm-y"], &["warm-y"]), None);
    assert_eq!(
        a.model.as_deref(),
        Some("warm-y"),
        "cooperative default = warm"
    );
    assert!(a.adopted_warm);
    assert_eq!(
        a.pin_conflict.as_deref(),
        Some("mine"),
        "the pin is surfaced as the force-swap choice"
    );
}

#[test]
fn shared_keeps_the_pin_when_the_pin_is_already_warm() {
    // No swap, no conflict — the pinned model happens to be resident.
    let b = managed_mux(Some("mine"), ManagedMode::Shared);
    let a = adopt(&b, &served_warm(&["mine", "other"], &["mine"]), None);
    assert_eq!(a.model.as_deref(), Some("mine"));
    assert!(!a.adopted_warm);
    assert_eq!(a.pin_conflict, None);
}

#[test]
fn shared_loads_the_pin_when_nothing_is_warm() {
    // Nothing resident: fall back to the pin (an unavoidable cold load).
    let b = managed_mux(Some("mine"), ManagedMode::Shared);
    let a = adopt(&b, &served_warm(&["mine", "other"], &[]), None);
    assert_eq!(a.model.as_deref(), Some("mine"));
    assert!(!a.adopted_warm);
    assert_eq!(a.pin_conflict, None);
}

#[test]
fn shared_surfaces_the_conflict_for_a_session_request_too() {
    // An explicit /model request is still a pin: on a Shared box it does not
    // silently force a swap — the warm model wins by default and the request
    // is offered as the force choice (the two-agent clash the ADR guards).
    let b = managed_mux(Some("declared"), ManagedMode::Shared);
    let a = adopt(
        &b,
        &served_warm(&["asked", "warm-y"], &["warm-y"]),
        Some("asked"),
    );
    assert_eq!(a.model.as_deref(), Some("warm-y"));
    assert!(a.adopted_warm);
    assert_eq!(a.pin_conflict.as_deref(), Some("asked"));
}

#[test]
fn dedicated_forces_the_pin_and_never_adopts_warm() {
    // "I own this box": force the configured model even if another is warm.
    let b = managed_mux(Some("mine"), ManagedMode::Dedicated);
    let a = adopt(&b, &served_warm(&["mine", "warm-y"], &["warm-y"]), None);
    assert_eq!(a.model.as_deref(), Some("mine"), "dedicated forces its pin");
    assert!(!a.adopted_warm);
    assert_eq!(a.pin_conflict, None);
}

#[test]
fn unmanaged_keeps_precedence_warm_is_only_a_tiebreaker() {
    // Regression: an ordinary (unmanaged) backend is unchanged — the declared
    // pin wins over a differently-warm model (warmth never overrides a pin).
    let b = openai_backend(Some("declared"), Some(Serving::Multiplexer));
    let a = adopt(&b, &served_warm(&["declared", "warm-y"], &["warm-y"]), None);
    assert_eq!(a.model.as_deref(), Some("declared"));
    assert!(!a.adopted_warm);
    assert_eq!(a.pin_conflict, None);
}
// --- adopt(): warm precedence ---

#[test]
fn multiplexer_prefers_warm_over_first_served() {
    let backend = BackendConfig {
        name: "b".into(),
        endpoint: "http://h:11434".into(),
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    let adoption = adopt(&backend, &served_warm(&["a", "b", "c"], &["c"]), None);
    assert_eq!(adoption.model.as_deref(), Some("c"));
    assert!(!adoption.requested_unavailable);
}

#[test]
fn requested_and_declared_still_outrank_warm() {
    let declared = BackendConfig {
        name: "b".into(),
        endpoint: "http://h:11434".into(),
        model: Some("b".into()),
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    // Requested wins over everything.
    let adoption = adopt(&declared, &served_warm(&["a", "b", "c"], &["c"]), Some("a"));
    assert_eq!(adoption.model.as_deref(), Some("a"));
    // Declared wins over warm.
    let adoption = adopt(&declared, &served_warm(&["a", "b", "c"], &["c"]), None);
    assert_eq!(adoption.model.as_deref(), Some("b"));
}

#[test]
fn stale_warm_entry_not_in_served_is_ignored() {
    let backend = BackendConfig {
        name: "b".into(),
        endpoint: "http://h:11434".into(),
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    };
    // /api/ps race: the warm model was just removed from /api/tags.
    let adoption = adopt(&backend, &served_warm(&["a", "b"], &["gone"]), None);
    assert_eq!(
        adoption.model.as_deref(),
        Some("a"),
        "falls to first served"
    );
}

#[test]
fn instance_adoption_unchanged_by_warm() {
    let backend = openai_backend(Some("requested"), Some(Serving::Instance));
    let adoption = adopt(&backend, &served_warm(&["served"], &["served"]), None);
    assert_eq!(adoption.model.as_deref(), Some("served"));
    assert!(adoption.requested_ignored);
}
