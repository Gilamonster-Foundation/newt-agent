//! Causal events over the existing derivation kernel.
//!
//! [`crate::Packet`] keeps its single-predecessor publication chain. An event
//! has a separate causal parent set, so a reply can name several antecedents
//! without changing what a packet's `prior` means. A store links the two in its
//! publication record. Elision events reference the existing [`Unit`] proof;
//! they do not mint a competing representation of a derivation.
//!
//! Derivation sources are an explicit subset of causal parents. Only those
//! sources determine generation depth: receiving a reply after a harness nudge
//! is a new external observation, while retrieving that nudge preserves its
//! harness origin and generation depth. Both construction and foreign decoding
//! resolve and verify every parent before admitting an immutable [`Event`].
//!
//! Admission establishes structure and provenance claims, not the truth of a
//! generated verdict or the authenticity of an external observation. The host
//! must own observation recording and must resolve roots, retain/check payload
//! bytes, and enforce session authorization. A resolver must expose only the
//! caller's authorized events; content identity alone grants no read authority.

use std::collections::{BTreeMap, BTreeSet};

use content_addressable::{ContentAddressable, ContentError, ContentId, MerkleNode, RawContentId};
use serde::{Deserialize, Serialize};

use crate::{verify_unit, Op, Unit, UnitId, Verified, VerifyError};

/// Who produced the material, independently of who later retrieved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventOrigin {
    /// The operator supplied the material.
    Operator,
    /// The primary model supplied the material.
    Model,
    /// A tool execution supplied the material.
    Tool,
    /// The harness supplied the material, including auxiliary model output.
    Harness,
}

/// The auxiliary classifier's claim about a recorded model reply.
///
/// Admission can check this closed vocabulary and its reply reference. It
/// cannot establish that the classifier's claim is true or the task is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyVerdict {
    /// The reply presents an answer or deliverable.
    Answer,
    /// The reply narrates intended work.
    Narration,
    /// The reply asks the operator a question.
    Question,
}

/// The event's operation; operation-specific references cannot be omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EventKind {
    /// External material recorded by the host, with no derivation sources.
    Observation,
    /// A generated harness instruction, rooted in the event's explicit root.
    Intervention,
    /// A classification of exactly one recorded model observation.
    Verdict {
        /// The classifier's proposed class.
        verdict: ReplyVerdict,
    },
    /// A read of one existing artifact, retaining its depth and origin.
    Retrieval,
    /// A pointer backed by the kernel's existing addressed elision proof.
    Elision {
        /// The admitted unit that addresses the removed source span.
        unit: UnitId,
    },
}

/// The addressed event body. Public fields are untrusted until admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventBody {
    /// The triggering [`crate::RootEvent`], resolved by the storage layer.
    pub root: ContentId,
    /// Origin of the material; retrieval and elision inherit it unchanged.
    pub origin: EventOrigin,
    /// The operation and any operation-specific claim.
    pub kind: EventKind,
    /// Retained event payload bytes, verified by their storage consumer.
    pub payload: RawContentId,
    /// Session-local occurrence ordinal, distinguishing identical observations.
    pub seq: u64,
    /// Derivation inputs, distinct from other causal antecedents.
    pub sources: BTreeSet<ContentId>,
    /// Claimed generation depth; admission recomputes it from resolved inputs.
    pub depth: u32,
}

/// Foreign bytes decode to this untrusted Merkle node before [`Event::admit`].
pub type RawEvent = MerkleNode<EventBody>;

/// An immutable event whose parent membership and derivation laws were checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Event(RawEvent);

