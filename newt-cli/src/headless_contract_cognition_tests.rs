/// Suite #2449: no instantiated turn is absent; an instantiated unset/off
/// selection is explicit null. Neither is fabricated as operator-off.
#[test]
fn semantic_cognition_distinguishes_absent_turn_from_captured_none() {
    let mut input = inputs();
    assert!(contract_record(&input)["effective_config"]
        .get("semantic_cognition")
        .is_none());
    input.semantic_cognition = Some(None);
    assert_eq!(
        contract_record(&input)["effective_config"].get("semantic_cognition"),
        Some(&serde_json::Value::Null)
    );
    input.semantic_cognition = Some(Some(newt_core::role_profile::Cognition::Meticulous));
    let config = contract_record(&input)["effective_config"].clone();
    assert_eq!(config["semantic_cognition"], "meticulous");
    assert_eq!(
        config["cognition"], "default",
        "historical projected field keeps its meaning"
    );
}
