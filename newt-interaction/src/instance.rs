//! The host-minted offer: the binding of a definition to an audience,
//! kept OUT of the definition so definition bytes stay immutable (ADR law
//! 12).
//!
//! **`InstanceId` is the identity of the OFFER, never of its state.** The
//! record binds what the offer IS — which definition, at which revision, to
//! whom, under which fence, until when, from where — and nothing that
//! changes as the interaction proceeds. Lifecycle, progress, responses, and
//! resolution are out of band (ADR law 8): A3's transition records carry
//! them and reference this stable id. An id that moved when the state moved
//! would be a snapshot id, and every response binding the earlier snapshot
//! would dangle.

use content_addressable::{canonical, ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;
use crate::ids::{DefinitionId, InstanceId, Nonce, Revision};

/// The versioned type tag every instance carries.
pub const INSTANCE_SCHEMA_V1: &str = "newt.interaction.instance/v1";

/// Where an instance is in its life.
///
/// A2 defines the STATES; A3 drives the transitions. Deliberately NOT a
/// field of [`InteractionInstance`]: state is out of band (ADR laws 8 and
/// 12), so A3's transition records carry it and reference the offer by its
/// stable [`InstanceId`](crate::InstanceId). Putting it inside the
/// content-addressed record would make that id name a snapshot — a
/// Published X would become an Answered Y, and a response that bound X
/// would refer to nothing the store still holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum LifecycleState {
    /// Minted, not yet offered.
    Draft,
    /// Offered to eligible responders.
    Published,
    /// Resolved by a valid response.
    Answered,
    /// Backed out without a decision.
    Cancelled,
    /// Its TTL elapsed. Expiry never authorizes.
    Expired,
    /// No attached surface can present a required control, so it fails
    /// closed rather than guessing (ADR law 5).
    Unsupported,
}

/// Who may answer, as distinct from what a surface can draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Audience {
    /// The operator at the terminal that owns this session.
    Terminal,
    /// An authenticated, in-scope web attachment.
    Web,
}

/// The frozen eligibility rule for one instance.
///
/// Frozen at publication: an offer cannot widen who may answer it after the
/// fact, which is the "attenuate, never amplify" law applied to responders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponderPolicy {
    /// Audiences this offer is open to.
    pub audiences: Vec<Audience>,
    /// Whether the responder must present an authenticated assertion.
    pub requires_assertion: bool,
}

/// The workspace fence an instance is confined to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Opaque workspace key — the same fence every permission query already
    /// applies in `newt_core`'s store.
    pub workspace_key: String,
    /// The conversation this instance belongs to.
    pub conversation_id: String,
}

/// Where an instance came from — the record's own sources.
///
/// Mandatory, not optional. The repo's one outstanding first-principle
/// violation is exactly this hole in `ConversationTurn`
/// (`first_principle.rs:802`); a new record type that cannot name its origin
/// repeats the defect the ledger counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// What minted this instance (a tool name, a subsystem, a persona).
    pub origin: String,
    /// The tick at which it was minted, as the host counts ticks.
    pub minted_tick: i64,
}

/// A live offer of one definition to one audience.
///
/// Binds the routing nonce to the definition's content id, the revision, the
/// TTL, the scope, the frozen responder policy, and the provenance — and the
/// whole binding is itself content-addressed, so an offer cannot be altered
/// without changing its [`InstanceId`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionInstance {
    /// Versioned type tag; see [`INSTANCE_SCHEMA_V1`].
    pub schema: String,
    /// Fresh, unguessable routing handle. Not the identity.
    pub nonce: Nonce,
    /// The definition being offered — its exact form digest.
    pub definition: DefinitionId,
    /// The revision offered.
    pub revision: Revision,
    /// Ticks after `provenance.minted_tick` at which this offer expires.
    pub ttl_ticks: i64,
    /// The workspace fence.
    pub scope: Scope,
    /// Frozen eligibility.
    pub responder_policy: ResponderPolicy,
    /// Where this offer came from.
    pub provenance: Provenance,
}

impl InteractionInstance {
    /// This instance's identity.
    ///
    /// # Errors
    ///
    /// Propagates a canonical-encoding failure.
    pub fn instance_id(&self) -> Result<InstanceId, ProtocolError> {
        Ok(InstanceId::from_content_id(self.content_id()?))
    }

    /// Refuse an unknown schema tag; unknown required behavior fails closed.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::UnknownSchema`] when the tag is not
    /// [`INSTANCE_SCHEMA_V1`].
    pub fn ensure_known_schema(&self) -> Result<(), ProtocolError> {
        if self.schema != INSTANCE_SCHEMA_V1 {
            return Err(ProtocolError::UnknownSchema {
                tag: self.schema.clone(),
                expected: INSTANCE_SCHEMA_V1,
            });
        }
        Ok(())
    }
}

impl ContentAddressable for InteractionInstance {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        // The whole binding is the identity: nonce, definition, revision,
        // TTL, scope, responder policy, provenance, and lifecycle. An offer
        // whose fence or expiry could change without changing its id is an
        // offer that can be silently re-aimed.
        canonical::to_canonical_dagcbor(self)
    }
}
