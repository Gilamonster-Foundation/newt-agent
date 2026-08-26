//! **What a build does with a record it was not written for** (A2.1,
//! #1828 — ADR laws 1 and 5).
//!
//! Three rules, and they pull in different directions on purpose:
//!
//! 1. **The bytes survive.** Stripping metadata always leaves a useful
//!    document (law 1), so an unrecognized record is kept VERBATIM rather
//!    than normalized into something this build happens to understand.
//! 2. **Unknown REQUIRED behavior fails closed.** If a document demands
//!    something we cannot do, we refuse. Guessing is how a mandatory
//!    secret field turns into a silently dropped one.
//! 3. **Unknown OPTIONAL behavior degrades VISIBLY.** Proceeding is right;
//!    proceeding quietly is not. The shortfall is reported in the result.
//!
//! The fourth test is the one that keeps the other three honest: a forward
//! version is never PARTIALLY interpreted. Reading the fields we recognize
//! out of a v2 record and calling the result a v1 is the most tempting
//! failure here, and the most damaging — it produces a record that looks
//! valid, whose id will not match what its author minted.

use content_addressable::canonical;
use newt_interaction::{
    decode_definition, decode_instance, decode_response, plan_presentation, Decoded, FeatureDemand,
    InteractionDefinition, ProtocolError, Requirement, SurfaceFeature, UnknownReason,
    DEFINITION_SCHEMA_V1,
};
use serde::Serialize;

mod fixtures;
use fixtures::definition;

/// A record from a version this build was not written for: the fields a v1
/// would recognize, plus one it would not, under a v2 tag.
#[derive(Serialize)]
struct ForwardDefinition {
    schema: &'static str,
    kind: &'static str,
    revision: u64,
    markdown: &'static str,
    controls: Vec<()>,
    features: Vec<()>,
    epilogue: &'static str,
}

fn forward_bytes(revision: u64, with_unknown_field: bool) -> Vec<u8> {
    let record = ForwardDefinition {
        schema: "newt.interaction.definition/v2",
        kind: "choice",
        revision,
        markdown: "# later",
        controls: Vec::new(),
        features: Vec::new(),
        epilogue: if with_unknown_field {
            "a shape v1 has no field for"
        } else {
            ""
        },
    };
    canonical::to_canonical_dagcbor(&record).unwrap()
}

/// A record this build does not know is preserved byte-for-byte.
#[test]
fn an_unknown_version_preserves_raw_fallback_bytes() {
    let forward = forward_bytes(0, true);
    match decode_definition(&forward).unwrap() {
        Decoded::Unknown(raw) => {
            assert_eq!(raw.schema(), "newt.interaction.definition/v2");
            assert_eq!(
                raw.bytes(),
                forward.as_slice(),
                "the bytes must survive verbatim — not re-serialized, not \
                 normalized, not trimmed"
            );
        }
        Decoded::Known(_) => panic!("a v2 record was read as a v1"),
    }
}

/// ...and it is never partially interpreted. This is the failure that
/// looks like success: a record assembled from the fields we recognized
/// would carry an id its author never minted.
#[test]
fn a_forward_version_is_never_guessed() {
    let forward = forward_bytes(7, false);
    let decoded = decode_definition(&forward).unwrap();
    assert!(
        matches!(decoded, Decoded::Unknown(_)),
        "every field of this record is one a v1 could read — which is \
         exactly why reading them would be wrong"
    );

    // The known tag still decodes, or the test above proves nothing.
    let known = canonical::to_canonical_dagcbor(&definition()).unwrap();
    match decode_definition(&known).unwrap() {
        Decoded::Known(def) => {
            assert_eq!(def.schema, DEFINITION_SCHEMA_V1);
            assert_eq!(
                def.definition_id().unwrap(),
                definition().definition_id().unwrap()
            );
        }
        Decoded::Unknown(_) => panic!("a v1 record was not recognized"),
    }
}

