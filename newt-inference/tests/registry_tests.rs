use std::sync::Arc;

use newt_core::router::Tier;
use newt_core::NewtError;
use newt_inference::BackendRegistry;
use tests_common::MockBackend;

#[test]
fn register_and_pick() {
    let mut reg = BackendRegistry::new();
    reg.register(Arc::new(MockBackend::all_tiers("ollama", "ok")));
    let backend = reg.pick(Tier::Fast).expect("should find a backend");
    assert_eq!(backend.name(), "ollama");
}

#[test]
fn registration_order_preserved() {
    let mut reg = BackendRegistry::new();
    reg.register(Arc::new(MockBackend::all_tiers("A", "a")));
    reg.register(Arc::new(MockBackend::all_tiers("B", "b")));
    assert_eq!(reg.names(), vec!["A", "B"]);
}

#[test]
fn pick_returns_first_match() {
    let mut reg = BackendRegistry::new();
    reg.register(Arc::new(MockBackend::new(
        "A",
        "a-model",
        vec![Tier::Fast],
        "a",
    )));
    reg.register(Arc::new(MockBackend::all_tiers("B", "b")));
    let backend = reg.pick(Tier::Fast).expect("should find a backend");
    assert_eq!(backend.name(), "A");
}

#[test]
fn pick_no_match_returns_error() {
    let reg = BackendRegistry::new();
    match reg.pick(Tier::Fast) {
        Err(NewtError::NoBackendForTier(Tier::Fast)) => {} // expected
        Err(other) => panic!("expected NoBackendForTier(Fast), got {other:?}"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

#[test]
fn mixed_tier_support() {
    let mut reg = BackendRegistry::new();
    reg.register(Arc::new(MockBackend::new(
        "A",
        "a-model",
        vec![Tier::Fast, Tier::Standard],
        "a",
    )));
    reg.register(Arc::new(MockBackend::new(
        "B",
        "b-model",
        vec![Tier::Complex, Tier::Review],
        "b",
    )));
    let backend = reg.pick(Tier::Complex).expect("should find B");
    assert_eq!(backend.name(), "B");
}

#[test]
fn len_and_is_empty() {
    let mut reg = BackendRegistry::new();
    assert!(reg.is_empty());
    assert_eq!(reg.len(), 0);

    reg.register(Arc::new(MockBackend::all_tiers("X", "x")));
    assert!(!reg.is_empty());
    assert_eq!(reg.len(), 1);
}
