//! Reading a record this build may not have been written for, and
//! presenting one a surface may not be able to satisfy.
//!
//! Two ADR laws meet here. Law 1: stripping metadata always leaves a
//! useful document, so an unrecognized record is kept VERBATIM rather than
//! normalized into whatever this build happens to understand. Law 5:
//! unknown REQUIRED behavior fails closed, unknown OPTIONAL behavior
//! degrades visibly.
//!
//! The rule that keeps both honest is that a forward version is never
//! PARTIALLY interpreted. Reading the fields we recognize out of a v2
//! record and calling the result a v1 is the tempting failure — it yields
//! a record that looks valid and carries an id its author never minted.

use content_addressable::{canonical, ContentAddressable};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::definition::{FeatureDemand, InteractionDefinition, Requirement, SurfaceFeature};
use crate::error::ProtocolError;
use crate::instance::InteractionInstance;
use crate::response::Response;

/// Why a record was preserved instead of interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnknownReason {
    /// The tag names a LATER version of the record type that was asked
    /// for. The operator's fix is a newer build.
    ForwardVersion,
    /// The tag names a DIFFERENT record type in this protocol — an
    /// instance handed to the definition decoder, say. The fix is a
    /// routing correction, not an upgrade, and saying "forward version"
    /// would send someone looking for a release that will never help.
    WrongRecordType,
    /// The tag is not one of this protocol's at all: foreign data, or
    /// corruption that happened to leave a readable string.
    UnknownSchema,
    /// The tag is ours, but the record does not fit the shape this build
    /// knows — most often a field we have no name for. Its requiredness is
    /// unknowable, so it is never interpreted.
    Uninterpretable,
}

/// A record this build did not interpret, kept whole.
///
/// The bytes are the payload: not re-serialized, not normalized, not
/// trimmed. A consumer can pass them on, store them, or show them, and
/// whoever does understand the version gets exactly what was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    schema: String,
    bytes: Vec<u8>,
    reason: UnknownReason,
}

impl RawRecord {
    /// The schema tag the record declared.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// The original bytes, verbatim.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Why it was not interpreted.
    #[must_use]
    pub fn reason(&self) -> UnknownReason {
        self.reason
    }
}

/// The result of reading a record: understood, or preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded<T> {
    /// The tag was known, the shape fit, and the bytes were canonical.
    Known(T),
    /// Anything else. Deliberately no partial interpretation.
    Unknown(RawRecord),
}

/// Just enough of any record to read its tag without committing to a shape.
///
/// **Deliberately NOT `deny_unknown_fields`, and that absence is
/// load-bearing.** Every other `Deserialize` type here forbids unknown
/// fields; this one must tolerate them, because its entire job is to read
/// the tag out of a record whose remaining shape is exactly what we do not
/// yet know. Adding the attribute for consistency would make every real
/// record fail the probe and turn all three gates into `Malformed`,
/// including records this build understands perfectly.
/// `the_probe_reads_a_tag_it_does_not_understand` pins it.
#[derive(Deserialize)]
struct SchemaProbe {
    schema: String,
}