/// Why an event or its elision proof was refused.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    /// An event link did not resolve in the caller's authorized graph.
    #[error("event reference is absent or unauthorized: {0}")]
    MissingReference(Box<ContentId>),
    /// A resolver returned an event filed under another event's address.
    #[error("event reference {expected} resolves to {actual}")]
    ReferenceMismatch {
        /// The requested content address.
        expected: Box<ContentId>,
        /// The returned event's recomputed address.
        actual: Box<ContentId>,
    },
    /// An origin, operation, or reference relationship violates the contract.
    #[error("invalid event: {0}")]
    Invalid(&'static str),
    /// The claimed depth disagrees with the resolved derivation inputs.
    #[error("event declares depth {declared}, but its derivation implies {implied}")]
    DepthMismatch {
        /// The depth supplied by the caller.
        declared: u32,
        /// The depth recomputed by admission.
        implied: u32,
    },
    /// Generating from an already generated artifact exceeds the depth bound.
    #[error("event generation depth {0} exceeds the bound of one")]
    DepthExceeded(u32),
    /// The addressing crate could not encode an event.
    #[error(transparent)]
    Content(#[from] Box<ContentError>),
    /// Existing kernel verification rejected the addressed source material.
    #[error(transparent)]
    Elision(#[from] VerifyError),
}

impl From<ContentError> for EventError {
    fn from(error: ContentError) -> Self {
        Self::Content(Box::new(error))
    }
}

impl Event {
    /// Construct an event through the same boundary used for foreign bytes.
    ///
    /// `resolve` supplies already admitted, authorized events. Root and payload
    /// resolution remain the storage layer's job; see the module's limits.
    ///
    /// # Errors
    ///
    /// Returns [`EventError`] for missing/substituted links or invalid derivation.
    pub fn new(
        body: EventBody,
        parents: impl IntoIterator<Item = ContentId>,
        resolve: impl Fn(&ContentId) -> Option<Event>,
    ) -> Result<Self, EventError> {
        Self::admit(MerkleNode::new(body, parents), resolve)
    }

    /// Admit a decoded event after resolving and re-addressing every parent.
    ///
    /// This checks one event against admitted parents, not an unbounded graph
    /// traversal. A durable reader must restore the closure in dependency order.
    /// An intervention with no derivation sources is generated from its explicit
    /// root at depth one; causal predecessors do not become its evidence.
    ///
    /// # Errors
    ///
    /// Returns [`EventError`] if links, origins, source relations, or depth fail.
    pub fn admit(
        raw: RawEvent,
        resolve: impl Fn(&ContentId) -> Option<Event>,
    ) -> Result<Self, EventError> {
        let body = raw.payload();
        if !body.sources.is_subset(raw.parents()) {
            return Err(EventError::Invalid(
                "derivation sources must also be causal parents",
            ));
        }
        let mut parents = BTreeMap::new();
        for id in raw.parents() {
            let parent = resolve(id).ok_or_else(|| EventError::MissingReference(Box::new(*id)))?;
            let actual = parent.id()?;
            if actual != *id {
                return Err(EventError::ReferenceMismatch {
                    expected: Box::new(*id),
                    actual: Box::new(actual),
                });
            }
            parents.insert(*id, parent);
        }
        let source_depth = body
            .sources
            .iter()
            .map(|id| parents[id].depth())
            .max()
            .unwrap_or(0);
        let implied = match &body.kind {
            EventKind::Observation => {
                if body.origin == EventOrigin::Harness || !body.sources.is_empty() {
                    return Err(EventError::Invalid(
                        "observations must be external and have no derivation sources",
                    ));
                }
                0
            }
            EventKind::Intervention => {
                require_harness(body)?;
                Op::Generate.depth_after(source_depth)
            }
            EventKind::Verdict { .. } => {
                require_harness(body)?;
                let source = single_source(body, &parents)?;
                if parents.len() != 1
                    || source.body().origin != EventOrigin::Model
                    || source.body().kind != EventKind::Observation
                {
                    return Err(EventError::Invalid(
                        "a verdict must name exactly one model reply observation as its parent",
                    ));
                }
                Op::Generate.depth_after(source_depth)
            }
            EventKind::Retrieval | EventKind::Elision { .. } => {
                let source = single_source(body, &parents)?;
                if body.origin != source.body().origin {
                    return Err(EventError::Invalid(
                        "retrieval and elision must preserve source origin",
                    ));
                }
                Op::Elide.depth_after(source_depth)
            }
        };
        if implied > 1 {
            return Err(EventError::DepthExceeded(implied));
        }
        if body.depth != implied {
            return Err(EventError::DepthMismatch {
                declared: body.depth,
                implied,
            });
        }
        Ok(Self(raw))
    }

    /// The event's admitted body, borrowed immutably.
    #[must_use]
    pub fn body(&self) -> &EventBody {
        self.0.payload()
    }

    /// Causal parents, separate from the publication packet's predecessor.
    #[must_use]
    pub fn parents(&self) -> &BTreeSet<ContentId> {
        self.0.parents()
    }

    /// The depth established by admission.
    #[must_use]
    pub fn depth(&self) -> u32 {
        self.body().depth
    }

    /// Address of the complete event body and causal parent set.
    ///
    /// # Errors
    ///
    /// Propagates canonical encoding errors.
    pub fn id(&self) -> Result<ContentId, ContentError> {
        self.0.id()
    }

    /// Clone the transparent encoding; modifications must pass admission again.
    #[must_use]
    pub fn to_raw(&self) -> RawEvent {
        self.0.clone()
    }

    /// Verify an elision's existing unit proof against its linked source event.
    ///
    /// The unit proves the addressed byte span. The event adds inherited origin
    /// and depth, which v0's byte-only unit cannot determine. This checks neither
    /// the pointer's rendering nor the root's authority; the consumer owns both.
    ///
    /// # Errors
    ///
    /// Refuses mismatched unit/source/root links, then propagates [`verify_unit`].
    pub fn verify_elision(
        &self,
        unit: &Unit,
        source: &Event,
        bytes: &[u8],
    ) -> Result<Verified, EventError> {
        let EventKind::Elision { unit: expected } = &self.body().kind else {
            return Err(EventError::Invalid("event is not an elision"));
        };
        if *expected != unit.id()?
            || unit.root() != self.body().root
            || !self.body().sources.contains(&source.id()?)
            || source.body().payload != unit.derivation().source
        {
            return Err(EventError::Invalid(
                "elision unit, source, or root does not match the event",
            ));
        }
        Ok(verify_unit(unit, bytes)?)
    }
}

fn require_harness(body: &EventBody) -> Result<(), EventError> {
    if body.origin != EventOrigin::Harness {
        return Err(EventError::Invalid(
            "generated interventions and verdicts must have harness origin",
        ));
    }
    Ok(())
}

fn single_source<'a>(
    body: &EventBody,
    parents: &'a BTreeMap<ContentId, Event>,
) -> Result<&'a Event, EventError> {
    if body.sources.len() != 1 {
        return Err(EventError::Invalid(
            "operation requires exactly one derivation source",
        ));
    }
    Ok(&parents[body.sources.first().expect("source cardinality checked")])
}

impl ContentAddressable for Event {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        self.0.canonical_form()
    }
}
