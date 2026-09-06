//! **The host-minted lifecycle** (A3, #1837).
//!
//! State is out of band (ADR laws 8 and 12): [`InteractionInstance`] names
//! the OFFER and never its state, so the state machine lives here and
//! references the offer by its stable [`InstanceId`].
//!
//! Two properties are load bearing, and both are about what this module
//! REFUSES to do.
//!
//! **Untrusted markup cannot publish** (law 11). Publication requires a
//! [`HostMint`] — a token with a private field, no `Deserialize`, no
//! `Default`, and no `Clone`. A document is DATA: decoding produces
//! records, and no sequence of decodes produces a `HostMint`. The runtime
//! half is checked too: publication verifies that the offer's digest and
//! revision match the definition the HOST holds, so an instance record
//! naming a definition the host never presented is refused rather than
//! taken on its own word.
//!
//! **Expiry synthesizes no response.** [`SemanticRole`] and
//! [`ChoiceOption::role`] are AUTHOR-assigned
//! (`definition.rs`, `ids.rs`), and the author may be untrusted markup. A
//! controller that computed its expiry default by scanning the definition
//! for `role == Deny` would make an attacker-chosen field decide what
//! "fail closed" means — and a mislabelled option (`label: "deny"`,
//! `role: Allow`) would become a path to authorization that the old
//! fixed-enum code structurally could not reach. So expiry is a pure
//! no-decision transition: it moves the state to
//! [`LifecycleState::Expired`] and produces no [`Response`] at all. There
//! is deliberately no API here that returns one.
//!
//! [`Response`]: crate::Response
//! [`SemanticRole`]: crate::SemanticRole
//! [`ChoiceOption::role`]: crate::ChoiceOption::role

use thiserror::Error;

use crate::definition::InteractionDefinition;
use crate::error::ProtocolError;
use crate::ids::{InstanceId, Revision};
use crate::instance::{InteractionInstance, LifecycleState};

/// The host's authority to publish an offer.
///
/// Deliberately not `Deserialize`, `Default`, `Clone`, or `Copy`: the
/// point is that no decoded document, and no value threaded out of one,
/// can produce it. Only host code that already holds the authority to
/// offer an interaction constructs one.
#[derive(Debug)]
pub struct HostMint {
    /// Private, so the struct-literal form is unavailable outside this
    /// module and [`HostMint::assert_host_authority`] is the only door.
    _private: (),
}

impl HostMint {
    /// Assert that the caller is the host.
    ///
    /// Name it what it is: a claim by the calling code, checked by the
    /// dependency direction rather than at runtime. `newt-interaction`
    /// has no ambient authority to verify it against, and inventing a
    /// runtime check here would be theatre.
    #[must_use]
    pub fn assert_host_authority() -> Self {
        Self { _private: () }
    }
}

/// Why a lifecycle transition was refused.
#[derive(Debug, Error)]
pub enum LifecycleError {
    /// The offer names a different definition than the host presented.
    #[error("the offer binds definition `{offered}`, but the host presented `{presented}`")]
    DefinitionMismatch {
        /// The digest the instance record binds.
        offered: String,
        /// The digest of the definition the host actually holds.
        presented: String,
    },
    /// The offer names a different revision than the definition carries.
    #[error("the offer binds revision {offered}, but the definition is at {presented}")]
    RevisionMismatch {
        /// The revision the instance record binds.
        offered: u64,
        /// The revision the definition carries.
        presented: u64,
    },
    /// A terminal state accepts no further transition.
    #[error("{state:?} is terminal; no further transition is possible")]
    AlreadyTerminal {
        /// The terminal state the lifecycle is already in.
        state: LifecycleState,
    },
    /// The transition is not one this machine allows.
    #[error("cannot move from {from:?} to {to:?}")]
    IllegalTransition {
        /// Where the lifecycle is.
        from: LifecycleState,
        /// Where the caller tried to move it.
        to: LifecycleState,
    },
    /// Content addressing failed while checking the binding.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Where one offer is in its life.
///
/// Cheap and `Clone`, because it is a fact ABOUT an offer rather than the
/// offer itself. Constructing one in [`LifecycleState::Published`] is
/// possible only through [`publish`], which is what makes "host-minted"
/// mean something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    instance: InstanceId,
    state: LifecycleState,
}

