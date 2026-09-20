// Suite #2449: producer identity must cover the exact emitted policy value.
// These tests initially run against the real v2 emitter, before its transition.
use content_addressable::{canonical, ContentId};

// Shared fixture shorthand: production remains fallible, and tests of emitted
// records fail immediately if the real encoder refuses their policy value.
fn contract_record(input: &ContractInputs<'_>) -> serde_json::Value {
    super::contract_record(input).expect("fixture policy encodes as contract v3")
}

fn asserted_config_id(record: &serde_json::Value) -> ContentId {
    record["config_digest"]
        .as_str()
        .expect("v3 requires an emitted config identity")
        .parse()
        .expect("the identity uses the structured ContentId profile")
}

/// The existing producer advertises v2 and cannot qualify the changed psyche semantics.
#[test]
fn producer_v3_declares_the_new_semantic_boundary() {
    assert_eq!(contract_record(&inputs())["contract_version"], "3");
}

/// Package semver cannot distinguish two different source builds of the same release.
#[test]
fn producer_v3_preserves_the_existing_opaque_build_identity() {
    assert_eq!(
        contract_record(&inputs())["agent_version"],
        newt_core::build_info::VERSION_WITH_COMMIT
    );
}

/// Independently decode the emitted JSON before recomputing the complete value identity.
#[test]
fn producer_v3_digest_covers_the_exact_emitted_config() {
    let manifest = serde_json::json!({
        "invocation_mode": "fresh",
        "starting_cid": null,
        "configuration": {"future_optional_knob": {"order": [3, 1], "enabled": true}}
    });
    let mut input = inputs();
    input.smart_harness = Some(&manifest);
    let record = contract_record(&input);
    let decoded: serde_json::Value = serde_json::from_str(&record.to_string()).unwrap();
    let expected = ContentId::from_canonical_bytes(
        &canonical::to_canonical_dagcbor(&decoded["effective_config"]).unwrap(),
    );
    assert_eq!(asserted_config_id(&record), expected);
    let mut changed = decoded["effective_config"].clone();
    changed["smart_harness"]["configuration"]["future_optional_knob"]["order"] =
        serde_json::json!([1, 3]);
    let tampered =
        ContentId::from_canonical_bytes(&canonical::to_canonical_dagcbor(&changed).unwrap());
    assert_ne!(
        expected, tampered,
        "unknown fields and array order stay covered"
    );
}

/// Captured verification is configuration as well as a byte-compatible legacy receipt.
#[test]
fn producer_v3_uses_the_instantiated_verification_policy() {
    let policy = serde_json::json!({
        "mode": "result_aware", "repair_allowance": 3, "required_by_tenacity": true
    });
    let mut input = inputs();
    input.verification = Some(policy.clone());
    let record = contract_record(&input);
    assert_eq!(record["receipt"]["verification"], policy);
    assert_eq!(record["effective_config"]["verification"], policy);
}

/// Outcome and timing describe a run; only changed policy should change configuration identity.
#[test]
fn producer_v3_identity_ignores_outcome_timing_and_build_envelope() {
    let first = contract_record(&inputs());
    let mut input = inputs();
    input.outcome = "model_error";
    input.wall_ms = 42;
    input.gen_tokens = Some(9);
    let mut second = contract_record(&input);
    second["agent_version"] = serde_json::json!("different opaque build");
    assert_eq!(asserted_config_id(&first), asserted_config_id(&second));
    input.max_rounds += 1;
    input.tool_round_limit.rounds += 1;
    assert_ne!(
        asserted_config_id(&first),
        asserted_config_id(&contract_record(&input))
    );
}

/// V3 identities describe the complete JSON value, retaining absent/null and
/// launch input differences while ignoring object insertion/formatting order.
#[test]
fn policy_identity_preserves_starting_cid_unknown_values_and_absence() {
    let mut launch = serde_json::json!({
        "invocation_mode": "resume", "starting_cid": "first-frame",
        "configuration": {"future_knob": {"steps": [2, 1]}}
    });
    let make = |manifest: &serde_json::Value| {
        let mut input = inputs();
        input.smart_harness = Some(manifest);
        contract_record(&input)
    };
    let first = make(&launch);
    let reordered: serde_json::Value = serde_json::from_str(
        r#"{ "configuration": { "future_knob": { "steps": [2,1] } },
             "starting_cid": "first-frame", "invocation_mode": "resume" }"#,
    )
    .unwrap();
    assert_eq!(
        asserted_config_id(&first),
        asserted_config_id(&make(&reordered))
    );
    launch["starting_cid"] = serde_json::json!("different-frame");
    assert_ne!(
        asserted_config_id(&first),
        asserted_config_id(&make(&launch))
    );
    let absent = make(&launch);
    launch["configuration"]["future_knob"]["optional"] = serde_json::Value::Null;
    assert_ne!(
        asserted_config_id(&absent),
        asserted_config_id(&make(&launch))
    );
}

/// A missing outcome cannot be upgraded into evidence of instantiated policy.
#[test]
fn policy_identity_without_outcome_omits_uninstantiated_evidence() {
    let mut input = inputs();
    input.features = None;
    input.verification = Some(serde_json::json!({"mode":"result_aware"}));
    let record = contract_record(&input);
    assert!(record.get("receipt").is_none());
    for key in [
        "verification",
        "semantic_cognition",
        "responses_capability",
        "reasoning_effort",
        "output_allowance",
        "initiative_read_only_rounds",
    ] {
        assert!(
            record["effective_config"].get(key).is_none(),
            "{key} requires captured evidence"
        );
    }
    // Configured host inputs still have a valid identity; this is not a claim
    // that a worker or optional technique executed.
    asserted_config_id(&record);
}

/// Every emitted captured parameter participates without a separate key list.
#[test]
fn policy_identity_changes_with_captured_numeric_and_wire_parameters() {
    let mut input = inputs();
    input.initiative_read_only_rounds = Some(7);
    let first = asserted_config_id(&contract_record(&input));
    input.initiative_read_only_rounds = Some(8);
    assert_ne!(first, asserted_config_id(&contract_record(&input)));
    input.initiative_read_only_rounds = Some(7);
    input.wire_api = "ollama";
    assert_ne!(first, asserted_config_id(&contract_record(&input)));
    input.wire_api = "chat_completions";
    input.tool_round_limit.source = newt_core::tenacity::ToolRoundLimitSource::Override;
    assert_ne!(
        first,
        asserted_config_id(&contract_record(&input)),
        "equal caps with different derivations remain distinguishable"
    );
}
