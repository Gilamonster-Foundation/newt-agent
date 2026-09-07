use super::*;

/// Build a capability key the way a Multiplexer (Ollama) backend would, so
/// migrate/lookup tests can key by a bare model name through the ONE
/// canonical constructor (no raw-String keys — that is now a type error).
fn mk(name: &str) -> CapKey {
    cap_key(newt_core::Serving::Multiplexer, "", name)
}

#[test]
fn looks_like_tool_call_json_native_object() {
    assert!(looks_like_tool_call_json(
        r#"{"name":"list_dir","arguments":{"path":"."}}"#
    ));
}

#[test]
fn looks_like_tool_call_json_array() {
    assert!(looks_like_tool_call_json(
        r#"[{"name":"list_dir","arguments":{"path":"."}}]"#
    ));
}

#[test]
fn looks_like_tool_call_json_plain_text() {
    assert!(!looks_like_tool_call_json(
        "Here are the files: README.md, src/"
    ));
}

#[test]
fn looks_like_tool_call_json_incomplete_object() {
    // Has "name" but no "arguments" — not a tool call.
    assert!(!looks_like_tool_call_json(r#"{"name":"list_dir"}"#));
}

#[test]
fn load_cache_returns_empty_on_missing_file() {
    // Can't mock the path, but at minimum it must not panic.
    let _ = load_cache();
}

#[test]
fn conformance_symbol_coverage() {
    assert!(ToolConformance::Native.symbol().contains('✓'));
    assert!(ToolConformance::TextMode.symbol().contains('~'));
    assert!(ToolConformance::NoTools.symbol().contains('✗'));
}

#[test]
fn tune_confidence_promotes_correctly() {
    assert_eq!(TuneConfidence::None.promote(), TuneConfidence::Low);
    assert_eq!(TuneConfidence::Low.promote(), TuneConfidence::Medium);
    assert_eq!(TuneConfidence::Medium.promote(), TuneConfidence::High);
    assert_eq!(TuneConfidence::High.promote(), TuneConfidence::High);
}

fn make_entry() -> CapabilityEntry {
    CapabilityEntry {
        conformance: ToolConformance::Native,
        tested_date: "2026-06-06".to_string(),
        context_window: Some(32768),
        safe_context: Some(26214),
        ..Default::default()
    }
}

#[test]
fn record_success_updates_max_ok_input() {
    let mut e = make_entry();
    e.record_success(10_000, "2026-06-06");
    assert_eq!(e.max_ok_input, Some(10_000));
    e.record_success(8_000, "2026-06-06");
    // Lower value should not replace higher.
    assert_eq!(e.max_ok_input, Some(10_000));
}

#[test]
fn record_success_promotes_confidence_after_five() {
    let mut e = make_entry();
    for i in 0..4 {
        e.record_success(5_000, "2026-06-06");
        assert_eq!(e.tune_confidence, TuneConfidence::None, "early iter {i}");
    }
    e.record_success(5_000, "2026-06-06");
    assert_eq!(e.tune_confidence, TuneConfidence::Low);
    assert_eq!(e.consecutive_ok, 0); // reset after promotion
}

#[test]
fn record_overflow_reduces_safe_context() {
    let mut e = make_entry();
    e.record_overflow(30_000, "2026-06-06");
    // 30_000 * 75 / 100 = 22_500
    assert_eq!(e.safe_context, Some(22_500));
    assert_eq!(e.tune_confidence, TuneConfidence::Low);
    assert_eq!(e.overflow_at, Some(30_000));
}

/// Phase 20 (§2.1): overflow learning was inert because both budget
/// resolvers prefer the larger figure and only `safe_context` was
/// lowered — `record_overflow` must now rein `max_ok_input` too.
#[test]
fn record_overflow_reins_max_ok_input_down() {
    let mut e = make_entry();
    e.max_ok_input = Some(28_000);
    e.record_overflow(20_000, "2026-06-12");
    // 20_000 * 75% = 15_000 — both figures reined to the same cap.
    assert_eq!(e.safe_context, Some(15_000));
    assert_eq!(e.max_ok_input, Some(15_000));
    // A LOWER existing ratchet is untouched (never raised by overflow).
    let mut e2 = make_entry();
    e2.max_ok_input = Some(10_000);
    e2.record_overflow(20_000, "2026-06-12");
    assert_eq!(e2.max_ok_input, Some(10_000));
    // An absent ratchet stays absent — overflow proves no acceptance.
    let mut e3 = make_entry();
    e3.record_overflow(20_000, "2026-06-12");
    assert_eq!(e3.max_ok_input, None);
}

// --- record_accepted_prompt (Phase 20 §2.2) ---

#[test]
fn record_accepted_prompt_is_a_pure_high_water_ratchet() {
    let mut e = make_entry();
    e.consecutive_ok = 3;
    e.tune_confidence = TuneConfidence::Medium;
    assert!(
        e.record_accepted_prompt(8_734, "2026-06-12"),
        "first: dirty"
    );
    assert_eq!(e.max_ok_input, Some(8_734));
    assert_eq!(e.tune_date.as_deref(), Some("2026-06-12"), "date stamped");
    // Confidence accounting is the turn-level record_success's job — a
    // multi-round turn must not inflate it per round.
    assert_eq!(e.consecutive_ok, 3, "untouched");
    assert_eq!(e.tune_confidence, TuneConfidence::Medium, "untouched");
    // Equal or lower observations are not dirty and do not lower.
    assert!(
        !e.record_accepted_prompt(8_734, "2026-06-13"),
        "equal: clean"
    );
    assert!(
        !e.record_accepted_prompt(4_000, "2026-06-13"),
        "lower: clean"
    );
    assert_eq!(e.max_ok_input, Some(8_734), "HWM only raises");
    assert_eq!(e.tune_date.as_deref(), Some("2026-06-12"), "no re-stamp");
    // Strictly higher raises again.
    assert!(e.record_accepted_prompt(9_000, "2026-06-13"));
    assert_eq!(e.max_ok_input, Some(9_000));
    assert_eq!(e.tune_date.as_deref(), Some("2026-06-13"));
}

// --- record_estimate_sample (Phase 20 §2.3) ---

#[test]
fn record_estimate_sample_initializes_then_emas() {
    let mut e = make_entry();
    // Init: first sample is stored verbatim. 8_734 / 6_600 ≈ 1.3233…
    assert!(e.record_estimate_sample(8_734, 6_600));
    let first = e.estimate_ratio.unwrap();
    assert!((first - 8_734.0 / 6_600.0).abs() < 1e-6, "got {first}");
    // EMA: 0.75·old + 0.25·sample (sample 2.0 here).
    assert!(e.record_estimate_sample(2_000, 1_000));
    let second = e.estimate_ratio.unwrap();
    assert!(
        (second - (0.75 * first + 0.25 * 2.0)).abs() < 1e-6,
        "got {second}"
    );
}

#[test]
fn record_estimate_sample_clamps_the_high_end() {
    // A wild over-report clamps the SAMPLE to 3.0 before the EMA.
    let mut e = make_entry();
    assert!(e.record_estimate_sample(10_000, 1_000)); // raw 10.0
    assert_eq!(e.estimate_ratio, Some(3.0), "init clamped to 3.0");
    // The EMA result is clamped too: stored value can never exceed 3.0
    // no matter the history.
    assert!(!e.record_estimate_sample(10_000, 1_000), "3.0 → 3.0: clean");
    assert_eq!(e.estimate_ratio, Some(3.0));
}

/// #1968 anti-vacuous pair: a cache-hit-SHAPED sample (`raw` in
/// `[0.5, 1.0)` — the exact band the pre-fix code admitted) is now
/// excluded outright, and its twin — a genuinely fresh, full-eval
/// sample at `raw >= 1.0` — still updates the EMA normally. Before this
/// fix only `raw < 0.5` was skipped; a `raw` of exactly 0.9 (a plausible
/// partial cache hit) fed straight into the EMA.
#[test]
fn record_estimate_sample_excludes_the_partial_cache_hit_band() {
    // The twin first: a legitimate full-eval sample (raw 1.0, the exact
    // new boundary) is NOT excluded and initializes the ratio.
    let mut clean = make_entry();
    assert!(clean.record_estimate_sample(1_000, 1_000));
    assert_eq!(clean.estimate_ratio, Some(1.0));

    // Now the excluded band: raw 0.9 — previously admitted (>= the old
    // 0.5 skip line), now excluded. Nothing stored.
    let mut cache_hit = make_entry();
    assert!(!cache_hit.record_estimate_sample(900, 1_000));
    assert_eq!(
        cache_hit.estimate_ratio, None,
        "a [0.5, 1.0) sample must not seed the ratio"
    );
    // And it must not disturb an already-learned ratio either — this is
    // exactly #1968's incident shape: a partial cache hit arriving
    // mid-session must not drag a previously-healthy EMA down.
    cache_hit.estimate_ratio = Some(1.3);
    assert!(!cache_hit.record_estimate_sample(900, 1_000));
    assert_eq!(cache_hit.estimate_ratio, Some(1.3));

    // The pre-fix admitted range's boundary, restated: raw just under
    // 1.0 is excluded; raw at or above 1.0 is not.
    let mut boundary = make_entry();
    assert!(!boundary.record_estimate_sample(999, 1_000)); // raw 0.999
    assert!(boundary.record_estimate_sample(1_001, 1_000)); // raw 1.001
}

#[test]
fn record_estimate_sample_skips_cache_hits_and_zero_estimates() {
    let mut e = make_entry();
    // A full Ollama prompt-cache hit: observed well under estimated —
    // would poison the ratio downward (spec §2.3, #1968). Skipped,
    // nothing stored.
    assert!(!e.record_estimate_sample(400, 1_000));
    assert_eq!(e.estimate_ratio, None);
    // Zero estimate: no honest ratio exists.
    assert!(!e.record_estimate_sample(400, 0));
    assert_eq!(e.estimate_ratio, None);
    // And a skip never disturbs an already-learned ratio.
    e.estimate_ratio = Some(1.3);
    assert!(!e.record_estimate_sample(100, 1_000));
    assert_eq!(e.estimate_ratio, Some(1.3));
}

#[test]
fn record_estimate_sample_dirty_only_above_threshold() {
    let mut e = make_entry();
    assert!(e.record_estimate_sample(1_300, 1_000)); // ratio 1.3
    let stored = e.estimate_ratio.unwrap();
    // A near-identical sample moves the EMA by ≪ 0.01: value updates
    // in memory but the call reports CLEAN (no save thrash).
    assert!(!e.record_estimate_sample(1_301, 1_000));
    let drifted = e.estimate_ratio.unwrap();
    assert!((drifted - stored).abs() < 0.01, "stored as-is, tiny drift");
    // A materially different sample (raw 2.0) moves the EMA by ~0.17.
    assert!(e.record_estimate_sample(2_000, 1_000));
}

// --- record_thinking_only (Phase 20 §2.1) ---

#[test]
fn record_thinking_only_is_sticky_and_dirty_once() {
    let mut e = make_entry();
    assert_eq!(e.emits_thinking, None);
    assert!(e.record_thinking_only(), "first observation: dirty");
    assert_eq!(e.emits_thinking, Some(true));
    assert!(!e.record_thinking_only(), "repeat: clean");
    assert_eq!(e.emits_thinking, Some(true));
}

// --- apply_observation (Phase 20 §2.2 dispatch seam) ---

#[test]
fn apply_observation_dispatches_each_variant() {
    let today = "2026-06-12";
    // Accepted → ratchet AND calibration sample (OR of both flags).
    let mut e = make_entry();
    let obs = newt_core::RoundObservation::Accepted {
        prompt_tokens: 8_734,
        estimated_tokens: 6_600,
    };
    assert!(apply_observation(&mut e, &obs, today));
    assert_eq!(e.max_ok_input, Some(8_734));
    assert!(e.estimate_ratio.is_some());
    // Same observation again: ratchet clean AND ratio drift below the
    // save threshold → overall clean.
    assert!(!apply_observation(&mut e, &obs, today));
    // Ratchet clean but the calibration sample materially different →
    // still dirty (the OR must not short-circuit the second record).
    let recal = newt_core::RoundObservation::Accepted {
        prompt_tokens: 8_000,
        estimated_tokens: 3_000,
    };
    assert!(apply_observation(&mut e, &recal, today));
    assert_eq!(e.max_ok_input, Some(8_734), "lower prompt: no ratchet");

    // SuspectedOverflow → record_overflow (always dirty, reins both).
    let mut e = make_entry();
    e.max_ok_input = Some(28_000);
    let obs = newt_core::RoundObservation::SuspectedOverflow {
        prompt_tokens: 20_000,
    };
    assert!(apply_observation(&mut e, &obs, today));
    assert_eq!(e.safe_context, Some(15_000));
    assert_eq!(e.max_ok_input, Some(15_000));
    assert_eq!(e.overflow_at, Some(20_000));

    // ThinkingOnly → sticky quirk.
    let mut e = make_entry();
    assert!(apply_observation(
        &mut e,
        &newt_core::RoundObservation::ThinkingOnly,
        today
    ));
    assert_eq!(e.emits_thinking, Some(true));
    assert!(!apply_observation(
        &mut e,
        &newt_core::RoundObservation::ThinkingOnly,
        today
    ));

    // A numbered context-window 400 updates the same in-memory entry as
    // the accepted retry, so the latter cannot overwrite recovered facts
    // from a separately loaded stale cache.
    let mut e = make_entry();
    e.context_window = Some(65_536);
    e.safe_context = Some(52_428);
    e.max_ok_input = Some(52_428);
    assert!(apply_observation(
        &mut e,
        &newt_core::RoundObservation::ContextWindow400 {
            context_window: 32_768,
        },
        today,
    ));
    assert_eq!(e.context_window, Some(32_768));
    assert_eq!(e.safe_context, Some(26_214));
    assert_eq!(e.max_ok_input, Some(26_214));
    assert!(apply_observation(
        &mut e,
        &newt_core::RoundObservation::Accepted {
            prompt_tokens: 1_000,
            estimated_tokens: 1_000,
        },
        today,
    ));
    assert_eq!(e.context_window, Some(32_768));
    assert_eq!(e.safe_context, Some(26_214));
}

/// New fields round-trip through JSON and stay absent (not `null`) when
/// unset — additive format change, old caches parse unchanged.
#[test]
fn estimate_ratio_and_emits_thinking_roundtrip_json() {
    let mut e = make_entry();
    e.estimate_ratio = Some(1.29);
    e.emits_thinking = Some(true);
    let json = serde_json::to_string(&e).unwrap();
    let back: CapabilityEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back.estimate_ratio, Some(1.29));
    assert_eq!(back.emits_thinking, Some(true));
    // Unset → keys skipped entirely.
    let bare = serde_json::to_string(&make_entry()).unwrap();
    assert!(!bare.contains("estimate_ratio"), "{bare}");
    assert!(!bare.contains("emits_thinking"), "{bare}");
}

#[test]
fn record_overflow_does_not_increase_safe_context() {
    let mut e = make_entry();
    e.safe_context = Some(10_000);
    // Overflow at only 5_000 — 75% = 3_750; safe_context must shrink.
    e.record_overflow(5_000, "2026-06-06");
    assert_eq!(e.safe_context, Some(3_750));
    // A second overflow at a higher token count should not raise safe_context.
    e.record_overflow(40_000, "2026-06-06");
    // 40_000 * 75% = 30_000 > 3_750 → new_safe > old; plan says keep the lower.
    // Actually looking at the impl: changed = new_safe < current → false → skip.
    assert_eq!(e.safe_context, Some(3_750));
}

#[test]
fn parse_context_window_error_none_for_unrelated_400() {
    let msg = "inference endpoint 400: invalid api key";
    assert_eq!(super::parse_context_window_error(msg), None);
}

#[test]
fn parse_context_window_error_extracts_prompt_and_max() {
    // The real litellm body from issue #223, embedded in the harness's
    // "inference endpoint 400: <body>" wrapper.
    let msg = "inference endpoint 400: litellm.ContextWindowExceededError: prompt is too long: 5960028 tokens > 1000000 maximum";
    assert_eq!(
        super::parse_context_window_error(msg),
        Some((5_960_028, 1_000_000))
    );
}

#[test]
fn parse_context_window_error_extracts_vllm_output_and_prompt_limits() {
    // Exact vLLM 0.19 validation wording when requested output plus prompt
    // input exceeds max_model_len (wrapped as Newt sees the HTTP 400 body).
    let msg = "inference endpoint 400 Bad Request: This model's maximum context length is 32768 tokens. However, you requested 16000 output tokens and your prompt contains 20000 input tokens, for a total of 36000 tokens (20000 + 16000 = 36000 > 32768). Please reduce the length of the input prompt or the number of requested output tokens.";
    assert_eq!(
        super::parse_context_window_error(msg),
        Some((20_000, 32_768))
    );
}

#[test]
fn parse_context_window_error_extracts_vllm_input_only_limit() {
    // Exact sibling validation wording when the input alone reaches the
    // full model window.
    let msg = "inference endpoint 400 Bad Request: This model's maximum context length is 32768 tokens. However, your request has 33000 input tokens. Please reduce the length of the input messages.";
    assert_eq!(
        super::parse_context_window_error(msg),
        Some((33_000, 32_768))
    );
}

#[test]
fn parse_context_window_error_none_without_max_clause() {
    // Truncated message missing the max half must not panic.
    let msg = "prompt is too long: 5960028 tokens";
    assert_eq!(super::parse_context_window_error(msg), None);
}

#[test]
fn record_context_window_400_tightens_max_ok_input_to_80pct() {
    // Reproduces issue #223: max_ok_input was stale-high (251_640) while the
    // endpoint's real limit is 1_000_000. A 400 must pull the gate down.
    let mut e = make_entry();
    e.max_ok_input = Some(251_640);
    let dirty = e.record_context_window_400(1_000_000, "2026-06-08");
    assert!(dirty);
    assert_eq!(e.context_window, Some(1_000_000));
    // 1_000_000 * 80% = 800_000 (headroom below the hard max).
    assert_eq!(e.max_ok_input, Some(800_000));
    assert_eq!(e.tune_confidence, TuneConfidence::Low);
    assert_eq!(e.consecutive_ok, 0);
}

#[test]
fn runtime_context_window_400_persists_the_configured_percentage_cap() {
    let mut e = make_entry();
    e.max_ok_input = Some(52_428);
    e.record_context_window_400_with_pct(32_768, 90, "2026-08-01");
    assert_eq!(e.context_window, Some(32_768));
    assert_eq!(e.hard_context_window, Some(32_768));
    assert_eq!(e.max_ok_input, Some(29_491));

    e.record_context_window_400_with_pct(65_536, 90, "2026-08-01");
    assert_eq!(
        e.hard_context_window,
        Some(32_768),
        "a later, larger error must not raise the persisted hard ceiling",
    );
    assert_eq!(e.context_window, Some(32_768));
    assert_eq!(e.max_ok_input, Some(29_491));

    let persisted = serde_json::to_string(&e).unwrap();
    let restored: CapabilityEntry = serde_json::from_str(&persisted).unwrap();
    assert_eq!(restored.hard_context_window, Some(32_768));
}

#[test]
fn record_context_window_400_lowers_an_overshot_cap() {
    // When tuning had overshot (max_ok_input above the model's real max),
    // a 400 pulls the gate down to 80% of the reported limit.
    let mut e = make_entry();
    e.max_ok_input = Some(2_000_000);
    e.record_context_window_400(1_000_000, "2026-06-08");
    assert_eq!(e.max_ok_input, Some(800_000));
}

#[test]
fn record_context_window_400_caps_safe_context_without_raising_it() {
    let mut e = make_entry();
    e.safe_context = Some(64_000); // small KV window
                                   // 80% of 1_000_000 = 800_000 > 64_000 → safe_context must NOT rise.
    e.record_context_window_400(1_000_000, "2026-06-08");
    assert_eq!(e.safe_context, Some(64_000));
}

#[test]
fn fmt_k_formats_correctly() {
    assert_eq!(fmt_k(1024), "1k");
    assert_eq!(fmt_k(32768), "32k");
    assert_eq!(fmt_k(131072), "128k");
    assert_eq!(fmt_k(512), "512");
}

#[test]
fn capability_entry_roundtrips_json_with_new_fields() {
    let mut e = make_entry();
    e.overflow_at = Some(28_000);
    e.max_ok_input = Some(25_000);
    e.tune_confidence = TuneConfidence::Medium;
    e.tune_date = Some("2026-06-06".to_string());
    let json = serde_json::to_string(&e).unwrap();
    let back: CapabilityEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(back.context_window, Some(32768));
    assert_eq!(back.overflow_at, Some(28_000));
    assert_eq!(back.tune_confidence, TuneConfidence::Medium);
}

#[test]
fn capability_entry_deserializes_legacy_json_without_new_fields() {
    // Old cache entries only have conformance + tested_date.
    let legacy = r#"{"conformance":"native","tested_date":"2026-06-04"}"#;
    let e: CapabilityEntry = serde_json::from_str(legacy).unwrap();
    assert_eq!(e.conformance, ToolConformance::Native);
    assert_eq!(e.context_window, None);
    assert_eq!(e.tune_confidence, TuneConfidence::None);
    // Missing accounting_version means the double-counting regime —
    // NOT the current version that in-process Default entries get.
    assert_eq!(e.accounting_version, 0);
}

// --- migrate_accounting (Step 18.1 ratchet de-poison) ---

/// The live poisoned entry from the B3 baseline: max_ok_input 25,602 at
/// High confidence when the largest evaluated prompt was 4,748 tokens
/// (and safe_context was 6,553 — provably impossible). Versionless →
/// invalidated once; tuning that is honest either way survives.
#[test]
fn migrate_accounting_invalidates_poisoned_entry() {
    let mut cache = CapabilityCache::default();
    cache.insert(
        mk("llama3.1:8b"),
        CapabilityEntry {
            conformance: ToolConformance::Native,
            tested_date: "2026-06-08".into(),
            context_window: Some(8_192),
            hard_context_window: None,
            safe_context: Some(6_553),
            overflow_at: None,
            max_ok_input: Some(25_602),
            consecutive_ok: 3,
            tune_confidence: TuneConfidence::High,
            tune_date: Some("2026-06-08".into()),
            estimate_ratio: None,
            emits_thinking: None,
            accounting_version: 0, // pre-18.1 (missing in the JSON)
        },
    );
    assert!(
        migrate_accounting(&mut cache),
        "migration must report dirty"
    );
    let e = &cache[&mk("llama3.1:8b")];
    assert_eq!(e.max_ok_input, None, "poisoned ratchet value dropped");
    assert_eq!(e.consecutive_ok, 0);
    assert_eq!(e.tune_confidence, TuneConfidence::None);
    assert_eq!(e.accounting_version, ACCOUNTING_VERSION);
    // Non-ratchet state survives: the declared window and the
    // conservatively-derived safe_context are not regime-dependent.
    assert_eq!(e.context_window, Some(8_192));
    assert_eq!(e.safe_context, Some(6_553));
    assert_eq!(e.conformance, ToolConformance::Native);
}

/// A clean current-version entry — including the legitimate post-#223
/// shape where max_ok_input (from the endpoint's reported hard limit)
/// exceeds the VRAM-capped safe_context — must be left untouched.
#[test]
fn migrate_accounting_leaves_current_version_entry_untouched() {
    let mut cache = CapabilityCache::default();
    let entry = CapabilityEntry {
        conformance: ToolConformance::Native,
        tested_date: "2026-06-09".into(),
        safe_context: Some(64_000),
        max_ok_input: Some(800_000), // cw-400 discovery: legit > safe_context
        consecutive_ok: 2,
        tune_confidence: TuneConfidence::Medium,
        ..Default::default() // accounting_version = current
    };
    cache.insert(mk("hosted-model"), entry.clone());
    assert!(!migrate_accounting(&mut cache), "nothing to migrate");
    let e = &cache[&mk("hosted-model")];
    assert_eq!(e.max_ok_input, Some(800_000));
    assert_eq!(e.consecutive_ok, 2);
    assert_eq!(e.tune_confidence, TuneConfidence::Medium);
}

/// A versionless entry WITHOUT tuning values just gets stamped (still
/// dirty — the stamp itself must persist so the check never re-runs).
#[test]
fn migrate_accounting_stamps_untuned_legacy_entry() {
    let mut cache = CapabilityCache::default();
    cache.insert(
        mk("old-model"),
        CapabilityEntry {
            conformance: ToolConformance::TextMode,
            tested_date: "2026-06-04".into(),
            accounting_version: 0,
            ..Default::default()
        },
    );
    assert!(migrate_accounting(&mut cache));
    assert_eq!(
        cache[&mk("old-model")].accounting_version,
        ACCOUNTING_VERSION
    );
    assert_eq!(
        cache[&mk("old-model")].conformance,
        ToolConformance::TextMode
    );
}

/// Running the migration twice must be a no-op the second time.
#[test]
fn migrate_accounting_is_idempotent() {
    let mut cache = CapabilityCache::default();
    let mut e = make_entry();
    e.max_ok_input = Some(25_602);
    e.accounting_version = 0;
    cache.insert(mk("m"), e);
    assert!(migrate_accounting(&mut cache), "first pass migrates");
    let snapshot = serde_json::to_string(&cache).unwrap();
    assert!(!migrate_accounting(&mut cache), "second pass is a no-op");
    assert_eq!(serde_json::to_string(&cache).unwrap(), snapshot);
}

// --- invalidate_suspect_pins (#1967/#1968 remediation) ---

/// Replays the live incident's exact numbers: `max_ok_input` 205,189
/// pinned at High confidence against `safe_context` 209,715 (97.8% —
/// inside the 95% suspect zone), `hard_context_window: None` (this
/// entry was never cw-400-confirmed — it came from the ungated
/// turn-level ratchet #1967 fixes). Must be invalidated.
#[test]
fn invalidate_suspect_pins_drops_the_1967_incident_pin() {
    let mut cache = CapabilityCache::default();
    cache.insert(
        mk("nemotron-3-ultra"),
        CapabilityEntry {
            conformance: ToolConformance::Native,
            tested_date: "2026-08-29".into(),
            context_window: Some(262_144),
            hard_context_window: None,
            safe_context: Some(209_715),
            max_ok_input: Some(205_189),
            consecutive_ok: 0,
            tune_confidence: TuneConfidence::High,
            tune_date: Some("2026-08-29".into()),
            ..Default::default()
        },
    );
    assert!(invalidate_suspect_pins(&mut cache), "must report dirty");
    let e = &cache[&mk("nemotron-3-ultra")];
    assert_eq!(e.max_ok_input, None, "the poisoned pin must be dropped");
    assert_eq!(e.consecutive_ok, 0);
    assert_eq!(e.tune_confidence, TuneConfidence::None);
    // Non-ratchet state survives — this is a targeted invalidation, not
    // a reset of the whole entry.
    assert_eq!(e.context_window, Some(262_144));
    assert_eq!(e.safe_context, Some(209_715));
}

/// Anti-false-positive twin: a LEGITIMATE cw-400-derived pin sits at or
/// above its own `safe_context` by construction
/// (`record_context_window_400_with_pct`), and that path is the ONE
/// writer of `hard_context_window` — its presence must exempt the
/// entry, or this remediation would regress a working safety mechanism
/// instead of fixing a broken one.
#[test]
fn invalidate_suspect_pins_spares_a_legitimate_cw_400_pin() {
    let mut cache = CapabilityCache::default();
    cache.insert(
        mk("hosted-model"),
        CapabilityEntry {
            conformance: ToolConformance::Native,
            tested_date: "2026-06-09".into(),
            hard_context_window: Some(1_000_000),
            safe_context: Some(64_000),
            max_ok_input: Some(800_000), // cw-400 discovery: legit > safe_context
            consecutive_ok: 2,
            tune_confidence: TuneConfidence::Medium,
            ..Default::default()
        },
    );
    assert!(
        !invalidate_suspect_pins(&mut cache),
        "a hard_context_window-confirmed pin must never be touched"
    );
    let e = &cache[&mk("hosted-model")];
    assert_eq!(e.max_ok_input, Some(800_000));
    assert_eq!(e.tune_confidence, TuneConfidence::Medium);
}

/// A genuinely healthy pin — well under its own `safe_context` — is
/// left alone regardless of confidence or `hard_context_window`.
#[test]
fn invalidate_suspect_pins_spares_a_healthy_pin() {
    let mut cache = CapabilityCache::default();
    let mut e = make_entry();
    e.max_ok_input = Some(4_136);
    e.tune_confidence = TuneConfidence::High;
    cache.insert(mk("healthy-model"), e);
    assert!(!invalidate_suspect_pins(&mut cache));
    assert_eq!(cache[&mk("healthy-model")].max_ok_input, Some(4_136));
}

/// Running the invalidation twice must be a no-op the second time.
#[test]
fn invalidate_suspect_pins_is_idempotent() {
    let mut cache = CapabilityCache::default();
    let mut e = make_entry();
    e.safe_context = Some(209_715);
    e.max_ok_input = Some(205_189);
    e.tune_confidence = TuneConfidence::High;
    cache.insert(mk("m"), e);
    assert!(
        invalidate_suspect_pins(&mut cache),
        "first pass invalidates"
    );
    let snapshot = serde_json::to_string(&cache).unwrap();
    assert!(
        !invalidate_suspect_pins(&mut cache),
        "second pass is a no-op"
    );
    assert_eq!(serde_json::to_string(&cache).unwrap(), snapshot);
}

// --- resolve_memory_budget (Step 18.2, #247) ---

/// Fixture capability cache with one tuned entry for "tuned-model".
fn fixture_entry(max_ok_input: Option<u32>, safe_context: Option<u32>) -> CapabilityEntry {
    CapabilityEntry {
        conformance: ToolConformance::Native,
        tested_date: "2026-06-10".into(),
        context_window: Some(32_768),
        safe_context,
        max_ok_input,
        ..Default::default()
    }
}

/// Tier 1: an explicit `[memory] context_tokens` is a deliberate user
/// override — it wins even when capability data exists.
#[test]
fn resolve_memory_budget_explicit_config_wins() {
    let entry = fixture_entry(Some(24_000), Some(26_214));
    assert_eq!(
        resolve_memory_budget(Some(16_000), Some(1_000_000), Some(&entry)),
        16_000
    );
}

#[test]
fn resolve_memory_budget_uses_the_active_models_declared_window() {
    let entry = fixture_entry(Some(24_000), None);
    assert_eq!(
        resolve_memory_budget(None, Some(1_000_000), Some(&entry)),
        1_000_000
    );
    // A newly selected model with its own declared window and no capability
    // entry yet: the declaration is the budget (the prior model's is gone).
    assert_eq!(
        resolve_memory_budget(None, Some(131_072), None),
        131_072,
        "the selected model's declaration replaces the prior model's budget"
    );
}

/// Tier 3a: without an override or declaration, the capability-derived
/// budget is `max(max_ok_input, safe_context)` (Phase 20 §2.1) — here the
/// proven figure exceeds the claim-derived one and wins.
#[test]
fn resolve_memory_budget_capability_max_ok_input_second() {
    let entry = fixture_entry(Some(24_000), Some(6_553));
    assert_eq!(resolve_memory_budget(None, None, Some(&entry)), 24_000);
}

/// Phase 20 §2.1: the high-water mark is a floor of proven-good, not a
/// ceiling — when it sits BELOW the believed-safe window, `max()` keeps
/// the budget at the window instead of shrinking it to the largest
/// prompt merely seen so far (the motivating 6,068-vs-8,734 failure).
#[test]
fn resolve_memory_budget_max_keeps_safe_context_over_low_hwm() {
    let entry = fixture_entry(Some(6_068), Some(26_214));
    assert_eq!(resolve_memory_budget(None, None, Some(&entry)), 26_214);
}

/// Tier 3b: with no `max_ok_input` yet (e.g. freshly de-poisoned by the
/// 18.1 migration), `safe_context` is the capability-derived budget.
#[test]
fn resolve_memory_budget_falls_back_to_safe_context() {
    let entry = fixture_entry(None, Some(6_553));
    assert_eq!(resolve_memory_budget(None, None, Some(&entry)), 6_553);
    // And the mirror: max_ok_input alone serves when safe_context is
    // absent (hosted endpoints discovered via cw-400 have no num_ctx).
    let entry = fixture_entry(Some(24_000), None);
    assert_eq!(resolve_memory_budget(None, None, Some(&entry)), 24_000);
}

/// Tier 4: the static default applies ONLY when no override, no declared
/// window, and no empirical tuning exists — a cache MISS (fresh model), or
/// an entry present but untuned. Cross-principal leakage is now impossible
/// by construction: the caller resolves the entry via `cap_key`, so this
/// function can never look up the wrong entry (see the cap_key keying
/// tests for the instance-isolation guarantee).
#[test]
fn resolve_memory_budget_static_default_last() {
    // Cache miss (fresh model / unknown principal → the caller passes None).
    assert_eq!(
        resolve_memory_budget(None, None, None),
        newt_core::DEFAULT_CONTEXT_TOKENS
    );
    // Entry present (declared window known) but no empirical tuning.
    let untuned = fixture_entry(None, None);
    assert_eq!(
        resolve_memory_budget(None, None, Some(&untuned)),
        newt_core::DEFAULT_CONTEXT_TOKENS
    );
}

/// Regression for the pre-18.2 parallel default: the TUI built providers
/// with `context_tokens.unwrap_or(8_192)`, silently ignoring probe data.
/// A session with capability data must NOT resolve to the static
/// default. (Phase 20 §2.1 updated the expected figure: the budget is
/// now `max(max_ok_input, safe_context)` = 26,214, not the HWM alone.)
#[test]
fn resolve_memory_budget_never_ignores_probe_data() {
    let entry = fixture_entry(Some(24_000), Some(26_214));
    let budget = resolve_memory_budget(None, None, Some(&entry));
    assert_ne!(
        budget,
        newt_core::DEFAULT_CONTEXT_TOKENS,
        "capability data present — the static default must not win"
    );
    assert_eq!(budget, 26_214);
}

// -----------------------------------------------------------------------
// Step 20.2 active discovery (docs/design/model-self-tuning.md §4)
// -----------------------------------------------------------------------

// --- refresh_context_window (§4.2) ---
//
// refresh_context_window's HTTP path (always-fetches, re-bootstrap only
// when safe_context is unset, never auto-raises) needs a Tokio reactor
// for the `/api/show` call, so it is covered end-to-end in
// `tests/probe_integration.rs` — mirroring how `ensure_context_window`
// is tested there rather than in this `#[cfg(test)]` block.

// --- message_thinking_fields (§4.3) truth table ---

#[test]
fn message_thinking_fields_truth_table() {
    // Empty content + non-empty thinking → true.
    assert!(message_thinking_fields(&serde_json::json!({
        "content": "", "thinking": "reasoning here"
    })));
    // Whitespace content + non-empty reasoning → true.
    assert!(message_thinking_fields(&serde_json::json!({
        "content": "   \n", "reasoning": "x"
    })));
    // reasoning_content variant → true.
    assert!(message_thinking_fields(&serde_json::json!({
        "content": "", "reasoning_content": "y"
    })));
    // Missing content key (treated as empty) + thinking → true.
    assert!(message_thinking_fields(&serde_json::json!({
        "thinking": "z"
    })));
    // Non-empty content → false even with a thinking field.
    assert!(!message_thinking_fields(&serde_json::json!({
        "content": "ok", "thinking": "z"
    })));
    // Empty content but no/empty thinking fields → false.
    assert!(!message_thinking_fields(&serde_json::json!({
        "content": "", "thinking": "  "
    })));
    assert!(!message_thinking_fields(
        &serde_json::json!({"content": ""})
    ));
}

// --- build_padded_prompt (§4.5) sizing ---

#[test]
fn build_padded_prompt_sizes_near_target_across_ratios() {
    for &(target, ratio) in &[
        (512u32, 1.0f32),
        (2_048, 1.0),
        (8_000, 1.3),
        (4_096, 0.8),
        (16_000, 2.5),
    ] {
        let s = build_padded_prompt(target, ratio);
        let est = s.chars().count() as f32 / 4.0;
        let predicted_real = est * sanitize_ratio(ratio);
        let rel = (predicted_real - target as f32).abs() / target as f32;
        assert!(
            rel <= 0.10,
            "target={target} ratio={ratio}: chars/4*ratio={predicted_real} ({:.1}% off)",
            rel * 100.0
        );
    }
}

#[test]
fn build_padded_prompt_sanitizes_bad_ratio_to_one() {
    // NaN / out-of-band ratios fall back to 1.0 (same as estimate-space).
    let s = build_padded_prompt(4_000, f32::NAN);
    let est = s.chars().count() as f32 / 4.0;
    assert!((est - 4_000.0).abs() / 4_000.0 <= 0.10);
    let s2 = build_padded_prompt(4_000, 99.0);
    let est2 = s2.chars().count() as f32 / 4.0;
    assert!((est2 - 4_000.0).abs() / 4_000.0 <= 0.10);
}

// --- classify_boundary_probe (§4.5) every arm ---

#[test]
fn classify_boundary_probe_accepted_when_eval_meets_threshold() {
    let json = serde_json::json!({
        "message": {"content": "ok"},
        "prompt_eval_count": 9_500,
        "eval_count": 3
    });
    // sent 10_000; 9_500 ≥ 90% → Accepted carrying the observed count.
    assert_eq!(
        classify_boundary_probe(Ok(&json), 10_000),
        BoundaryClass::Accepted {
            prompt_tokens: 9_500
        }
    );
}

#[test]
fn classify_boundary_probe_accepted_via_tool_call_or_eval_count() {
    // Empty content but a tool call is still usable.
    let tc = serde_json::json!({
        "message": {"content": "", "tool_calls": [{"function": {"name": "x"}}]},
        "prompt_eval_count": 9_900
    });
    assert!(matches!(
        classify_boundary_probe(Ok(&tc), 10_000),
        BoundaryClass::Accepted { .. }
    ));
    // Empty content, no tool call, but eval_count > 0 → usable.
    let ec = serde_json::json!({
        "message": {"content": ""},
        "prompt_eval_count": 9_900,
        "eval_count": 5
    });
    assert!(matches!(
        classify_boundary_probe(Ok(&ec), 10_000),
        BoundaryClass::Accepted { .. }
    ));
}

#[test]
fn classify_boundary_probe_truncated_when_eval_below_threshold() {
    // 200 but only 4_000 of 10_000 evaluated (head dropped) → Truncated.
    let json = serde_json::json!({
        "message": {"content": "ok"},
        "prompt_eval_count": 4_000,
        "eval_count": 2
    });
    assert_eq!(
        classify_boundary_probe(Ok(&json), 10_000),
        BoundaryClass::Truncated
    );
    // 200 with a usable body but no prompt_eval_count at all → Truncated
    // (we cannot confirm the prompt was evaluated).
    let no_count = serde_json::json!({"message": {"content": "ok"}, "eval_count": 1});
    assert_eq!(
        classify_boundary_probe(Ok(&no_count), 10_000),
        BoundaryClass::Truncated
    );
}

#[test]
fn classify_boundary_probe_ctx_window_400() {
    let err = anyhow::anyhow!(
        "inference endpoint 400: litellm.ContextWindowExceededError: \
         prompt is too long: 12000 tokens > 8192 maximum"
    );
    assert_eq!(
        classify_boundary_probe(Err(&err), 12_000),
        BoundaryClass::CtxWindow400 { limit: 8_192 }
    );
}

#[test]
fn classify_boundary_probe_inconclusive_for_other_errors() {
    let err = anyhow::anyhow!("request failed: connection reset");
    assert_eq!(
        classify_boundary_probe(Err(&err), 10_000),
        BoundaryClass::Inconclusive
    );
}

// --- is_tuning_stale (§4.6) boundaries ---

#[test]
fn is_tuning_stale_boundaries() {
    // None → stale.
    assert!(is_tuning_stale(None, "2026-06-13", 30));
    // Exactly max_age days old → NOT stale (strictly greater is stale).
    assert!(!is_tuning_stale(Some("2026-05-14"), "2026-06-13", 30));
    // One day over max_age → stale.
    assert!(is_tuning_stale(Some("2026-05-13"), "2026-06-13", 30));
    // Same day → fresh.
    assert!(!is_tuning_stale(Some("2026-06-13"), "2026-06-13", 30));
    // Unparseable stored date → treat as stale.
    assert!(is_tuning_stale(Some("not-a-date"), "2026-06-13", 30));
    // Unparseable `today` is also stale (defensive).
    assert!(is_tuning_stale(Some("2026-06-13"), "garbage", 30));
}

// --- sanitize_ratio ---

#[test]
fn sanitize_ratio_clamps_and_defaults() {
    assert_eq!(sanitize_ratio(1.3), 1.3);
    assert_eq!(sanitize_ratio(0.5), 0.5);
    assert_eq!(sanitize_ratio(3.0), 3.0);
    assert_eq!(sanitize_ratio(0.4), 1.0); // below band → default
    assert_eq!(sanitize_ratio(3.1), 1.0); // above band → default
    assert_eq!(sanitize_ratio(f32::NAN), 1.0);
    assert_eq!(sanitize_ratio(f32::INFINITY), 1.0);
}

// --- parse_show_response ---

#[test]
fn parse_show_response_reads_llama_key() {
    let json = serde_json::json!({"model_info": {"llama.context_length": 32768}});
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        Some(32768)
    );
}