/// A demand we cannot satisfy, marked Required, refuses.
#[test]
fn an_unknown_required_behavior_fails_closed() {
    let mut def = definition();
    def.features = vec![FeatureDemand {
        feature: SurfaceFeature::new("holography").unwrap(),
        requirement: Requirement::Required,
    }];

    let err = plan_presentation(&def, &[]).unwrap_err();
    match err {
        ProtocolError::UnsupportedFeature { ref feature, .. } => {
            assert_eq!(feature, "holography");
        }
        other => panic!("expected a typed refusal, got {other:?}"),
    }
    // The refusal names the feature, so an operator can tell "this build is
    // too old" from "this document is malformed".
    assert!(err.to_string().contains("holography"));

    // A KNOWN feature the surface lacks refuses on the same terms: the
    // rule is about what the surface can do, not about what we recognize.
    let mut known_but_absent = definition();
    known_but_absent.features = vec![FeatureDemand {
        feature: SurfaceFeature::new(SurfaceFeature::SECRET_INPUT).unwrap(),
        requirement: Requirement::Required,
    }];
    assert!(plan_presentation(&known_but_absent, &[]).is_err());
}

/// The same demand, marked Optional, proceeds — and says so.
#[test]
fn an_unknown_optional_behavior_degrades_visibly() {
    let mut def = definition();
    def.features = vec![
        FeatureDemand {
            feature: SurfaceFeature::new("holography").unwrap(),
            requirement: Requirement::Optional,
        },
        FeatureDemand {
            feature: SurfaceFeature::new(SurfaceFeature::DIAGRAMS).unwrap(),
            requirement: Requirement::Optional,
        },
    ];

    let plan = plan_presentation(&def, &[]).expect("optional demands must not refuse");
    let reported: Vec<&str> = plan.degradations().iter().map(|d| d.feature()).collect();
    assert_eq!(
        reported,
        vec!["holography", SurfaceFeature::DIAGRAMS],
        "every unmet optional demand must be reported, in order"
    );
    assert!(
        !plan.is_faithful(),
        "a plan with unmet demands must not claim to be faithful"
    );

    // Satisfy one and it stops being reported — the marker tracks reality
    // rather than merely recording that demands existed.
    let supported = [SurfaceFeature::new(SurfaceFeature::DIAGRAMS).unwrap()];
    let partial = plan_presentation(&def, &supported).unwrap();
    assert_eq!(
        partial.degradations().len(),
        1,
        "a satisfied demand must not be reported as a shortfall"
    );

    // Nothing demanded, nothing to report.
    let plain = plan_presentation(&definition(), &[]).unwrap();
    assert!(plain.degradations().is_empty());
    assert!(plain.is_faithful());
}

/// Append one unknown field to an encoded DAG-CBOR map.
///
/// The key is 24 `z`s: DAG-CBOR sorts map keys length-first, so a key
/// longer than every real one belongs LAST, and appending it keeps the
/// encoding canonical. That matters — the point is to test the
/// unknown-FIELD rule, not to accidentally test canonical ordering at the
/// same time.
fn with_extra_field(bytes: &[u8]) -> Vec<u8> {
    let header = bytes[0];
    assert!(
        (0xa0..0xb7).contains(&header),
        "expected a small CBOR map header, got {header:#04x}"
    );
    let mut out = vec![header + 1];
    out.extend_from_slice(&bytes[1..]);
    out.push(0x78); // text, one-byte length
    out.push(24);
    out.extend_from_slice(&[b'z'; 24]);
    out.push(0x61); // text, length 1
    out.push(b'x');
    out
}

/// **A record carrying a field this build does not know is NEVER `Known`.**
///
/// Serde drops unknown fields by default, so before this rule a v1 record
/// with one extra field decoded happily, re-encoded 43 bytes shorter, and
/// minted an id for a record its author never wrote — while whatever that
/// field said, possibly a REQUIRED demand, vanished without trace. An
/// unknown field's requiredness is unknowable, so law 5 says fail closed:
/// never interpreted, bytes preserved for whoever does understand them.
#[test]
fn a_known_tag_with_an_unknown_field_is_never_known() {
    let def = definition();
    let inst = fixtures::instance(&def);
    let resp = fixtures::response(&def, &inst);

    let def_bytes = canonical::to_canonical_dagcbor(&def).unwrap();
    let inst_bytes = canonical::to_canonical_dagcbor(&inst).unwrap();
    let resp_bytes = canonical::to_canonical_dagcbor(&resp).unwrap();

    // Each clean record decodes, or the tainted assertions prove nothing.
    assert!(matches!(
        decode_definition(&def_bytes).unwrap(),
        Decoded::Known(_)
    ));
    assert!(matches!(
        decode_instance(&inst_bytes).unwrap(),
        Decoded::Known(_)
    ));
    assert!(matches!(
        decode_response(&resp_bytes).unwrap(),
        Decoded::Known(_)
    ));

    // ...and each tainted one does not, preserving its bytes verbatim.
    let tainted_def = with_extra_field(&def_bytes);
    match decode_definition(&tainted_def).unwrap() {
        Decoded::Unknown(raw) => {
            assert_eq!(raw.bytes(), tainted_def.as_slice());
            assert_eq!(raw.reason(), UnknownReason::Uninterpretable);
        }
        Decoded::Known(known) => panic!(
            "an unknown field was dropped and the record minted {} — an id \
             its author never wrote",
            known.definition_id().unwrap()
        ),
    }

    let tainted_inst = with_extra_field(&inst_bytes);
    assert!(
        matches!(decode_instance(&tainted_inst).unwrap(), Decoded::Unknown(_)),
        "an instance with an unknown field decoded as known"
    );

    let tainted_resp = with_extra_field(&resp_bytes);
    assert!(
        matches!(decode_response(&tainted_resp).unwrap(), Decoded::Unknown(_)),
        "a response with an unknown field decoded as known"
    );
}

