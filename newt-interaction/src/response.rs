//! The typed response: what a responder submitted, bound to exactly what
//! they were offered.

use content_addressable::{canonical, ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::ids::{ControlId, DefinitionId, IdempotencyKey, InstanceId, ResponseId, Revision};
use crate::instance::Audience;

/// The versioned type tag every response carries.
pub const RESPONSE_SCHEMA_V1: &str = "newt.interaction.response/v1";

/// A reference to a secret the host holds — never the secret.
///
/// A response is durable, content-addressed, and tamper-evident, which is
/// exactly what makes a secret inside one a permanent disclosure
/// liability. The record names the sealed value; resolving the handle is
/// the host's business and requires its own authority (ADR D1: never
/// persist secret values in markup or logs).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    /// Adopt a handle the host can resolve.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidId`] when empty — a reference that
    /// references nothing is not a reference.
    pub fn new(handle: impl Into<String>) -> Result<Self, ProtocolError> {
        let handle = handle.into();
        if handle.is_empty() {
            return Err(ProtocolError::InvalidId {
                kind: "secret reference",
                reason: "must not be empty".to_string(),
            });
        }
        Ok(Self(handle))
    }

    /// The handle as adopted.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What was submitted for one control, typed.
///
/// Typed at the TYPE rather than at a downstream parser: a `String` here
/// would make every consumer decide whether `"TRUE"`, `"1"`, and `"yes"`
/// are the same toggle, which is the per-surface drift the epic exists to
/// end. Canonicalization lives in the shape, so A3 revalidates rather than
/// re-parses.
/// Deliberately NOT `#[non_exhaustive]`, unlike the open vocabularies in
/// this crate (`InteractionKind`, `SemanticRole`, `Audience`, …). This
/// variant set is a security boundary: "no variant can carry a secret" is
/// provable only by an exhaustive match, and `#[non_exhaustive]` would
/// force every such match to end in a wildcard — after which adding a
/// plaintext-carrying variant would compile silently everywhere. Adding a
/// variant here SHOULD break every consumer that reasons about the set.
/// Every variant is a STRUCT variant: serde's internally-tagged form
/// cannot represent a newtype variant wrapping a primitive, and the named
/// field is the clearer thing to freeze into A2.1's vectors anyway
/// (`{"kind":"toggle","on":true}`).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ControlValue {
    /// One of the definition's controls was chosen.
    Choice {
        /// Which control. Its charset is enforced at construction, so a
        /// choice can never carry a sentence.
        option: ControlId,
    },
    /// Free text, as typed.
    Text {
        /// The submitted text.
        text: String,
    },
    /// A boolean. Travels as a bool, never as text.
    Toggle {
        /// Whether the control is on.
        on: bool,
    },
    /// A secret, BY REFERENCE. There is deliberately no variant that can
    /// hold plaintext.
    Secret {
        /// The handle the host can resolve.
        reference: SecretRef,
    },
}

/// How a responder was authenticated, IDENTIFIED — never the credential
/// itself.
///
/// A2 gives responder provenance a place to live and binds it into the
/// response's identity; A3 authenticates it and revalidates it. The
/// separation matters: an [`Audience`] says which KIND of surface answered,
/// which is not who did.
///
/// **This record carries no credentials and no secrets.** It names the
/// assertion by reference — a kind, a subject, and an opaque
/// [`assertion`](ResponderProvenance::assertion) handle the host can
/// resolve — so a durable, content-addressed response never becomes a
/// disclosure liability. A digest in the record beats the bytes: it is
/// tamper-evident without being a secret.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponderProvenance {
    /// What kind of assertion authenticated this responder.
    pub kind: AssertionKind,
    /// The principal the assertion speaks for, as the host names it. A
    /// stable reference, never a credential.
    pub subject: String,
    /// Which audience the assertion was presented from.
    pub audience: Audience,
    /// An opaque handle the host can resolve to the assertion itself — a
    /// content id, a row id, or a session reference. `None` when the
    /// responder was unauthenticated, which is a fact worth recording
    /// rather than hiding: A3 decides whether that is admissible.
    pub assertion: Option<String>,
}

/// What established the responder's identity.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AssertionKind {
    /// The operator at the terminal that owns the session; authority comes
    /// from holding the terminal, not from a presented token.
    TerminalOperator,
    /// A signed assertion from an enrolled credential.
    SignedAssertion,
    /// No assertion was presented. Recorded, not hidden.
    Unauthenticated,
}

/// One control's answer: which control, and the typed value.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    /// Which control this answers.
    pub control: ControlId,
    /// What was submitted.
    pub value: ControlValue,
}

/// A submission bound to the exact offer it answers.
///
/// Binds type, definition, instance, digest, revision, control values,
/// idempotency key, and responder provenance — the ADR's own list. Nothing
/// here authorizes on its own: the controller revalidates every field
/// against the definition before it resolves anything.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// The submitted values, each against the control it answers.
    pub values: Vec<Submission>,
    /// Makes a retry the same submission rather than a second one.
    pub idempotency_key: IdempotencyKey,
    /// How the responder was authenticated, by reference — including which
    /// audience they answered from. Bound into the response's identity, so
    /// a submission cannot be re-attributed after the fact.
    ///
    /// There is deliberately no second `responder: Audience` field beside
    /// this one. Two bound fields carrying the same fact can disagree, and
    /// nothing in a plain record keeps them equal; the identity table would
    /// then be pinning a contradiction. The audience a responder was
    /// authenticated FROM is part of the assertion, so it lives with it.
    pub responder_provenance: ResponderProvenance,
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
