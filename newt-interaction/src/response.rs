//! The typed response: what a responder submitted, bound to exactly what
//! they were offered.

use content_addressable::{canonical, ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::ids::{ControlId, DefinitionId, IdempotencyKey, InstanceId, ResponseId, Revision};
use crate::instance::Audience;

/// The versioned type tag every response carries.
pub const RESPONSE_SCHEMA_V1: &str = "newt.interaction.response/v1";

/// One control's submitted value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlValue {
    /// Which control this answers.
    pub control: ControlId,
    /// What was submitted. A secret control's value never reaches a durable
    /// record; the controller substitutes a handle (ADR D1).
    pub value: String,
}

/// A submission bound to the exact offer it answers.
///
/// Binds type, definition, instance, digest, revision, control values,
/// idempotency key, and responder provenance — the ADR's own list. Nothing
/// here authorizes on its own: the controller revalidates every field
/// against the definition before it resolves anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// Versioned type tag; see [`RESPONSE_SCHEMA_V1`].
    pub schema: String,
    /// The definition answered — and, being a content id, its exact form
    /// digest. A definition that changed by one byte cannot be answered by
    /// a response minted against the old one.
    pub definition: DefinitionId,
    /// The instance answered.
    pub instance: InstanceId,
    /// The revision the responder saw.
    pub revision: Revision,
    /// The submitted values.
    pub values: Vec<ControlValue>,
    /// Makes a retry the same submission rather than a second one.
    pub idempotency_key: IdempotencyKey,
    /// Which audience the responder answered from.
    pub responder: Audience,
}

impl Response {
    /// This response's identity.
    ///
    /// # Errors
    ///
    /// Propagates a canonical-encoding failure.
    pub fn response_id(&self) -> Result<ResponseId, ProtocolError> {
        Ok(ResponseId::from_content_id(self.content_id()?))
    }

    /// Refuse an unknown schema tag; unknown required behavior fails closed.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::UnknownSchema`] when the tag is not
    /// [`RESPONSE_SCHEMA_V1`].
    pub fn ensure_known_schema(&self) -> Result<(), ProtocolError> {
        if self.schema != RESPONSE_SCHEMA_V1 {
            return Err(ProtocolError::UnknownSchema {
                tag: self.schema.clone(),
                expected: RESPONSE_SCHEMA_V1,
            });
        }
        Ok(())
    }
}

impl ContentAddressable for Response {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        // The whole submission is the identity — the ADR's list in full:
        // type, definition, instance, digest, revision, control values,
        // idempotency key, and responder provenance.
        canonical::to_canonical_dagcbor(self)
    }
}
