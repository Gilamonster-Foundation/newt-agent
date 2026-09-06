use super::*;

/// The budget is a plain construction-time value: whatever number the
/// caller resolved (e.g. the TUI's capability-derived figure) is exactly
/// what governs pruning — `usage()` exposes it as the denominator.
#[test]
fn token_budget_construction_injects_budget() {
    let tb = TokenBudget::new(24_000, 0.80);
    let (label, _used, budget) = tb.usage().unwrap();
    assert_eq!(label, "tokens");
    assert_eq!(budget, 19_200); // 24_000 × 0.80
}
/// `with_budget` is the builder form of the same injection, mirroring
/// `with_summarizer` — it replaces the constructor value.
#[test]
fn token_budget_with_budget_overrides_constructor_value() {
    let tb = TokenBudget::new(512, 0.80).with_budget(24_000);
    assert_eq!(tb.usage().unwrap().2, 19_200);
    // Same ≥512 clamp as the constructor.
    let clamped = TokenBudget::new(4_096, 1.0).with_budget(0);
    assert_eq!(clamped.max_tokens, 512);
}
#[test]
fn summarizing_construction_injects_budget() {
    let s = Summarizing::new(32_768);
    let (label, _used, budget) = s.usage().unwrap();
    assert_eq!(label, "tokens");
    assert_eq!(budget, 26_214); // 32_768 × 0.80
}
#[test]
fn summarizing_with_budget_overrides_constructor_value() {
    let s = Summarizing::new(100).with_budget(32_768);
    assert_eq!(s.usage().unwrap().2, 26_214);
    // Same ≥1 clamp as the constructor.
    let clamped = Summarizing::new(100).with_budget(0);
    assert_eq!(clamped.max_tokens, 1);
}
/// The deleted `from_config()` path silently fell back to 8,192 even
/// when the caller had empirical capability data. Construction-time
/// injection means a capability-derived number flows through verbatim —
/// the provider must NOT sit at the static default.
#[test]
fn token_budget_does_not_sit_at_static_default_with_capability_data() {
    // A capability-derived budget (e.g. max_ok_input = 24,000) injected
    // at construction.
    let capability_derived = 24_000;
    let tb = TokenBudget::new(capability_derived, 0.80);
    let static_default_budget = (DEFAULT_CONTEXT_TOKENS as f32 * 0.80) as usize;
    assert_ne!(
        tb.usage().unwrap().2,
        static_default_budget,
        "provider budget must reflect injected capability data, \
         not the static default"
    );
    assert_eq!(tb.usage().unwrap().2, 19_200);
}
#[test]
fn memory_manager_rebinds_context_budget_without_losing_history() {
    let mut manager = MemoryManager::new();
    let mut provider = TokenBudget::new(16_384, 0.80);
    provider.history.push(TurnRecord {
        user: "kept question".to_string(),
        assistant: "kept answer".to_string(),
        est_tokens: 10,
    });
    manager.add_provider(provider);

    manager.set_context_tokens(1_000_000);

    let messages = manager.build_messages("system", "next question");
    assert!(messages.iter().any(|m| m.content == "kept question"));
    assert_eq!(manager.usage()[0].2, 800_000);
}
#[tokio::test]
async fn token_budget_usage_reporting() {
    let tb = TokenBudget::new(1000, 0.80);
    let (label, cur, max) = tb.usage().unwrap();
    assert_eq!(label, "tokens");
    assert_eq!(cur, 0);
    assert_eq!(max, 800); // 1000 * 0.80
}
#[tokio::test]
async fn summarizing_usage_reporting() {
    let s = Summarizing::new(1000);
    let (label, cur, max) = s.usage().unwrap();
    assert_eq!(label, "tokens");
    assert_eq!(cur, 0);
    assert_eq!(max, 800); // 1000 * 0.80
}