#[test]
fn parse_show_response_reads_nemotron_key() {
    let json = serde_json::json!({"model_info": {"nemotron_h_omni.context_length": 131072}});
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        Some(131072)
    );
}

#[test]
fn parse_show_response_bare_context_length_key() {
    let json = serde_json::json!({"model_info": {"context_length": 8192}});
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        Some(8192)
    );
}

#[test]
fn parse_show_response_modelfile_num_ctx_wins_when_smaller() {
    let json = serde_json::json!({
        "model_info": {"llama.context_length": 131072},
        "parameters": "num_ctx 32768\ntemperature 0.7"
    });
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        Some(32768)
    );
}

#[test]
fn parse_show_response_arch_wins_when_num_ctx_larger() {
    let json = serde_json::json!({
        "model_info": {"llama.context_length": 4096},
        "parameters": "num_ctx 32768"
    });
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        Some(4096)
    );
}

#[test]
fn parse_show_response_returns_none_when_no_keys() {
    let json = serde_json::json!({"model_info": {"general.architecture": "llama"}});
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        None
    );
}

#[test]
fn parse_show_response_uses_minimum_when_multiple_arch_keys() {
    let json = serde_json::json!({
        "model_info": {
            "llama.context_length": 131072,
            "gemma.context_length": 8192
        }
    });
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        Some(8192)
    );
}