/// Rewrite `revision`'s value from CBOR's minimal form to a non-minimal
/// one: `0x00` (immediate 0) becomes `0x18 0x00` (one-byte follow, still
/// 0). Valid CBOR, decodes to the same number, and is NOT the canonical
/// encoding — the shape a foreign encoder produces by accident.
fn make_non_canonical(bytes: &[u8]) -> Vec<u8> {
    let key: Vec<u8> = std::iter::once(0x68u8)
        .chain(b"revision".iter().copied())
        .collect();
    let at = bytes
        .windows(key.len())
        .position(|w| w == key.as_slice())
        .expect("the record carries a `revision` key");
    let value_at = at + key.len();
    assert_eq!(
        bytes[value_at], 0x00,
        "expected revision 0 in minimal form, got {:#04x}",
        bytes[value_at]
    );
    let mut out = bytes[..value_at].to_vec();
    out.extend_from_slice(&[0x18, 0x00]);
    out.extend_from_slice(&bytes[value_at + 1..]);
    out
}

/// **Bytes that decode are not thereby canonical.**
///
/// A reordered map key, a non-minimal integer, an indefinite-length string
/// — each decodes to the right value and re-encodes to different bytes,
/// which means a different id. Accepting them would let two encodings of
/// one record carry two identities, and the one an author published would
/// not be the one a consumer computed.
#[test]
fn non_canonical_bytes_of_a_valid_record_are_refused() {
    let def = definition();
    let canonical_bytes = canonical::to_canonical_dagcbor(&def).unwrap();
    let perturbed = make_non_canonical(&canonical_bytes);
    assert_ne!(perturbed, canonical_bytes, "the fixture perturbed nothing");

    // It really does still decode — otherwise this tests the decoder's
    // strictness rather than our check.
    let lenient: InteractionDefinition =
        canonical::from_canonical_dagcbor(&perturbed).expect("perturbed bytes still decode");
    assert_eq!(
        lenient, def,
        "the perturbation changed the VALUE, not just the bytes"
    );

    match decode_definition(&perturbed) {
        Err(ProtocolError::NonCanonical { ref schema, .. }) => {
            assert_eq!(schema, DEFINITION_SCHEMA_V1);
        }
        Err(other) => panic!("expected NonCanonical, got {other:?}"),
        Ok(_) => panic!("non-canonical bytes were accepted"),
    }
}

/// **Anti-vacuous twin for the byte-equality gate.** The same bytes, run
/// through a decoder WITHOUT that gate, are accepted — which is what makes
/// the refusal above attributable to the gate rather than to the decoder
/// happening to be strict. (It is not: the assertion above proves it
/// decodes.)
#[test]
fn without_the_byte_check_the_same_bytes_are_accepted() {
    let def = definition();
    let perturbed = make_non_canonical(&canonical::to_canonical_dagcbor(&def).unwrap());

    // Gates 1 and 2 only — the shape this decoder had before the byte
    // check existed.
    let probe: serde_json::Value = {
        let decoded: InteractionDefinition =
            canonical::from_canonical_dagcbor(&perturbed).expect("gate 2 passes");
        serde_json::to_value(decoded).unwrap()
    };
    assert_eq!(
        probe["schema"], DEFINITION_SCHEMA_V1,
        "a gate-2-only decoder accepts these bytes, so the refusal is the \
         byte check's doing"
    );
}
