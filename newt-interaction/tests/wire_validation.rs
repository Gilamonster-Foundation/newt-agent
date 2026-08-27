//! **A validated scalar must be validated on the WIRE, not only in Rust**
//! (A2.1 round 3, #1828).
//!
//! `ControlId::new` refuses `"this is not an id"`. A derived
//! `Deserialize` on a `#[serde(transparent)]` newtype builds the private
//! field directly and never calls the constructor — so the same value
//! arrives intact over the wire, in canonical bytes that re-encode
//! identically, which means gate 3 cannot see it either.
//!
//! The tell that this is a recurring class rather than an oversight: the
//! `Choice{option}` doc asserted "its charset is enforced at construction,
//! so a choice can never carry a sentence." True of the constructor,
//! false of the wire — a guarantee published in a doc comment and not
//! provided by the code.

use content_addressable::canonical;
use newt_interaction::{
    decode_definition, ControlId, Decoded, IdempotencyKey, Nonce, OptionId, SecretRef,
    SurfaceFeature,
};

mod fixtures;
use fixtures::definition;

/// Replace an exact-length substring inside encoded bytes, so the CBOR
/// stays structurally identical and canonically encoded — only the
/// scalar's CONTENT becomes invalid.
fn patch_same_length(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    assert_eq!(from.len(), to.len(), "the patch must not change any length");
    let at = bytes
        .windows(from.len())
        .position(|w| w == from.as_bytes())
        .unwrap_or_else(|| panic!("`{from}` not found in the encoded record"));
    let mut out = bytes.to_vec();
    out[at..at + to.len()].copy_from_slice(to.as_bytes());
    out
}

/// Each validated scalar refuses its invalid forms at the serde boundary,
/// not merely in its constructor.
#[test]
fn an_invalid_scalar_does_not_deserialize() {
    // Charset-validated ids.
    for bad in ["\"not an id\"", "\"\"", "\"has/slash\"", "\"tab\\there\""] {
        assert!(
            serde_json::from_str::<ControlId>(bad).is_err(),
            "ControlId accepted {bad}"
        );
        assert!(
            serde_json::from_str::<OptionId>(bad).is_err(),
            "OptionId accepted {bad}"
        );
    }
    // Non-empty scalars.
    for ty in ["Nonce", "IdempotencyKey", "SecretRef", "SurfaceFeature"] {
        let empty = "\"\"";
        let refused = match ty {
            "Nonce" => serde_json::from_str::<Nonce>(empty).is_err(),
            "IdempotencyKey" => serde_json::from_str::<IdempotencyKey>(empty).is_err(),
            "SecretRef" => serde_json::from_str::<SecretRef>(empty).is_err(),
            _ => serde_json::from_str::<SurfaceFeature>(empty).is_err(),
        };
        assert!(refused, "{ty} accepted an empty string");
    }

    // ...and the valid forms still round-trip, or the rule is just "refuse
    // everything".
    let id: ControlId = serde_json::from_str("\"decision\"").unwrap();
    assert_eq!(id.as_str(), "decision");
    let feature: SurfaceFeature = serde_json::from_str("\"holography\"").unwrap();
    assert_eq!(
        feature.as_str(),
        "holography",
        "an UNRECOGNIZED feature name is still a valid one — that is the \
         forward-compatibility case, not an invalid scalar"
    );
}

/// A canonical record carrying an invalid scalar is never `Known`.
///
/// The bytes are canonical and re-encode identically, so gate 3 cannot
/// catch this; only validating deserialization can.
#[test]
fn a_canonical_record_with_an_invalid_id_is_never_known() {
    let clean = canonical::to_canonical_dagcbor(&definition()).unwrap();
    assert!(
        matches!(decode_definition(&clean).unwrap(), Decoded::Known(_)),
        "the clean record must decode, or this proves nothing"
    );

    // "decision" -> "deci ion": same length, so the encoding stays
    // canonical; only the charset rule is broken.
    let tainted = patch_same_length(&clean, "decision", "deci ion");
    assert_eq!(tainted.len(), clean.len(), "the patch changed the length");

    match decode_definition(&tainted).unwrap() {
        Decoded::Unknown(raw) => assert_eq!(raw.bytes(), tainted.as_slice()),
        Decoded::Known(known) => panic!(
            "a record naming control `deci ion` decoded as valid and minted {}",
            known.definition_id().unwrap()
        ),
    }

    // The same for an option id, which is what `Choice{option}`'s doc
    // claimed could never carry a sentence.
    let tainted_option = patch_same_length(&clean, "allow-once", "allow once");
    assert!(
        !matches!(
            decode_definition(&tainted_option).unwrap(),
            Decoded::Known(_)
        ),
        "an option id carrying a space decoded as valid"
    );
}
