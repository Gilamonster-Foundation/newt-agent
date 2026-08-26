//! The immutable, semantic definition of one interaction.

use content_addressable::{canonical, ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::ids::{ControlId, DefinitionId, Revision};

/// The versioned type tag every definition carries as its first field.
///
/// Bound into identity, so bumping it re-addresses every definition — the
/// property `newt_core::agentic::content_spill`'s `SPILL_SCHEMA_V1` already
/// relies on (`content_spill.rs:45-47`).
pub const DEFINITION_SCHEMA_V1: &str = "newt.interaction.definition/v1";

/// What kind of interaction this is. The kind is semantic, never a renderer
/// choice: `modal` is a view decision (ADR C1), not a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum InteractionKind {
    /// Pick one of a fixed, displayed set — today's `Question<A>`.
    Choice,
    /// Free text.
    Prompt,
    /// A yes/no decision.
    Confirm,
    /// A multi-field form.
    Form,
    /// A notice that carries no controls and expects no response.
    Notice,
}

/// What a control MEANS, independent of how any surface draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SemanticRole {
    /// Grants the thing being asked about.
    Allow,
    /// Refuses it. The fail-closed default for an absent answer.
    Deny,
    /// Backs out without deciding.
    Cancel,
    /// Ends the session.
    Exit,
    /// Supplies a value rather than a decision.
    Value,
}

/// How a control accepts input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ControlKind {
    /// One option from the definition's displayed set.
    Choice,
    /// Free text.
    Text,
    /// A boolean.
    Toggle,
    /// A secret: never persisted in markup or logs (ADR D1).
    Secret,
}

/// One control: a stable id, what it means, and how it takes input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Control {
    /// Stable within the definition; the definition commits to it.
    pub id: ControlId,
    /// What answering with this control means.
    pub role: SemanticRole,
    /// How it accepts input.
    pub kind: ControlKind,
    /// Human-readable label. Labels never confer authority (ADR law 2).
    pub label: String,
    /// Whether a response must include this control.
    pub required: bool,
}

/// What a surface can DO, as distinct from who may answer.
///
/// ADR law 4 requires these be separate types from responder eligibility:
/// conflating "this surface can render a secret field" with "this responder
/// may grant permanently" is exactly the authority laundering the epic's
/// risk table names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SurfaceFeatures {
    /// The surface can accept secret input without echoing it.
    pub secret_input: bool,
    /// The surface can render a diagram extension (E0).
    pub diagrams: bool,
    /// The surface can present more than one control at once.
    pub multi_control: bool,
}

/// The immutable semantic model of one interaction.
///
/// Identity is a [`DefinitionId`] — a `ContentId` over the canonical
/// encoding of this whole record, which is also its **exact form digest**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionDefinition {
    /// Versioned type tag; see [`DEFINITION_SCHEMA_V1`].
    pub schema: String,
    /// What kind of interaction this is.
    pub kind: InteractionKind,
    /// Which revision of this definition's authoring lineage this is.
    pub revision: Revision,
    /// The readable body — canonical Markdown, the fallback every surface
    /// can render (ADR law 1). Held as text; parsing belongs to
    /// `newt_core::markup`, not to the protocol layer.
    pub markdown: String,
    /// The controls, in presentation order.
    pub controls: Vec<Control>,
    /// What a surface must be able to do to present this faithfully.
    pub features: SurfaceFeatures,
}

impl InteractionDefinition {
    /// Build a definition carrying the current schema tag.
    #[must_use]
    pub fn new(kind: InteractionKind, markdown: impl Into<String>, controls: Vec<Control>) -> Self {
        Self {
            schema: DEFINITION_SCHEMA_V1.to_string(),
            kind,
            revision: Revision::FIRST,
            markdown: markdown.into(),
            controls,
            features: SurfaceFeatures::default(),
        }
    }

    /// This definition's identity, which is also its exact form digest.
    ///
    /// # Errors
    ///
    /// Propagates a canonical-encoding failure.
    pub fn definition_id(&self) -> Result<DefinitionId, ProtocolError> {
        Ok(DefinitionId::from_content_id(self.content_id()?))
    }

    /// Refuse a record whose schema tag this build does not know. Unknown
    /// REQUIRED behavior fails closed (ADR law 5).
    ///
    /// # Errors
    ///
    /// [`ProtocolError::UnknownSchema`] when the tag is not
    /// [`DEFINITION_SCHEMA_V1`].
    pub fn ensure_known_schema(&self) -> Result<(), ProtocolError> {
        if self.schema != DEFINITION_SCHEMA_V1 {
            return Err(ProtocolError::UnknownSchema {
                tag: self.schema.clone(),
                expected: DEFINITION_SCHEMA_V1,
            });
        }
        Ok(())
    }
}

impl ContentAddressable for InteractionDefinition {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        // The canonical form covers the WHOLE record — schema tag, kind,
        // revision, markdown, controls, and surface features. A form over a
        // hand-picked subset would let two definitions that differ in a
        // skipped field share one id, and an "exact form digest" that is not
        // exact is worse than none.
        canonical::to_canonical_dagcbor(self)
    }
}
