//! Units: what a derived thing is, what it owes its source, and what it takes
//! to get one into the system.
//!
//! # The admission boundary
//!
//! [`Unit`] deliberately does **not** derive [`serde::Deserialize`]. An earlier
//! revision did, and that made [`Unit::seal`] one construction path among two:
//! a caller could decode `{"op":"generate","depth":0,...}` straight past every
//! check the constructor performs. The claim that `depth <= 1` was "unreachable"
//! was false for exactly that reason — it held for the mint and not for the
//! decoder, which is the half an attacker uses.
//!
//! Decoding therefore lands in [`RawUnit`], a transparent DTO that asserts
//! nothing, and the only way from there to a `Unit` is the fallible
//! [`TryFrom`] below, which enforces the whole v0 contract:
//!
//! | rule | why |
//! |---|---|
//! | `op` must be [`Op::Elide`] | v0 mints only what it can check; the others' verification classes have no implementation |
//! | `depth == op.depth_after(0)` | rejects a decoded depth 2, and any other declared depth the operation does not imply |
//! | `span.start < span.end` | a range covering no bytes addresses nothing — this is what `addressed: bool` was pretending to be |
//!
//! Whether the span lies inside the *actual* source is not knowable here — it
//! needs the bytes — so it is checked by [`crate::verify`], and the negative
//! case is tested there.
//!
//! # Identity is the derivation, not the lifecycle
//!
//! [`Unit::id`] is the id of the [`Derivation`]. `life` is deliberately outside
//! it: superseding a root is a fact *about* a derivation, not a different
//! derivation, and if it changed the id then marking a unit superseded would
//! fork every reference to it.

use content_addressable::{ContentAddressable, ContentError, ContentId, RawContentId};
use serde::{Deserialize, Serialize};

use crate::derivation::{Derivation, Span};
use crate::op::Op;

/// Lifecycle of a unit's root.
///
/// Operator intent drifts ("do Y instead"), and tracking that per-unit is
/// intractable; tracking it at the ROOT is not, and everything downstream
/// inherits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Life {
    /// The root still stands. Nothing downstream is freely evictable.
    Live,
    /// The operator replaced this intent ("do Y instead").
    Superseded,
    /// The operator retracted this intent outright.
    Withdrawn,
}

/// Why a mint was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    /// v0 mints elision only. The operation exists in the type; the constructor
    /// refuses it until its verification class has an implementation.
    #[error(
        "v0 mints elision only: `{op:?}` has verification class `{check:?}`, which has \
         no implementation and no calibrated threshold. Minting it would produce a unit \
         whose check nobody can run."
    )]
    NotMintableInV0 {
        /// The operation the caller asked to seal.
        op: Op,
        /// Its verification class — the reason the refusal is principled rather
        /// than a not-implemented-yet.
        check: crate::op::Check,
    },
    /// The span does not lie within the source it claims to address.
    #[error("span [{start}, {end}) does not lie within a source of {source_len} bytes")]
    SpanOutOfSource {
        /// Start of the offending span.
        start: u64,
        /// End of the offending span.
        end: u64,
        /// Length of the source actually supplied.
        source_len: usize,
    },
    /// A span covering no bytes addresses nothing.
    #[error(
        "span [{start}, {end}) covers no bytes: a unit that addresses nothing cannot be re-derived"
    )]
    UnaddressedSpan {
        /// Start of the offending span.
        start: u64,
        /// End of the offending span.
        end: u64,
    },
}

/// Why a decoded unit was refused admission.
///
/// Distinct from [`SealError`] on purpose: sealing is *this process minting*,
/// admission is *foreign bytes arriving*. They fail for overlapping reasons and
/// a reader of a log needs to know which side produced the refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdmitError {
    /// The decoded operation is not mintable in v0.
    #[error(
        "decoded unit declares `{op:?}` (verification class `{check:?}`), which v0 does not \
         admit: its check has no implementation, so the unit would be unverifiable evidence"
    )]
    NotMintableInV0 {
        /// The operation the decoded unit declared.
        op: Op,
        /// Its verification class.
        check: crate::op::Check,
    },
    /// The declared depth is not the depth the operation implies.
    #[error(
        "decoded unit declares depth {declared}, but `{op:?}` over source material implies \
         depth {implied}. This is the check the old `Deserialize` derive bypassed."
    )]
    DepthMismatch {
        /// The operation declared.
        op: Op,
        /// The depth the bytes claimed.
        declared: u32,
        /// The depth the operation actually implies.
        implied: u32,
    },
    /// A span covering no bytes addresses nothing.
    #[error("decoded unit's span [{start}, {end}) covers no bytes and so addresses nothing")]
    UnaddressedSpan {
        /// Start of the offending span.
        start: u64,
        /// End of the offending span.
        end: u64,
    },
}

/// The transparent decode target: **asserts nothing**.
///
/// Every field is public and plain. This type is what foreign bytes become, and
/// it is not usable as a unit until it has been through
/// [`TryFrom<RawUnit> for Unit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawUnit {
    /// What the bytes claim was done.
    pub op: Op,
    /// The source byte string claimed.
    pub source: RawContentId,
    /// The byte range claimed.
    pub span: Span,
    /// The addressed material claimed.
    pub elided: RawContentId,
    /// The root event claimed.
    pub root: ContentId,
    /// The generation depth claimed.
    pub depth: u32,
    /// The lifecycle claimed.
    pub life: Life,
}

