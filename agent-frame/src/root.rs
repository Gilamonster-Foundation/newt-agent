//! Roots: **which** event caused a derivation, not merely what kind it was.
//!
//! An earlier revision of this kernel made `Root` an enum of three kinds and
//! stored the kind on the unit. That cannot answer the question a forensic
//! consumer actually asks — *which operator turn caused this?* — because every
//! turn of the same kind is the same value. A kind is a category; provenance
//! needs an address.
//!
//! So the kind stays, demoted to a field of a [`RootEvent`] that has its own
//! [`ContentId`], and a unit references that id. "Caused by an operator prompt"
//! becomes "caused by *this* operator prompt", and the difference is checkable.

use content_addressable::{ContentAddressable, ContentError, ContentId, RawContentId};
use serde::{Deserialize, Serialize};

/// The kind of thing that rooted a derivation.
///
/// Note what is absent: there is **no** `ModelOutput` constructor. A model's
/// assertion is never a provenance root, and that is enforced by the type
/// rather than by a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    /// The human SAID: intent expressed in language. Needs adjudication.
    OperatorPrompt,
    /// The human DID: intent expressed as control. Unambiguous by construction.
    UserAction,
    /// The machine decided: a budget threshold, a retry, an automatic seal.
    HarnessEvent,
}

/// A specific rooting event, addressed by its own content.
///
/// # Why `seq`
///
/// An operator can type "run the tests" twice in one session. Those are two
/// turns, and a unit rooted in the second must not be indistinguishable from
/// one rooted in the first. Content alone collapses them; `seq` is what keeps
/// "which turn" answerable. It is the session-local ordinal of the event, so it
/// is deterministic — a replay of the same session mints the same ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootEvent {
    /// What kind of event this was.
    pub kind: RootKind,
    /// The event's own material — the prompt bytes, the action record, the
    /// harness event record. A [`RawContentId`] because this is an opaque byte
    /// string, not a canonical structured value.
    pub content: RawContentId,
    /// Session-local ordinal, so two identical events remain distinct.
    pub seq: u64,
}

impl RootEvent {
    /// Address a rooting event by hashing its material.
    #[must_use]
    pub fn new(kind: RootKind, material: &[u8], seq: u64) -> Self {
        Self {
            kind,
            content: RawContentId::from_content(material),
            seq,
        }
    }

    /// This event's address — what a [`crate::Unit`] references.
    ///
    /// # Errors
    ///
    /// Propagates an encoding failure from the canonical form.
    pub fn id(&self) -> Result<ContentId, ContentError> {
        self.content_id()
    }
}

impl ContentAddressable for RootEvent {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}
