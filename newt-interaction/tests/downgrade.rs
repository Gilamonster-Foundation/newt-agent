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
    decode_definition, plan_presentation, Decoded, FeatureDemand, ProtocolError, Requirement,
    SurfaceFeature, DEFINITION_SCHEMA_V1,
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