#[test]
fn parse_show_response_modelfile_only_no_model_info() {
    // No model_info at all — the Modelfile num_ctx line is the only source.
    let json = serde_json::json!({
        "parameters": "stop \"<|end|>\"\nnum_ctx 16384\ntemperature 0.2"
    });
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        Some(16384)
    );
}

#[test]
fn parse_show_response_ignores_unparsable_num_ctx() {
    // num_ctx value that isn't a u32 must be skipped, not panic.
    let json = serde_json::json!({"parameters": "num_ctx lots"});
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        None
    );
}

#[test]
fn parse_show_response_parameters_without_num_ctx() {
    let json = serde_json::json!({"parameters": "temperature 0.7\ntop_p 0.9"});
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        None
    );
}

#[test]
fn parse_show_response_non_numeric_context_length_ignored() {
    // A context_length that isn't a u64 (e.g. a string) must not match.
    let json = serde_json::json!({"model_info": {"llama.context_length": "32768"}});
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&json),
        None
    );
}

#[test]
fn parse_show_response_empty_json() {
    assert_eq!(
        newt_core::backend_probe::parse_ollama_show_window(&serde_json::json!({})),
        None
    );
}

// --- probe_tool_schema ---

#[test]
fn probe_tool_schema_is_single_list_dir_function() {
    let schema = super::probe_tool_schema();
    let arr = schema.as_array().expect("schema is a JSON array");
    assert_eq!(arr.len(), 1, "probe uses exactly one tool");
    let f = &arr[0];
    assert_eq!(f["type"], "function");
    assert_eq!(f["function"]["name"], "list_dir");
    // The probe prompt tells the model to pass `path` — the schema must
    // declare it as a required string parameter or the probe is invalid.
    let params = &f["function"]["parameters"];
    assert_eq!(params["properties"]["path"]["type"], "string");
    assert_eq!(params["required"][0], "path");
}

