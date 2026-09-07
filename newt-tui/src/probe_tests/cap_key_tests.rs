use super::cap_key;
use newt_core::Serving;

#[test]
fn multiplexer_keys_by_bare_model_name_backwards_compatible() {
    // Every existing model-capabilities.json entry keeps working: the
    // multiplexer key IS the model name, byte-for-byte.
    assert_eq!(
        cap_key(
            Serving::Multiplexer,
            "gpu-runner-ollama",
            "qwen2.5-coder:7b"
        )
        .as_str(),
        "qwen2.5-coder:7b"
    );
}

#[test]
fn instance_keys_by_backend_so_restarts_overwrite_and_twins_dont_collide() {
    // Capabilities attach to the BACKEND for an instance: a vLLM restart
    // with a new model overwrites the same key (stale-by-definition), and
    // two instances serving the same model name stay distinct.
    assert_eq!(
        cap_key(Serving::Instance, "dgx1-vllm-8000", "ornith-1.0-35b").as_str(),
        "backend:dgx1-vllm-8000"
    );
    // The identity law in one place: two DISTINCT instances serving the
    // SAME model name resolve to DISTINCT keys, so neither poisons the
    // other's empirical capability evidence (the #1647 instance-isolation
    // gap this follow-up closes).
    assert_ne!(
        cap_key(Serving::Instance, "dgx1-vllm-8000", "m"),
        cap_key(Serving::Instance, "dgx1-vllm-8001", "m")
    );
    // …while a multiplexer keys purely by model, so two routes to the same
    // model on one gateway intentionally SHARE evidence.
    assert_eq!(
        cap_key(Serving::Multiplexer, "gpu-runner-ollama", "m"),
        cap_key(Serving::Multiplexer, "other-gateway", "m")
    );
}

/// The cache is serde-`transparent` over the inner string, so the on-disk
/// `model-capabilities.json` is byte-for-byte the pre-newtype format:
/// existing (multiplexer, bare-model-keyed) files keep round-tripping, and
/// an instance entry persists as `backend:<name>`.
#[test]
fn capability_cache_round_trips_through_the_legacy_string_key_format() {
    use super::{CapabilityCache, CapabilityEntry};
    let entry = |max_ok_input: Option<u32>, safe_context: Option<u32>| CapabilityEntry {
        conformance: super::ToolConformance::Native,
        tested_date: "2026-06-10".into(),
        safe_context,
        max_ok_input,
        ..Default::default()
    };
    let mut cache = CapabilityCache::default();
    cache.insert(
        cap_key(
            Serving::Multiplexer,
            "gpu-runner-ollama",
            "qwen2.5-coder:7b",
        ),
        entry(Some(24_000), Some(26_214)),
    );
    cache.insert(
        cap_key(Serving::Instance, "dgx1-vllm-8000", "m"),
        entry(Some(120_000), None),
    );
    let json = serde_json::to_string(&cache).unwrap();
    // Legacy string keys, exactly as prior versions wrote them.
    assert!(json.contains("\"qwen2.5-coder:7b\""));
    assert!(json.contains("\"backend:dgx1-vllm-8000\""));
    let restored: CapabilityCache = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, cache);
}

/// INSTANCE ISOLATION — the headline #1647 gap this follow-up closes. Two
/// vLLM `Serving::Instance` backends A and B serve the SAME model name "m"
/// but have different real capabilities. Exercising A → B → A must prove B
/// never inherits A's `max_ok_input` / `safe_context` / recovered window /
/// `estimate_ratio` / budget, and B's smaller limit + failure evidence never
/// poison A. (Before the cap_key rekey, both keyed by "m" and collided.)
#[test]
fn two_instances_serving_the_same_model_are_isolated_through_a_to_b_to_a() {
    use super::{resolve_memory_budget, CapKey, CapabilityCache, CapabilityEntry};
    let entry = |max_ok_input: Option<u32>,
                 safe_context: Option<u32>,
                 estimate_ratio: Option<f32>| CapabilityEntry {
        conformance: super::ToolConformance::Native,
        tested_date: "2026-08-01".into(),
        safe_context,
        max_ok_input,
        estimate_ratio,
        ..Default::default()
    };
    // A: a 128K-class instance; B: a 32K-class instance — SAME model "m".
    let key_a = cap_key(Serving::Instance, "vllm-8000", "m");
    let key_b = cap_key(Serving::Instance, "vllm-8001", "m");
    assert_ne!(
        key_a, key_b,
        "same model, distinct backends → distinct keys"
    );

    let mut cache = CapabilityCache::default();
    cache.insert(
        key_a.clone(),
        entry(Some(120_000), Some(120_000), Some(1.10)),
    );
    cache.insert(key_b.clone(), entry(Some(30_000), Some(30_000), Some(0.90)));

    // A → B → A: each resolves to ITS OWN budget, never the other's.
    assert_eq!(
        resolve_memory_budget(None, None, cache.get(&key_a)),
        120_000
    );
    assert_eq!(
        resolve_memory_budget(None, None, cache.get(&key_b)),
        30_000,
        "B must NOT inherit A's 120K high-water mark / safe context / budget"
    );
    assert_eq!(
        resolve_memory_budget(None, None, cache.get(&key_a)),
        120_000,
        "A survives the round trip unchanged"
    );
    // Distinct empirical state — ratios never cross.
    assert_eq!(cache.get(&key_a).unwrap().estimate_ratio, Some(1.10));
    assert_eq!(cache.get(&key_b).unwrap().estimate_ratio, Some(0.90));

    // B hits a hard 400 and tightens its OWN safe_context — A is untouched.
    cache.get_mut(&key_b).unwrap().safe_context = Some(16_000);
    assert_eq!(
        resolve_memory_budget(None, None, cache.get(&key_a)),
        120_000,
        "B's failure/recovery must not poison A"
    );

    // recovered_context_windows is the same HashMap<CapKey, u32> run_chat
    // now keys by cap_id: B's recovery ceiling never lands on A's key.
    let mut recovered = std::collections::HashMap::<CapKey, u32>::new();
    recovered.insert(key_b.clone(), 16_000);
    assert_eq!(recovered.get(&key_a).copied(), None);
    assert_eq!(recovered.get(&key_b).copied(), Some(16_000));

    // MULTIPLEXER SHARING preserved: two routes to the same model on a
    // gateway intentionally share one model-keyed entry.
    let mux = cap_key(Serving::Multiplexer, "gpu-runner-ollama", "m");
    cache.insert(mux.clone(), entry(Some(64_000), Some(64_000), None));
    assert_eq!(resolve_memory_budget(None, None, cache.get(&mux)), 64_000);
    assert_eq!(
        cache.get(&cap_key(Serving::Multiplexer, "other-route", "m")),
        cache.get(&mux),
        "any route to model m on a multiplexer shares the same evidence"
    );
}
