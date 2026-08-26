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

use content_addressable::canonical;
use serde::Deserialize;

use crate::definition::{InteractionDefinition, Requirement, SurfaceFeature};
use crate::error::ProtocolError;

/// A record whose schema tag this build does not know, kept whole.
///
/// The bytes are the payload: not re-serialized, not normalized, not
/// trimmed. A consumer can pass them on, store them, or show them, and
/// whoever does understand the version gets exactly what was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    schema: String,
    bytes: Vec<u8>,
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
}

/// The result of reading a record: understood, or preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded<T> {
    /// The tag was known and the record parsed.
    Known(T),
    /// The tag was not known. Deliberately no partial interpretation.
    Unknown(RawRecord),
}

/// Just enough of any record to read its tag without committing to a shape.
#[derive(Deserialize)]
struct SchemaProbe {
    schema: String,
}

/// Read a definition, preserving it whole if this build does not know its
/// version.
///
/// # Errors
///
/// [`ProtocolError::Malformed`] when the bytes are not a readable record
/// carrying a `schema` string — corruption is a different fact from a
/// version we do not know, and is reported differently.
pub fn decode_definition(bytes: &[u8]) -> Result<Decoded<InteractionDefinition>, ProtocolError> {
    // Canonical DAG-CBOR: the same bytes identity is minted over, so a
    // record that survives this round trip is the record whose id its
    // author published. Reading the tag first, with a probe that commits
    // to no other field, is what makes "unknown" a decision rather than a
    // parse failure.
    let probe: SchemaProbe =
        canonical::from_canonical_dagcbor(bytes).map_err(|e| ProtocolError::Malformed {
            reason: format!("no readable schema tag: {e}"),
        })?;
    if probe.schema != crate::definition::DEFINITION_SCHEMA_V1 {
        return Ok(Decoded::Unknown(RawRecord {
            schema: probe.schema,
            bytes: bytes.to_vec(),
        }));
    }
    let definition =
        canonical::from_canonical_dagcbor(bytes).map_err(|e| ProtocolError::Malformed {
            reason: format!(
                "record claims {} but does not parse as one: {e}",
                crate::definition::DEFINITION_SCHEMA_V1
            ),
        })?;
    Ok(Decoded::Known(definition))
}

/// One demand a surface could not meet, reported rather than swallowed.
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
    for demand in &definition.features {
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