// --- defaults ---

#[test]
fn capability_entry_default_is_untested_no_tools() {
    let e = CapabilityEntry::default();
    assert_eq!(e.conformance, ToolConformance::NoTools);
    assert!(e.tested_date.is_empty());
    assert_eq!(e.context_window, None);
    assert_eq!(e.safe_context, None);
    assert_eq!(e.overflow_at, None);
    assert_eq!(e.max_ok_input, None);
    assert_eq!(e.consecutive_ok, 0);
    assert_eq!(e.tune_confidence, TuneConfidence::None);
    assert_eq!(e.tune_date, None);
}

#[test]
fn tune_confidence_default_is_none() {
    assert_eq!(TuneConfidence::default(), TuneConfidence::None);
}

// --- the capability table ---
//
// D3e (#1918) split the rendering out of `print_capabilities_table`, so
// these no longer have to settle for "did not panic". The goldens below
// record what an operator sees TODAY; the migration onto `markup::table`
// amends them and declares its diff.

/// Two models, one active, one untested — the ordinary case, byte for
/// byte, AFTER the migration onto `markup::table`.
///
/// The pre-migration bytes are in the previous commit; this is the
/// declared diff. Columns now fit their content, alignment is carried by
/// the delimiter row rather than by spaces inside each cell, and the
/// active marker is part of the Model cell instead of a second field
/// padded inside the name's width.
#[test]
fn the_capability_table_renders_its_current_bytes() {
    let mut cache = CapabilityCache::default();
    let mut e = make_entry();
    e.tune_confidence = TuneConfidence::High;
    e.tested_date = "2026-06-10".to_string();
    cache.insert(mk("llama3.1:8b"), e);
    let models: Vec<ModelInfo> = [("llama3.1:8b", "8B"), ("qwen2.5-coder:7b", "7B")]
        .into_iter()
        .map(|(n, s)| ModelInfo {
            name: n.to_string(),
            param_size: s.to_string(),
        })
        .collect();

    let t = capabilities_table(&models, &cache, "llama3.1:8b");
    assert_eq!(t.active, Some(0), "the active row is identified by index");
    assert_eq!(
        t.head[0],
        "| Model            | Size | Tool Use | Think | Ctx Win | Safe Ctx | Conf | Tested     |"
    );
    assert_eq!(
        t.head[1],
        "| ---------------- | ---: | -------- | ----- | ------: | -------: | ---: | ---------- |"
    );
    assert_eq!(t.rows.len(), 2);
    assert_eq!(
        t.rows[0],
        "| llama3.1:8b \u{25c0}    |   8B | \u{2713} native | \u{2014}     |     32k |      25k | High | 2026-06-10 |"
    );
    assert_eq!(
        t.rows[1],
        "| qwen2.5-coder:7b |   7B | \u{2014}        | \u{2014}     |       \u{2014} |        \u{2014} |    \u{2014} | (untested) |"
    );
    // The Model column now fits its content. It used to be padded to at
    // least 20 for an 11-character name, because the active marker was a
    // separate field living inside that width.
    assert!(
        !t.rows.iter().any(|r| r.contains("           ")),
        "no run of hand-laid padding survives: {:?}",
        t.rows
    );
}