/// Read a record of type `T`, preserving it whole unless this build can
/// account for every byte.
///
/// Three gates, in order, and each exists because the one before it is not
/// enough on its own:
///
/// 1. **The tag.** A version we do not have is preserved (law 1) and never
///    guessed at.
/// 2. **The shape.** `deny_unknown_fields` refuses a record carrying a
///    field we have no name for. Serde's default is to DROP such a field,
///    which silently discards whatever it said — possibly a required
///    demand — and yields a record whose id its author never minted.
/// 3. **The bytes.** Even a record that decodes may not be the record that
///    was written: reordered map keys, a non-minimal integer, an
///    indefinite-length string all decode fine and re-encode differently.
///    So the decoded value is re-encoded and compared byte-for-byte with
///    the input, and any difference is a refusal.
///
/// Gate 3 is not redundant with gate 2. It does not depend on how strict
/// the CBOR decoder happens to be in the version we have locked, and
/// decoder strictness drifts between releases — this repo has been bitten
/// by exactly that before. The rule learned there applies here: prove a
/// non-canonical encoding by FORWARD encoding, never by trusting a decoder
/// to reject it.
fn decode<T>(bytes: &[u8], expected: &'static str) -> Result<Decoded<T>, ProtocolError>
where
    T: DeserializeOwned + ContentAddressable,
{
    let probe: SchemaProbe =
        canonical::from_canonical_dagcbor(bytes).map_err(|e| ProtocolError::Malformed {
            reason: format!("no readable schema tag: {e}"),
        })?;
    if probe.schema != expected {
        let reason = classify_tag(&probe.schema, expected);
        return Ok(Decoded::Unknown(RawRecord {
            schema: probe.schema,
            bytes: bytes.to_vec(),
            reason,
        }));
    }

    // Gate 2. A shape we cannot fully account for is preserved, not
    // partially read. The probe already proved these bytes are a map with
    // a schema string, so a failure here is "shaped like a record we do
    // not know", not "not a record".
    let Ok(record) = canonical::from_canonical_dagcbor::<T>(bytes) else {
        return Ok(Decoded::Unknown(RawRecord {
            schema: probe.schema,
            bytes: bytes.to_vec(),
            reason: UnknownReason::Uninterpretable,
        }));
    };

    // Gate 3.
    let reencoded = record.canonical_form()?;
    if reencoded != bytes {
        return Err(ProtocolError::NonCanonical {
            schema: probe.schema,
            decoded_len: reencoded.len(),
            input_len: bytes.len(),
        });
    }
    Ok(Decoded::Known(record))
}

/// Which kind of schema mismatch this is.
///
/// Tags are `newt.interaction.<record>/v<n>`, so the record name is the
/// segment before the last `/`. Same record, different version is a
/// version gap; a different record in our namespace is a routing mistake;
/// anything else is not ours.
fn classify_tag(seen: &str, expected: &'static str) -> UnknownReason {
    let family = |tag: &str| tag.rsplit_once('/').map(|(head, _)| head.to_string());
    match (family(seen), family(expected)) {
        (Some(seen_family), Some(expected_family)) if seen_family == expected_family => {
            UnknownReason::ForwardVersion
        }
        (Some(seen_family), _) if seen_family.starts_with("newt.interaction.") => {
            UnknownReason::WrongRecordType
        }
        _ => UnknownReason::UnknownSchema,
    }
}

/// Read a definition. See [`decode`] for the three gates.
///
/// # Errors
///
/// [`ProtocolError::Malformed`] when the bytes are not a readable record;
/// [`ProtocolError::NonCanonical`] when they decode but are not the
/// canonical encoding of what they decode to.
pub fn decode_definition(bytes: &[u8]) -> Result<Decoded<InteractionDefinition>, ProtocolError> {
    decode(bytes, crate::definition::DEFINITION_SCHEMA_V1)
}

/// Read an instance. See [`decode`] for the three gates.
///
/// # Errors
///
/// As [`decode_definition`].
pub fn decode_instance(bytes: &[u8]) -> Result<Decoded<InteractionInstance>, ProtocolError> {
    decode(bytes, crate::instance::INSTANCE_SCHEMA_V1)
}

/// Read a response. See [`decode`] for the three gates.
///
/// # Errors
///
/// As [`decode_definition`].
pub fn decode_response(bytes: &[u8]) -> Result<Decoded<Response>, ProtocolError> {
    decode(bytes, crate::response::RESPONSE_SCHEMA_V1)
}

/// One demand a surface could not meet, reported rather than swallowed.
///
/// A consumer-side RESULT, not a wire record: it is computed here from a
/// definition and a surface's capabilities, and nothing ever parses one.
/// It therefore derives no serde traits at all — which is why it carries
/// no `deny_unknown_fields`, and why it is absent from the schema and
/// vector corpora. A2 publishes the records that travel; how A3 chooses to
/// report a shortfall outward is A3's envelope to define.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degradation {
    feature: SurfaceFeature,
}

