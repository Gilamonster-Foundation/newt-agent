// Suite #2449: producer identity must cover the exact emitted policy value.
// These tests initially run against the real v2 emitter, before its transition.
use content_addressable::{canonical, ContentId};

// Shared fixture shorthand: production remains fallible, and tests of emitted
// records fail immediately if the real encoder refuses their policy value.
fn contract_record(input: &ContractInputs<'_>) -> serde_json::Value {
    super::contract_record(input).expect("fixture policy encodes as contract v3")
}

fn cid_of(value: &serde_json::Value) -> ContentId {
    ContentId::from_canonical_bytes(&canonical::to_canonical_dagcbor(value).unwrap())
}

/// The identity of `inputs()` with exactly one captured input edited.
fn id_with<'a>(edit: impl FnOnce(&mut ContractInputs<'a>)) -> ContentId {
    let mut input: ContractInputs<'a> = inputs();
    edit(&mut input);
    asserted_config_id(&contract_record(&input))
}

fn assert_pairwise_distinct(ids: &[ContentId]) {
    let distinct: std::collections::BTreeSet<_> = ids.iter().map(ToString::to_string).collect();
    assert_eq!(
        distinct.len(),
        ids.len(),
        "every variant is its own identity: {ids:?}"
    );
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
/// The build identity is envelope: it must be absent from the hashed value itself.
#[test]
fn producer_v3_identity_ignores_outcome_timing_and_build_envelope() {
    let first = contract_record(&inputs());
    let mut input = inputs();
    input.outcome = "model_error";
    input.wall_ms = 42;
    input.gen_tokens = Some(9);
    assert_eq!(
        asserted_config_id(&first),
        asserted_config_id(&contract_record(&input))
    );
    let config = &first["effective_config"];
    assert!(config.get("agent_version").is_none());
    assert!(
        !config
            .to_string()
            .contains(newt_core::build_info::VERSION_WITH_COMMIT),
        "the build identity must not be hashed into the configuration identity"
    );
    assert_eq!(
        asserted_config_id(&first),
        cid_of(config),
        "the identity is a function of effective_config alone"
    );
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

/// A missing outcome cannot be upgraded into evidence of instantiated policy,
/// whatever the caller passes: the builder itself refuses to claim it.
#[test]
fn policy_identity_without_outcome_omits_uninstantiated_evidence() {
    use newt_core::agentic::{Enforcement, OutputAllowance};
    use newt_core::model_card::{ReasoningEffort, ResponsesCapability};
    let capability = ResponsesCapability::default();
    let mut input = inputs();
    input.initiative_read_only_rounds = Some(7);
    input.semantic_cognition = Some(Some(newt_core::role_profile::Cognition::Meticulous));
    input.responses_capability = Some(&capability);
    input.reasoning_effort = Some(ReasoningEffort::High);
    input.output_allowance = Some(OutputAllowance {
        tokens: 16_000,
        enforced: Enforcement::Local,
    });
    input.verification = Some(serde_json::json!({"mode":"result_aware"}));
    let keys = [
        "verification",
        "semantic_cognition",
        "responses_capability",
        "reasoning_effort",
        "output_allowance",
        "initiative_read_only_rounds",
    ];
    // Control: with an outcome every one of these is claimed, so the absence
    // asserted below is the gate working, not an untouched fixture.
    let claimed = contract_record(&input);
    for key in keys {
        assert!(
            claimed["effective_config"].get(key).is_some(),
            "{key} is claimed once a turn outcome exists"
        );
    }
    input.features = None;
    let record = contract_record(&input);
    assert!(record.get("receipt").is_none());
    for key in keys {
        assert!(
            record["effective_config"].get(key).is_none(),
            "{key} requires captured evidence"
        );
    }
    // Configured host inputs still have a valid identity, and it is exactly the
    // identity of the run that never claimed the evidence.
    let mut bare = inputs();
    bare.features = None;
    assert_eq!(
        asserted_config_id(&record),
        asserted_config_id(&contract_record(&bare))
    );
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

/// Acceptance 1: the verification requirement alone moves the identity, and an
/// absent stanza stays distinct from an explicit `off`.
#[test]
fn policy_identity_changes_with_the_verification_requirement() {
    let receipts = [
        None,
        Some(serde_json::json!({"mode": "off"})),
        Some(serde_json::json!({"mode": "attempted"})),
        Some(serde_json::json!({"mode": "result_aware", "repair_allowance": 3})),
        Some(serde_json::json!({"mode": "result_aware", "repair_allowance": 2})),
        Some(serde_json::json!({
            "mode": "result_aware", "repair_allowance": 3, "required_by_tenacity": true
        })),
        Some(serde_json::json!({
            "mode": "result_aware", "repair_allowance": 3, "required_by_tenacity": false
        })),
    ];
    let ids: Vec<_> = receipts
        .into_iter()
        .map(|receipt| id_with(|input| input.verification = receipt))
        .collect();
    assert_pairwise_distinct(&ids);
}

/// The output and run allowances are numeric policy: each alone moves the identity.
#[test]
fn policy_identity_changes_with_output_and_run_allowances() {
    use newt_core::agentic::{Enforcement, OutputAllowance};
    let allowance = |tokens, enforced| Some(OutputAllowance { tokens, enforced });
    let output = [
        None,
        allowance(8_000, Enforcement::Server),
        allowance(16_000, Enforcement::Server),
        allowance(16_000, Enforcement::Local),
    ]
    .map(|value| id_with(|input| input.output_allowance = value));
    assert_pairwise_distinct(&output);
    let run = [None, Some(1), Some(2)].map(|value| id_with(|input| input.run_allowance = value));
    assert_pairwise_distinct(&run);
}

/// An accepted Responses wire control moves the identity: the declaration and
/// the effort actually sent, each alone.
#[test]
fn policy_identity_changes_with_accepted_responses_controls() {
    use newt_core::model_card::{ReasoningEffort as E, ReasoningEffortLadder, ResponsesCapability};
    let declared = |efforts: Vec<E>| ResponsesCapability {
        reasoning_effort: Some(ReasoningEffortLadder::try_from(efforts).unwrap()),
    };
    let (undeclared, high_only, medium_high) = (
        ResponsesCapability::default(),
        declared(vec![E::High]),
        declared(vec![E::Medium, E::High]),
    );
    let by_declaration = [
        Some(&undeclared),
        Some(&high_only),
        Some(&medium_high),
        None,
    ]
    .map(|capability| {
        id_with(|input| {
            input.responses_capability = capability;
            input.reasoning_effort = Some(E::High);
        })
    });
    assert_pairwise_distinct(&by_declaration);
    let by_effort = [None, Some(E::Low), Some(E::High)].map(|effort| {
        id_with(|input| {
            input.responses_capability = Some(&undeclared);
            input.reasoning_effort = effort;
        })
    });
    assert_pairwise_distinct(&by_effort);
}

/// Two JSON texts that differ only in key order and whitespace are one value
/// to the encoder, and that value has a literal identity computed once,
/// outside this code, straight from the `content_addressable` crate.
#[test]
fn producer_v3_identity_ignores_text_formatting_and_matches_a_pinned_literal() {
    let compact: serde_json::Value = serde_json::from_str(
        r#"{"tenacity":"normal","max_rounds":40,"smart_harness":{"starting_cid":null,"configuration":{"order":[3,1]}}}"#,
    )
    .unwrap();
    let spaced: serde_json::Value = serde_json::from_str(
        "{ \"smart_harness\" : { \"configuration\": { \"order\": [3, 1] },\n  \"starting_cid\": null },\n  \"max_rounds\" : 40, \"tenacity\" : \"normal\" }",
    )
    .unwrap();
    assert_eq!(cid_of(&compact), cid_of(&spaced));
    assert_eq!(
        cid_of(&compact).to_string(),
        "bafyr4if727subfusdzqou5dxjejg2ofhvfw46vn3zquf5gg4tzaz6jdqp4"
    );
}

/// The emitted digest of the fixed fixture equals a literal, and that literal
/// was minted from a hand-written copy of the expected value, not this helper.
#[test]
fn producer_v3_emits_a_pinned_literal_identity_for_a_pinned_literal_config() {
    let record = contract_record(&inputs());
    let expected: serde_json::Value = serde_json::from_str(
        r#"{ "wire_api": "chat_completions", "tenacity": "normal", "ocap": "off",
             "max_rounds": 40, "initiative": "measured", "cognition": "default",
             "crew": "off", "context_window": 32768,
             "tool_round_limit": { "rounds": 40, "source": "config",
                                   "configured": 40, "tenacity": null } }"#,
    )
    .unwrap();
    assert_eq!(record["effective_config"], expected);
    assert_eq!(
        record["config_digest"],
        "bafyr4ib52x6s3qahxqphaovb3d7mcleasimf2ittpwqq2msmjkrrxbb4am"
    );
}

/// Accepted Chat Completions controls change the request body, so they change
/// the identity; absent (another wire) stays distinct from declared-but-empty.
#[test]
fn policy_identity_changes_with_accepted_chat_controls() {
    use newt_core::model_card::{ChatCompletionsCapability as C, ReasoningReplayScope as S};
    let base = C {
        cognition: Some(true),
        chat_template_kwargs: Some(true),
        ..C::default()
    };
    let with =
        |capability, scope| id_with(|input| input.chat_completions = Some((capability, scope)));
    let first = with(base, S::Never);
    assert_eq!(
        first,
        with(base, S::Never),
        "identical controls, identical identity"
    );
    assert_pairwise_distinct(&[
        first,
        with(
            C {
                chat_template_kwargs: Some(false),
                ..base
            },
            S::Never,
        ),
        with(
            C {
                chat_template_kwargs: None,
                ..base
            },
            S::Never,
        ),
        with(
            C {
                parallel_tool_calls: Some(false),
                ..base
            },
            S::Never,
        ),
        with(
            C {
                parallel_tool_calls: Some(true),
                ..base
            },
            S::Never,
        ),
        with(
            C {
                bounded_reasoning_continuation: Some(true),
                ..base
            },
            S::Never,
        ),
        with(
            C {
                cognition: Some(false),
                ..base
            },
            S::Never,
        ),
        with(base, S::CurrentUserTurn),
        with(base, S::FullHistory),
        with(C::default(), S::Never),
        id_with(|_| {}),
    ]);
    let mut input = inputs();
    assert!(contract_record(&input)["effective_config"]
        .get("chat_completions")
        .is_none());
    input.chat_completions = Some((base, S::CurrentUserTurn));
    assert_eq!(
        contract_record(&input)["effective_config"]["chat_completions"],
        serde_json::json!({
            "capability": {"cognition": true, "chat_template_kwargs": true},
            "reasoning_replay_scope": "current_user_turn"
        })
    );
}
