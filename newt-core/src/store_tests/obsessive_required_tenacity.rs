// UNCOMPILED / UNRUN. Append inside the existing core obsessive pin test
// module to reuse PinDb and original_a. No independent minting or verifier.
/// An explicit auto selector is valid; absent evidence must not infer auto.
#[test]
fn obsessive_pin_missing_nullable_tenacity_is_refused_by_actual_reader() {
    let db = PinDb::new();
    db.create("required-selector");
    let mut value = serde_json::to_value(db.pin("required-selector", original_a())).unwrap();
    assert!(value["obsessive"]["inverse"]["original"]["tenacity"].is_null());
    let cid = value["obsessive"]["id"].clone();
    let removed = value["obsessive"]["inverse"]["original"]
        .as_object_mut()
        .unwrap()
        .remove("tenacity");
    assert_eq!(removed, Some(serde_json::Value::Null));
    assert_eq!(value["obsessive"]["id"], cid);
    db.restore_raw("required-selector", &value);
    let before = db.raw_pin("required-selector");
    assert!(
        db.store.preference_pin("required-selector").is_err(),
        "missing required evidence must not deserialize back to auto and verify"
    );
    assert_eq!(db.raw_pin("required-selector"), before);
}
