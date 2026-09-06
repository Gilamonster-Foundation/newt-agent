use super::*;

// Context managers, triggers, features, and backend-dependent defaults.

#[test]
fn context_manager_keyword_roundtrip_and_availability() {
    for m in [
        ContextManager::Standard,
        ContextManager::AppendOnly,
        ContextManager::Progressive,
        ContextManager::Distributed,
    ] {
        assert_eq!(ContextManager::from_keyword(m.keyword()), Some(m));
    }
    assert_eq!(
        ContextManager::from_keyword("  STANDARD "),
        Some(ContextManager::Standard),
        "case/space-insensitive"
    );
    assert_eq!(ContextManager::from_keyword("nope"), None);
    // standard + append-only are implemented; the card managers are #546.
    assert!(ContextManager::Standard.available());
    assert!(ContextManager::AppendOnly.available());
    assert!(!ContextManager::Progressive.available());
    assert!(!ContextManager::Distributed.available());
    assert_eq!(ContextManager::default(), ContextManager::Standard);
    // The predicate the whole preset exists to express.
    assert!(!ContextManager::AppendOnly.rewrites_history());
    for m in [
        ContextManager::Standard,
        ContextManager::Progressive,
        ContextManager::Distributed,
    ] {
        assert!(m.rewrites_history(), "{m:?} rewrites history");
    }
    // Every spelling `from_keyword` advertises must actually parse.
    for alias in [
        "append-only",
        "append_only",
        "appendonly",
        "append",
        "  APPEND-ONLY ",
    ] {
        assert_eq!(
            ContextManager::from_keyword(alias),
            Some(ContextManager::AppendOnly),
            "alias {alias:?}"
        );
    }
}

/// The keyword an operator is SHOWN must be the keyword their config file
/// accepts. `#[serde(rename_all = "lowercase")]` would have named this
/// variant "appendonly" while every other surface said "append-only", so
/// `manager = "append-only"` — the spelling `/context manager` echoes back
/// and the docs use — failed the WHOLE config load, and the interactive path
/// swallows that into a silent fall back to defaults.
///
/// Regression for the `lowercase` → `kebab-case` fix: this fails on the old
/// attribute. `keyword()` alone could never catch it — it never touches serde.
#[test]
fn context_manager_serde_name_matches_its_keyword() {
    for m in [
        ContextManager::Standard,
        ContextManager::AppendOnly,
        ContextManager::Progressive,
        ContextManager::Distributed,
    ] {
        let encoded = serde_json::to_string(&m).expect("serialize");
        assert_eq!(
            encoded,
            format!("\"{}\"", m.keyword()),
            "{m:?} serializes as something other than its keyword"
        );
        assert_eq!(
            serde_json::from_str::<ContextManager>(&encoded).expect("round-trip"),
            m
        );
    }
    // The documented spelling, through the real config surface.
    let parsed: ContextConfig =
        toml::from_str("manager = \"append-only\"").expect("append-only must load");
    assert_eq!(parsed.manager, ContextManager::AppendOnly);
}

#[test]
fn compaction_trigger_policy_keyword_roundtrip_and_default() {
    for policy in [
        CompactionTriggerPolicy::HeadroomAware,
        CompactionTriggerPolicy::MessageCount,
    ] {
        assert_eq!(
            CompactionTriggerPolicy::from_keyword(policy.as_str()),
            Some(policy)
        );
        assert_eq!(policy.keyword(), policy.as_str());
    }
    assert_eq!(
        CompactionTriggerPolicy::from_keyword("  MESSAGE_COUNT "),
        Some(CompactionTriggerPolicy::MessageCount),
        "case/space-insensitive"
    );
    assert_eq!(CompactionTriggerPolicy::from_keyword("nope"), None);
    assert_eq!(
        CompactionTriggerPolicy::default(),
        CompactionTriggerPolicy::HeadroomAware
    );
}

#[test]
fn context_section_defaults_and_parses() {
    // Absent [context] → None on Config; the resolver falls back to standard.
    let cfg: Config = toml::from_str("").unwrap();
    assert!(cfg.context.is_none());
    let c: ContextConfig =
        toml::from_str("manager = \"progressive\"\ncompaction_trigger_policy = \"message_count\"")
            .unwrap();
    assert_eq!(c.manager, ContextManager::Progressive);
    assert_eq!(
        c.compaction_trigger_policy,
        CompactionTriggerPolicy::MessageCount
    );
    let defaults = ContextConfig::default();
    assert_eq!(defaults.manager, ContextManager::Standard);
    assert_eq!(
        defaults.compaction_trigger_policy,
        CompactionTriggerPolicy::HeadroomAware
    );
    // Omitting the key uses the serde default rather than requiring every
    // existing `[context]` configuration to opt in explicitly.
    let parsed_default: ContextConfig = toml::from_str("manager = \"standard\"").unwrap();
    assert_eq!(
        parsed_default.compaction_trigger_policy,
        CompactionTriggerPolicy::HeadroomAware
    );
    assert!(
        toml::from_str::<ContextConfig>("compaction_trigger_policy = \"not_a_policy\"").is_err(),
        "an invalid policy must fail config parsing rather than silently changing safety behavior"
    );
}