/// A single unit of compacted context, addressed by its derivation.
///
/// Constructed only by [`Unit::seal`] or by admitting a [`RawUnit`]. Serialises
/// (so it can be stored and hashed) but does not deserialise (so the admission
/// boundary cannot be walked around).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit {
    derivation: Derivation,
    depth: u32,
    life: Life,
}

/// A `Unit` encodes exactly as its [`RawUnit`], so what this process writes is
/// what another process decodes — and what it decodes must then pass admission.
/// Written by hand rather than derived: `#[serde(flatten)]` over the derivation
/// would be a second, silently different layout, and `deny_unknown_fields` does
/// not compose with it.
impl Serialize for Unit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_raw().serialize(s)
    }
}

impl Unit {
    /// Seal one unit over SOURCE material.
    ///
    /// The source bytes are required — not a convenience, the whole point. A
    /// unit cannot be minted without the material it addresses, so there is no
    /// way to produce one whose `elided` claim was never computed from anything.
    ///
    /// # Errors
    ///
    /// [`SealError::NotMintableInV0`] for any operation other than [`Op::Elide`];
    /// [`SealError::UnaddressedSpan`] for a span covering no bytes;
    /// [`SealError::SpanOutOfSource`] when the span runs past the source.
    pub fn seal(op: Op, source: &[u8], span: Span, root: ContentId) -> Result<Self, SealError> {
        if op != Op::Elide {
            return Err(SealError::NotMintableInV0 {
                op,
                check: op.check(),
            });
        }
        if span.is_empty() {
            return Err(SealError::UnaddressedSpan {
                start: span.start,
                end: span.end,
            });
        }
        let derivation =
            Derivation::over(op, source, span, root).ok_or(SealError::SpanOutOfSource {
                start: span.start,
                end: span.end,
                source_len: source.len(),
            })?;
        Ok(Self {
            derivation,
            depth: op.depth_after(0),
            life: Life::Live,
        })
    }

    /// The addressed fact this unit's identity is computed over.
    #[must_use]
    pub const fn derivation(&self) -> &Derivation {
        &self.derivation
    }

    /// This unit's address: the id of its [`Derivation`].
    ///
    /// # Errors
    ///
    /// Propagates an encoding failure from the canonical form.
    pub fn id(&self) -> Result<crate::UnitId, ContentError> {
        self.derivation.id().map(crate::UnitId::from)
    }

    /// What was done to the source material.
    #[must_use]
    pub const fn op(&self) -> Op {
        self.derivation.op
    }

    /// Generation provenance depth: 0 for source material, one more per generation.
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// The root event this unit is caused by.
    #[must_use]
    pub const fn root(&self) -> ContentId {
        self.derivation.root
    }

    /// The lifecycle of this unit's root.
    #[must_use]
    pub const fn life(&self) -> Life {
        self.life
    }

    /// The depth bound: `depth <= 1`.
    #[must_use]
    pub const fn is_well_formed(&self) -> bool {
        self.depth <= 1
    }

    /// A unit is freely evictable when its root is no longer live.
    ///
    /// Deterministic: no grounding check, no model call, no judgement.
    #[must_use]
    pub const fn evictable(&self) -> bool {
        match self.life {
            Life::Live => false,
            Life::Superseded | Life::Withdrawn => true,
        }
    }

    /// Mark this unit's root as no longer live.
    ///
    /// Deliberately does **not** change [`Unit::id`]: lifecycle is a fact about
    /// a derivation, not a different derivation.
    #[must_use]
    pub const fn with_life(mut self, life: Life) -> Self {
        self.life = life;
        self
    }

    /// The transparent form, for encoding to bytes another process will decode.
    #[must_use]
    pub const fn to_raw(&self) -> RawUnit {
        RawUnit {
            op: self.derivation.op,
            source: self.derivation.source,
            span: self.derivation.span,
            elided: self.derivation.elided,
            root: self.derivation.root,
            depth: self.depth,
            life: self.life,
        }
    }
}

impl TryFrom<RawUnit> for Unit {
    type Error = AdmitError;

    /// **The only way in from foreign bytes.**
    ///
    /// # Errors
    ///
    /// See [`AdmitError`]. Span-versus-source is checked by [`crate::verify`],
    /// which needs the bytes this function does not have.
    fn try_from(raw: RawUnit) -> Result<Self, Self::Error> {
        if raw.op != Op::Elide {
            return Err(AdmitError::NotMintableInV0 {
                op: raw.op,
                check: raw.op.check(),
            });
        }
        let implied = raw.op.depth_after(0);
        if raw.depth != implied {
            return Err(AdmitError::DepthMismatch {
                op: raw.op,
                declared: raw.depth,
                implied,
            });
        }
        if raw.span.is_empty() {
            return Err(AdmitError::UnaddressedSpan {
                start: raw.span.start,
                end: raw.span.end,
            });
        }
        Ok(Self {
            derivation: Derivation {
                op: raw.op,
                source: raw.source,
                span: raw.span,
                elided: raw.elided,
                root: raw.root,
            },
            depth: raw.depth,
            life: raw.life,
        })
    }
}

impl ContentAddressable for Unit {
    /// The canonical form is the **derivation's**, so a unit's bytes and its id
    /// agree with [`Unit::id`].
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        self.derivation.canonical_form()
    }
}
