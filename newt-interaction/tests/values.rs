//! **Typed responses are typed at the TYPE, not at a downstream parser**
//! (A2.0 review item 3, #1828).
//!
//! `ControlValue` used to be `value: String`, which made every consumer a
//! parser: `"TRUE"`, `"true"`, `"1"`, and `"yes"` were all submittable for
//! a toggle, and canonicalization became A3's problem — repeated per
//! surface, which is exactly the drift the epic exists to end. Freezing the
//! shape before A2.1 pins schemas and vectors is the last cheap moment.
//!
//! The secret case is the sharp one: a content-addressed response is a
//! durable, tamper-evident record, and a secret inside one is a disclosure
//! liability forever. So a secret is carried by REFERENCE and the type
//! offers no way to put plaintext in.

use newt_interaction::{ControlId, ControlValue, SecretRef};

/// A toggle is a bool. `"TRUE"` is not a value this protocol can express,
/// on the wire or in memory.
#[test]
fn a_toggle_cannot_be_submitted_as_a_string() {
    for spelling in [
        r#"{"kind":"toggle","on":"TRUE"}"#,
        r#"{"kind":"toggle","on":"true"}"#,
        r#"{"kind":"toggle","on":"1"}"#,
        r#"{"kind":"toggle","on":"yes"}"#,
        r#"{"kind":"toggle","value":true}"#,
    ] {
        assert!(
            serde_json::from_str::<ControlValue>(spelling).is_err(),
            "a stringly toggle must not deserialize: {spelling}"
        );
    }
    let real = ControlValue::Toggle { on: true };
    assert_eq!(
        serde_json::to_value(&real).unwrap()["on"],
        serde_json::Value::Bool(true),
        "a toggle travels as a bool, not as text"
    );
}

/// A choice names one of the definition's controls; it is not free text.
#[test]
fn a_choice_names_a_control_not_free_text() {
    let choice = ControlValue::Choice {
        option: ControlId::new("allow-once").unwrap(),
    };
    let wire = serde_json::to_value(&choice).unwrap();
    assert_eq!(wire["kind"], "choice");
    assert_eq!(wire["option"], "allow-once");
    // A control id's charset is enforced at construction, so a choice can
    // never carry a sentence.
    assert!(ControlId::new("not a control id").is_err());
}

/// **No `ControlValue` variant can carry secret bytes.** The exhaustive
/// match is the proof: adding a variant that could would fail to compile
/// here until someone justifies it.
#[test]
fn no_control_value_variant_can_carry_a_secret() {
    let sealed = SecretRef::new("vault-handle-9").unwrap();
    let value = ControlValue::Secret {
        reference: sealed.clone(),
    };

    match &value {
        // Text is operator-typed content that is NOT marked secret; a
        // definition marks a control `Secret` when its input must not be
        // echoed or persisted, and that control's value is a reference.
        ControlValue::Text { .. } | ControlValue::Toggle { .. } | ControlValue::Choice { .. } => {}
        ControlValue::Secret { reference } => {
            assert_eq!(reference.as_str(), "vault-handle-9");
        }
    }

    // The serialized form carries the handle and nothing resolvable to a
    // secret by whoever holds the transcript.
    let wire = serde_json::to_string(&value).unwrap();
    assert!(wire.contains("vault-handle-9"));
    assert!(
        !wire.contains("password"),
        "no plaintext path exists: {wire}"
    );
}

/// A reference must actually reference something.
#[test]
fn a_secret_reference_must_not_be_empty() {
    assert!(SecretRef::new("").is_err());
}

/// Every variant round-trips through JSON unchanged — the wire shape A2.1
/// will freeze into vectors.
#[test]
fn every_variant_round_trips() {
    let values = vec![
        ControlValue::Choice {
            option: ControlId::new("deny").unwrap(),
        },
        ControlValue::Text {
            text: "a typed answer".to_string(),
        },
        ControlValue::Toggle { on: false },
        ControlValue::Secret {
            reference: SecretRef::new("handle").unwrap(),
        },
    ];
    for value in values {
        let json = serde_json::to_string(&value).unwrap();
        let back: ControlValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, value, "round trip changed the value: {json}");
    }
}
