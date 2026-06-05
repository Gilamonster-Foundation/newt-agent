// Integration tests for router classification functionality.

use newt_core::router::Tier;
use newt_core::router::Router;

/// Test that the router correctly classifies commands into appropriate tiers.
#[test]
fn test_router_classification_fast() {
    let router = Router::new();
    let tier = router.classify("list files");
    assert_eq!(tier, Tier::Fast);
}

#[test]
fn test_router_classification_standard() {
    let router = Router::new();
    let tier = router.classify("build a complex docker image");
    assert_eq!(tier, Tier::Standard);
}

#[test]
fn test_router_classification_complex() {
    let router = Router::new();
    let tier = router.classify("optimize a machine learning training pipeline for GPU acceleration");
    assert_eq!(tier, Tier::Complex);
}

#[test]
fn test_router_classification_review() {
    let router = Router::new();
    let tier = router.classify("perform a full security audit and code review of the entire codebase");
    assert_eq!(tier, Tier::Review);
}