#[test]
fn context_input_ceiling_pct_normalizes_at_deserialization_boundary() {
    for value in [1, 80, 99] {
        let parsed: ContextConfig =
            toml::from_str(&format!("input_ceiling_pct = {value}")).unwrap();
        assert_eq!(parsed.input_ceiling_pct, value);
    }

    for value in [0, 100, 101, u32::MAX] {
        let parsed: ContextConfig =
            toml::from_str(&format!("input_ceiling_pct = {value}")).unwrap();
        assert_eq!(
            parsed.input_ceiling_pct,
            default_input_ceiling_pct(),
            "out-of-range value {value} must fall back to the documented safe default"
        );
    }

    assert_eq!(input_percentage_ceiling(32_768, 90), 29_491);
    assert_eq!(
        input_percentage_ceiling(32_768, 0),
        26_214,
        "programmatic callers share the same invalid-value fallback",
    );
}

#[test]
fn context_feature_keyword_alias_availability_and_issue() {
    // canonical keyword round-trips
    for f in ContextFeature::ALL {
        assert_eq!(ContextFeature::from_keyword(f.keyword()), Some(f));
    }
    // aliases + hyphen/underscore/case
    assert_eq!(
        ContextFeature::from_keyword("TOOL-OFFLOAD"),
        Some(ContextFeature::ToolOffload)
    );
    assert_eq!(
        ContextFeature::from_keyword("offload"),
        Some(ContextFeature::ToolOffload)
    );
    assert_eq!(
        ContextFeature::from_keyword(" state "),
        Some(ContextFeature::Scratchpad)
    );
    assert_eq!(ContextFeature::from_keyword("nope"), None);
    // tool_offload (26.3), scratchpad (26.4), semantic (26.5), experiential
    // (26.6a), scheduled (26.6b) shipped; only provenance is still pending.
    assert!(ContextFeature::ToolOffload.available());
    assert!(ContextFeature::Scratchpad.available());
    assert!(ContextFeature::Semantic.available());
    assert!(ContextFeature::Experiential.available());
    assert!(ContextFeature::Scheduled.available());
    assert!(
        !ContextFeature::Provenance.available(),
        "provenance still pending"
    );
    assert!(ContextFeature::ALL
        .iter()
        .filter(|f| !matches!(f, ContextFeature::Provenance))
        .all(|f| f.available()));
    // issues route to the right tracking ticket
    assert_eq!(ContextFeature::Semantic.issue(), 582);
    assert_eq!(ContextFeature::Scratchpad.issue(), 583);
    assert_eq!(ContextFeature::ToolOffload.issue(), 584);
    assert_eq!(ContextFeature::Provenance.issue(), 584);
    assert_eq!(ContextFeature::Experiential.issue(), 585);
    assert_eq!(ContextFeature::Scheduled.issue(), 586);
}

#[test]
fn context_features_override_layering_and_parse() {
    use ContextFeature as F;
    // Every preset resolves to all-off today (standard behavior).
    let base = ContextManager::Standard.base_features();
    assert!(base.enabled().is_empty());
    // An override layers on top of the base, leaving others untouched.
    let mut ov = ContextFeatures::default();
    ov.set(F::Scratchpad, Some(true));
    let resolved = ov.apply_to(base);
    assert!(resolved.get(F::Scratchpad));
    assert!(!resolved.get(F::Semantic));
    assert_eq!(resolved.enabled(), vec![F::Scratchpad]);
    // None override = inherit (no change); Some(false) = force off.
    let mut ov2 = ContextFeatures::default();
    ov2.set(F::Scratchpad, Some(false));
    assert!(!ov2.apply_to(resolved).get(F::Scratchpad));
    // [context.features] parses keyed by canonical keyword.
    let c: ContextConfig =
        toml::from_str("manager = \"standard\"\n[features]\nsemantic = true\nscratchpad = false")
            .unwrap();
    assert_eq!(c.features.get(F::Semantic), Some(true));
    assert_eq!(c.features.get(F::Scratchpad), Some(false));
    assert_eq!(c.features.get(F::ToolOffload), None);
}

#[test]
fn base_for_defaults_tool_offload_on_and_local_assist_on_for_ollama() {
    use ContextFeature as F;
    // #945: tool offload is local spill storage and defaults ON for every
    // backend. Step 27.4: local (Ollama) backends additionally default
    // scratchpad + scheduled ON; semantic also defaults ON but degrades to a
    // one-shot no-op until an embedder is configured.
    let local = ContextFeatureSet::base_for(ContextManager::Standard, BackendKind::Ollama);
    assert!(local.get(F::ToolOffload));
    assert!(local.get(F::Scratchpad));
    assert!(local.get(F::Semantic));
    assert!(local.get(F::Scheduled));
    // Cloud (OpenAI-compatible): per the user's context policy, every
    // available feature defaults ON except Provenance, regardless of
    // backend. Semantic degrades to a no-op until an embedder is set.
    let cloud = ContextFeatureSet::base_for(ContextManager::Standard, BackendKind::Openai);
    assert!(cloud.get(F::ToolOffload));
    assert!(cloud.get(F::Scratchpad));
    assert!(cloud.get(F::Semantic));
    assert!(cloud.get(F::Scheduled));
    // An explicit override still wins over the local default (force off).
    let mut ov = ContextFeatures::default();
    ov.set(F::Scheduled, Some(false));
    ov.set(F::ToolOffload, Some(false));
    assert!(!ov.apply_to(local).get(F::Scheduled));
    assert!(!ov.apply_to(local).get(F::ToolOffload));
    assert!(ov.apply_to(local).get(F::Scratchpad)); // untouched feature stays on
}
