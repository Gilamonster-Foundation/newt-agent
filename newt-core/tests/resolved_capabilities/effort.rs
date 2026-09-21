//! Ordered effort declarations travel through the existing exact binding.
//! No model-name guessing or provider requests participate in these decisions.

use super::*;

fn declared(values: &[&str]) -> Capability {
    serde_json::from_value(serde_json::json!({
        "responses": { "reasoning_effort": values }
    }))
    .expect("an ordered Responses effort declaration is a supported capability")
}

fn efforts(decision: &newt_core::model_card::CapabilityDecision) -> serde_json::Value {
    serde_json::to_value(decision.effective()).unwrap()["responses"]["reasoning_effort"].clone()
}

fn effort_card(values: &str) -> String {
    format!(
        "name = \"team-reasoner\"\nbackend = \"vllm\"\n[vllm]\nserved_name = \"team-reasoner\"\n[capability.responses]\nreasoning_effort = [{values}]\n"
    )
}

/// Suite #2449: accepted effort is endpoint evidence, not a global maximum.
/// The real catalog grounds the same binding and deserialization used at startup.
#[test]
fn effort_ladders_survive_card_binding_and_inline_override() {
    let card = effort_card("\"minimal\", \"low\", \"medium\", \"high\"");
    let (_dir, config) = catalog_with(&[("team-reasoner", &card)]);
    let b = backend(Some("bound-model"), Some("team-reasoner"), None);
    let caps = resolve(&b, Some(&config)).expect("valid advertised efforts resolve");
    assert_eq!(
        efforts(&caps.for_route(&home(), ServingPrincipal::SelectedModel("bound-model"))),
        serde_json::json!(["minimal", "low", "medium", "high"])
    );

    let b = backend(
        Some("bound-model"),
        Some("team-reasoner"),
        Some(declared(&["minimal", "low"])),
    );
    let overlaid = resolve(&b, Some(&config)).unwrap();
    assert_eq!(
        efforts(&overlaid.for_route(&home(), ServingPrincipal::Instance)),
        serde_json::json!(["minimal", "low"]),
        "the backend's explicit ladder replaces the card ladder as a whole"
    );
    let previous = caps.for_route(&home(), ServingPrincipal::Instance);
    let elsewhere = BackendDestination::new(Some("http://127.0.0.1:11435".into()), None);
    for decision in [
        caps.for_route(&elsewhere, ServingPrincipal::Instance),
        caps.for_route(&home(), ServingPrincipal::SelectedModel("another-model")),
        caps.for_route(&home(), ServingPrincipal::Unknown),
    ] {
        assert!(
            efforts(&decision).is_null(),
            "no borrowed ladder across a binding mismatch"
        );
    }
    assert_eq!(
        efforts(&previous),
        serde_json::json!(["minimal", "low", "medium", "high"])
    );
}

/// Suite #2449: thinking support is not evidence for Responses effort values.
#[test]
fn thinking_only_and_unknown_capabilities_advertise_no_effort_ladder() {
    let b = backend(
        Some("bound-model"),
        None,
        Some(
            serde_json::from_value(serde_json::json!({
                "thinking_default": true,
                "chat_completions": { "cognition": true, "chat_template_kwargs": true }
            }))
            .unwrap(),
        ),
    );
    assert!(efforts(
        &resolve(&b, None)
            .unwrap()
            .for_route(&home(), ServingPrincipal::Instance)
    )
    .is_null());
    assert!(
        efforts(&ResolvedCapabilities::none().for_route(&home(), ServingPrincipal::Unknown))
            .is_null()
    );
}

/// Suite #2449: malformed declarations must fail on inline and catalog ingress.
#[test]
fn malformed_effort_declarations_are_refused() {
    for values in [
        serde_json::json!([]),
        serde_json::json!(["low", "low"]),
        serde_json::json!(["high", "low"]),
        serde_json::json!(["maximum"]),
        serde_json::json!(["xhigh"]),
    ] {
        assert!(
            serde_json::from_value::<Capability>(serde_json::json!({
                "responses": { "reasoning_effort": values }
            }))
            .is_err(),
            "invalid or unsupported declaration: {values}"
        );
        let list = values
            .as_array()
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let card = effort_card(&list);
        let (_dir, config) = catalog_with(&[("team-reasoner", &card)]);
        let b = backend(Some("bound-model"), Some("team-reasoner"), None);
        assert!(
            resolve(&b, Some(&config)).is_err(),
            "catalog must reject {values}"
        );
    }
}

/// Suite #2449: serde must preserve semantic order, not lexicographically sort it.
#[test]
fn effort_capability_round_trip_preserves_semantic_order() {
    let capability = declared(&["minimal", "low", "medium", "high"]);
    let encoded = toml::to_string(&capability).unwrap();
    let decoded: Capability = toml::from_str(&encoded).unwrap();
    assert_eq!(decoded, capability);
    assert_eq!(
        serde_json::to_value(decoded).unwrap()["responses"]["reasoning_effort"],
        serde_json::json!(["minimal", "low", "medium", "high"])
    );
}

/// Suite #2449: absent declarations preserve the old serialized capability
/// payload, rather than adding a null/default capability to existing records.
#[test]
fn absent_responses_declaration_preserves_existing_payload() {
    let old = serde_json::json!({
        "thinking_default": true,
        "chat_completions": { "cognition": true }
    });
    let capability: Capability = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(serde_json::to_value(&capability).unwrap(), old);
    let round_trip: Capability = toml::from_str(&toml::to_string(&capability).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(round_trip).unwrap(), old);
    assert_eq!(
        serde_json::to_value(Capability::default()).unwrap(),
        serde_json::json!({})
    );
}

/// Suite #2449: typed maxima follow semantic order, never string ordering.
#[test]
fn typed_advertised_maximum_comes_from_the_validated_ladder() {
    use newt_core::model_card::ReasoningEffort;
    for (values, maximum) in [
        (vec!["minimal", "low"], ReasoningEffort::Low),
        (vec!["low", "medium", "high"], ReasoningEffort::High),
    ] {
        let b = backend(Some("bound-model"), None, Some(declared(&values)));
        let decision = resolve(&b, None)
            .unwrap()
            .for_route(&home(), ServingPrincipal::Instance);
        assert_eq!(
            decision.responses().reasoning_effort.unwrap().maximum(),
            maximum
        );
    }
}