/// **The bug, fixed — asserted with the SAME measurement that recorded
/// it** (A0 §4.2.13, "the wrong metric twice over" — really three).
///
/// `name_w` is computed from `m.name.len()`, which is BYTES. The padding
/// `{:<name_w$}` counts CHARS. The terminal renders CELLS. A CJK model
/// name therefore sizes the column by its UTF-8 length, gets padded by
/// its character count, and occupies twice its character count on screen
/// — so every later column on that row lands somewhere else than on
/// every other row.
///
/// The previous commit asserted the misalignment; this one asserts its
/// absence, by measuring display cells exactly as before. Turning the
/// assertion round rather than deleting it is what makes the fix legible:
/// the same six-cell discrepancy that was `assert_ne!` is now zero. It is
/// the same A0 entry D3c fixed the other half of, in `mcp_cmd`.
#[test]
fn a_cjk_model_name_no_longer_misaligns_any_column() {
    let cache = CapabilityCache::default();
    let models: Vec<ModelInfo> = [("日本語モデル:7b", "7B"), ("ascii-model:7b", "7B")]
        .into_iter()
        .map(|(n, s)| ModelInfo {
            name: n.to_string(),
            param_size: s.to_string(),
        })
        .collect();
    let t = capabilities_table(&models, &cache, "none");

    // Unchanged facts about the name: 21 bytes, 9 chars, 15 cells. What
    // changed is which of the three the renderer uses.
    assert_eq!("日本語モデル:7b".len(), 21);
    assert_eq!("日本語モデル:7b".chars().count(), 9);

    let cells = |s: &str| -> usize {
        s.chars()
            .map(|c| {
                if ('\u{1100}'..='\u{115f}').contains(&c)
                    || ('\u{2e80}'..='\u{a4cf}').contains(&c)
                    || ('\u{ac00}'..='\u{d7a3}').contains(&c)
                    || ('\u{f900}'..='\u{faff}').contains(&c)
                    || ('\u{ff00}'..='\u{ff60}').contains(&c)
                {
                    2
                } else {
                    1
                }
            })
            .sum()
    };
    assert_eq!(
        cells(&t.rows[0]),
        cells(&t.rows[1]),
        "the two rows line up on a terminal — they differed by 6 cells before"
    );
    // The header and delimiter line up with them too, which is what
    // sizing by cells rather than by bytes actually buys.
    assert_eq!(cells(&t.head[0]), cells(&t.rows[0]));
    assert_eq!(cells(&t.head[1]), cells(&t.rows[0]));
}