impl Lifecycle {
    /// Which offer this is the state of.
    #[must_use]
    pub fn instance(&self) -> &InstanceId {
        &self.instance
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> LifecycleState {
        self.state
    }

    /// Whether this state accepts any further transition.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        terminal(self.state)
    }

    /// Move to `to`, refusing anything the machine does not allow.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::AlreadyTerminal`] from any terminal state, and
    /// [`LifecycleError::IllegalTransition`] for a move this machine does
    /// not define.
    pub fn transition(&self, to: LifecycleState) -> Result<Self, LifecycleError> {
        if terminal(self.state) {
            return Err(LifecycleError::AlreadyTerminal { state: self.state });
        }
        // From a non-terminal state, only a move to a TERMINAL state is
        // defined. Draft -> Published is `publish`'s job and needs the
        // host mint, so it is not reachable through here.
        if !terminal(to) {
            return Err(LifecycleError::IllegalTransition {
                from: self.state,
                to,
            });
        }
        Ok(Self {
            instance: self.instance,
            state: to,
        })
    }

    /// Expire this offer.
    ///
    /// **Synthesizes no [`Response`](crate::Response) and authorizes
    /// nothing.** The return type carries a state and an id, and there is
    /// no field or method through which a decision could arrive — which
    /// is the point: an expiry default read out of an author-supplied
    /// `role` would let a document decide what failing closed means.
    ///
    /// # Errors
    ///
    /// [`LifecycleError::AlreadyTerminal`] if it has already resolved.
    pub fn expire(&self) -> Result<Self, LifecycleError> {
        self.transition(LifecycleState::Expired)
    }

    /// Whether this offer's TTL has elapsed at `now_tick`.
    ///
    /// A pure comparison against the offer's own minting tick and TTL. It
    /// reads no clock: `newt-interaction` has no ambient authority, and
    /// the caller's clock is the one that matters.
    #[must_use]
    pub fn has_elapsed(instance: &InteractionInstance, now_tick: i64) -> bool {
        let deadline = instance
            .provenance
            .minted_tick
            .saturating_add(instance.ttl_ticks);
        now_tick >= deadline
    }
}

/// Is this state terminal?
///
/// Exhaustive on purpose — no `_` arm. [`LifecycleState`] is
/// `#[non_exhaustive]` to its consumers, but this crate DEFINES it, so a
/// new variant must be classified here before it compiles.
fn terminal(state: LifecycleState) -> bool {
    match state {
        LifecycleState::Draft | LifecycleState::Published => false,
        LifecycleState::Answered
        | LifecycleState::Cancelled
        | LifecycleState::Expired
        | LifecycleState::Unsupported => true,
    }
}

/// Publish a host-minted offer of `definition`.
///
/// Requires a [`HostMint`], and verifies the binding the offer claims:
/// the definition digest and the revision must match the definition the
/// host is holding. An instance record that names a definition the host
/// never presented is refused rather than believed.
///
/// # Errors
///
/// [`LifecycleError::DefinitionMismatch`] or
/// [`LifecycleError::RevisionMismatch`] when the offer does not bind the
/// presented definition; [`LifecycleError::Protocol`] if addressing fails.
pub fn publish(
    _mint: &HostMint,
    instance: &InteractionInstance,
    definition: &InteractionDefinition,
) -> Result<Lifecycle, LifecycleError> {
    let presented = definition.definition_id()?;
    if instance.definition != presented {
        return Err(LifecycleError::DefinitionMismatch {
            offered: instance.definition.to_string(),
            presented: presented.to_string(),
        });
    }
    if instance.revision != definition.revision {
        return Err(LifecycleError::RevisionMismatch {
            offered: instance.revision.get(),
            presented: definition.revision.get(),
        });
    }
    Ok(Lifecycle {
        instance: instance.instance_id()?,
        state: LifecycleState::Published,
    })
}

/// The revision an offer must bind to be publishable against `definition`.
///
/// Exposed so a host can mint a correct offer rather than discovering the
/// mismatch at publication.
#[must_use]
// INERT-CODE-RATCHET: X16 DELETE: publishable-revision helper has zero consumers and only exposes an existing field.
pub fn publishable_revision(definition: &InteractionDefinition) -> Revision {
    definition.revision
}
