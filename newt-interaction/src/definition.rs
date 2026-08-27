//! The immutable, semantic definition of one interaction.

use content_addressable::{canonical, ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::ids::{ControlId, DefinitionId, OptionId, Revision};

/// The versioned type tag every definition carries as its first field.
///
/// Bound into identity, so bumping it re-addresses every definition — the
/// property `newt_core::agentic::content_spill`'s `SPILL_SCHEMA_V1` already
/// relies on (`content_spill.rs:45-47`).
pub const DEFINITION_SCHEMA_V1: &str = "newt.interaction.definition/v1";

/// What kind of interaction this is. The kind is semantic, never a renderer
/// choice: `modal` is a view decision (ADR C1), not a kind.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

/// Whether something is REQUIRED to present or answer an interaction
/// faithfully, or merely preferred.
///
/// This is the axis ADR law 5 turns on: *unknown REQUIRED behavior fails
/// closed; unknown OPTIONAL behavior degrades visibly.* Without it a
/// consumer that cannot satisfy a demand has only one move for both cases,
/// and whichever it picks is wrong half the time — silently dropping a
/// mandatory secret field, or refusing a document that merely wanted a
/// diagram.
///
/// One vocabulary, deliberately: controls and surface features both use
/// it, so a wire consumer learns the distinction once. On a control it
/// means "the response must answer this FIELD" — which is only coherent
/// because a choice is one field with many options, not many fields.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Requirement {
    /// Must be satisfiable. If it is not, the interaction is
    /// `Unsupported` and no answer may be guessed.
    Required,
    /// Preferred. If it is not satisfiable, the interaction proceeds and
    /// the shortfall is reported visibly.
    Optional,
}

/// One mutually-exclusive option of a choice control.
///
/// The option carries the SemanticRole, not the control: a permission
/// question has no single meaning — each of its options does. `allow once`
/// means Allow, `deny (default)` means Deny, and they are options of one
/// question rather than two questions.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceOption {
    /// Stable within the control; the definition commits to it.
    pub id: OptionId,
    /// What picking this option means.
    pub role: SemanticRole,
    /// Human-readable label. Labels never confer authority (ADR law 2).
    pub label: String,
}

/// How a control accepts input.
///
/// `Choice` carries its options inline because they belong to it: one
/// field, N mutually-exclusive answers. Modelling each option as its own
/// control would make [`Requirement`] incoherent — under "the response
/// must include this control", answering an allow would require also
/// answering the deny.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControlKind {
    /// Pick exactly one of the options offered.
    Choice {
        /// The options, in presentation order.
        options: Vec<ChoiceOption>,
    },
    /// Free text.
    Text,
    /// A boolean.
    Toggle,
    /// A secret: never persisted in markup or logs (ADR D1).
    Secret,
}

/// One control: a stable id, what it means, and how it takes input.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    /// Stable within the definition; the definition commits to it.
    pub id: ControlId,
    /// How it accepts input, and — for a choice — what it offers.
    pub kind: ControlKind,
    /// Human-readable label. Labels never confer authority (ADR law 2).
    pub label: String,
    /// Whether a response must include this control.
    pub requirement: Requirement,
}

/// A capability a surface must have to present some part of an
/// interaction: accepting secret input without echoing it, rendering a
/// diagram, showing more than one control at once.
///
/// A named string rather than a closed enum, on purpose. This is a WIRE
/// vocabulary: a v2 document may demand a feature this build has never
/// heard of, and a closed enum could not even represent it — the name
/// would be lost at parse time and law 5 would have nothing left to act
/// on. Carrying the name verbatim is what lets an unknown demand be
/// refused (when required) or reported (when optional) instead of
/// silently vanishing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
// See the note on `ControlId`: deserialization goes through the
// constructor. An UNRECOGNIZED feature name is still valid — that is the
// forward-compatibility case — but an EMPTY one is not.
#[serde(into = "String", try_from = "String")]
pub struct SurfaceFeature(String);

#[cfg(feature = "schema")]
crate::string_scalar_schema!(SurfaceFeature, None::<&str>);

impl From<SurfaceFeature> for String {
    fn from(value: SurfaceFeature) -> Self {
        value.0
    }
}

impl TryFrom<String> for SurfaceFeature {
    type Error = ProtocolError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl SurfaceFeature {
    /// The surface accepts secret input without echoing or persisting it.
    pub const SECRET_INPUT: &'static str = "secret-input";
    /// The surface renders a diagram extension (epic slice E0).
    pub const DIAGRAMS: &'static str = "diagrams";
    /// The surface can present more than one control at once.
    pub const MULTI_CONTROL: &'static str = "multi-control";

    /// Every feature name this build understands.
    pub const KNOWN: &'static [&'static str] =
        &[Self::SECRET_INPUT, Self::DIAGRAMS, Self::MULTI_CONTROL];

    /// Adopt a feature name.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidId`] when empty. An UNRECOGNIZED name is
    /// not an error — that is the forward-compatibility case, and
    /// [`is_known`](Self::is_known) is how a consumer asks.
    pub fn new(name: impl Into<String>) -> Result<Self, ProtocolError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ProtocolError::InvalidId {
                kind: "surface feature",
                reason: "must not be empty".to_string(),
            });
        }
        Ok(Self(name))
    }

    /// The name as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this build understands the feature.
    #[must_use]
    pub fn is_known(&self) -> bool {
        Self::KNOWN.contains(&self.0.as_str())
    }
}

/// A definition's demand for one surface feature, and how hard a demand it
/// is.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureDemand {
    /// Which capability.
    pub feature: SurfaceFeature,
    /// Required, or merely preferred.
    pub requirement: Requirement,
}

/// The immutable semantic model of one interaction.
///
/// Identity is a [`DefinitionId`] — a `ContentId` over the canonical
/// encoding of this whole record, which is also its **exact form digest**.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionDefinition {
    /// Versioned type tag; see [`DEFINITION_SCHEMA_V1`].
    pub schema: crate::tag::DefinitionTag,
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
    /// What a surface must be able to do to present this faithfully, each
    /// demand marked required or optional.
    pub features: Vec<FeatureDemand>,
}

impl InteractionDefinition {
    /// Build a definition carrying the current schema tag.
    #[must_use]
    pub fn new(kind: InteractionKind, markdown: impl Into<String>, controls: Vec<Control>) -> Self {
        Self {
            schema: crate::tag::DefinitionTag,
            kind,
            revision: Revision::FIRST,
            markdown: markdown.into(),
            controls,
            features: Vec::new(),
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

    /// The schema tag, as a string.
    ///
    /// Reading it needs no check: the tag is a TYPE that deserializes from
    /// exactly one value, so a record of this type cannot carry any other
    /// one. What used to be a runtime `ensure_known_schema` is now a thing
    /// the wire cannot express — and the published schema says so with a
    /// `const`, so a foreign implementor validating against the wrong
    /// record's schema fails instead of passing.
    #[must_use]
    pub fn schema_tag(&self) -> &'static str {
        self.schema.as_str()
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