#[test]
fn print_capabilities_table_handles_empty_model_list() {
    let cache = CapabilityCache::default();
    print_capabilities_table(&[], &cache, "none", "http://localhost:11434", false);
}

#[test]
fn print_capabilities_table_renders_all_branches() {
    let mut cache = CapabilityCache::default();
    // Fully-populated entry at each confidence level.
    for (name, conf) in [
        ("m-none", TuneConfidence::None),
        ("m-low", TuneConfidence::Low),
        ("m-med", TuneConfidence::Medium),
        ("m-high", TuneConfidence::High),
    ] {
        let mut e = make_entry();
        e.tune_confidence = conf;
        cache.insert(mk(name), e);
    }
    // Tested entry with no ctx data (the `—` placeholders).
    cache.insert(
        mk("m-noctx"),
        CapabilityEntry {
            conformance: ToolConformance::TextMode,
            tested_date: "2026-06-06".to_string(),
            ..Default::default()
        },
    );
    // A reasoning model: emits_thinking → the ✓ in the Think column.
    cache.insert(
        mk("m-think"),
        CapabilityEntry {
            conformance: ToolConformance::Native,
            tested_date: "2026-06-10".to_string(),
            emits_thinking: Some(true),
            ..Default::default()
        },
    );
    let models: Vec<ModelInfo> = [
        ("m-none", "7B"),
        ("m-low", "13B"),
        ("m-med", ""),
        ("m-high", "32.8B"),
        ("m-noctx", "3B"),
        ("m-think", "30B"),
        ("m-untested", "1B"),
    ]
    .into_iter()
    .map(|(n, s)| ModelInfo {
        name: n.to_string(),
        param_size: s.to_string(),
    })
    .collect();
    // Plain path, with an active row.
    print_capabilities_table(&models, &cache, "m-low", "http://localhost:11434", false);
    // Colour path for the active row (execute! to stdout).
    print_capabilities_table(&models, &cache, "m-high", "http://localhost:11434", true);
    // Active model not in list — no row gets the active tag.
    print_capabilities_table(&models, &cache, "absent", "http://localhost:11434", true);
}