impl Degradation {
    /// The feature that went unmet.
    #[must_use]
    pub fn feature(&self) -> &str {
        self.feature.as_str()
    }
}

/// What a surface can honestly present, and what it had to give up.
///
/// A consumer-side result like [`Degradation`], and not on the wire for
/// the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation {
    degradations: Vec<Degradation>,
}

impl Presentation {
    /// Every optional demand this surface could not meet, in the order the
    /// definition asked for them.
    #[must_use]
    pub fn degradations(&self) -> &[Degradation] {
        &self.degradations
    }

    /// Whether every demand was met.
    ///
    /// Named for what a caller wants to know. A surface that dropped a
    /// diagram is still usable, but it is not showing what was written,
    /// and the difference should be visible rather than inferred from an
    /// empty list.
    #[must_use]
    pub fn is_faithful(&self) -> bool {
        self.degradations.is_empty()
    }
}

/// Capabilities a definition's own controls REQUIRE, whether or not its
/// author restated them in `features`.
///
/// A Secret control needs a surface that accepts secret input without
/// echoing or persisting it — that is what makes it a secret control, not
/// a separate claim its author might forget. Reading only the declared
/// `features` let a definition with a required Secret control report
/// `is_faithful() == true` on a surface that cannot handle one, which is
/// the shortfall that matters most.
///
/// Derived rather than validated at construction on purpose: derivation
/// has NO wire impact, so it stays correctable if the rule turns out
/// wrong. Requiring authors to declare it — or refusing records that do
/// not — would change which records are representable, and this slice
/// freezes that.
///
/// The control's own [`Requirement`] carries through: an optional secret
/// field degrades visibly rather than refusing, or the derivation would be
/// stricter than its author asked for.
fn intrinsic_demands(definition: &InteractionDefinition) -> Vec<FeatureDemand> {
    let mut demands: Vec<FeatureDemand> = Vec::new();
    for control in &definition.controls {
        let Some(feature) = (match control.kind {
            crate::definition::ControlKind::Secret => Some(SurfaceFeature::SECRET_INPUT),
            _ => None,
        }) else {
            continue;
        };
        let Ok(feature) = SurfaceFeature::new(feature) else {
            continue;
        };
        // A feature demanded by two controls is one demand; the stricter
        // requirement wins, since satisfying it satisfies both.
        match demands.iter_mut().find(|d| d.feature == feature) {
            Some(existing) => {
                if control.requirement == Requirement::Required {
                    existing.requirement = Requirement::Required;
                }
            }
            None => demands.push(FeatureDemand {
                feature,
                requirement: control.requirement,
            }),
        }
    }
    demands
}

/// Decide what `supported` can present of `definition`.
///
/// # Errors
///
/// [`ProtocolError::UnsupportedFeature`] when a REQUIRED demand cannot be
/// met — whether because this build does not recognize the feature or
/// because the surface lacks it. Both are the same fact to the operator:
/// this document cannot be shown faithfully, and guessing is not on offer.
pub fn plan_presentation(
    definition: &InteractionDefinition,
    supported: &[SurfaceFeature],
) -> Result<Presentation, ProtocolError> {
    let mut degradations = Vec::new();
    for demand in intrinsic_demands(definition)
        .iter()
        .chain(definition.features.iter())
    {
        if supported.contains(&demand.feature) {
            continue;
        }
        match demand.requirement {
            Requirement::Required => {
                return Err(ProtocolError::UnsupportedFeature {
                    feature: demand.feature.as_str().to_string(),
                    known: demand.feature.is_known(),
                })
            }
            Requirement::Optional => degradations.push(Degradation {
                feature: demand.feature.clone(),
            }),
        }
    }
    Ok(Presentation { degradations })
}
