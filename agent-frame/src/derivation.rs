//! Derivations: the addressed fact a unit's identity is computed over.
//!
//! # The defect this type exists to fix
//!
//! The first revision of this kernel hashed a `Unit` holding
//! `{ op, depth, root_kind, life, addressed: bool }`. None of those fields
//! names the material. Two elisions over completely different sources, with the
//! same root kind, serialize to identical bytes and therefore mint **the same
//! content id** — the distinguishing information never enters the hash. And
//! `addressed: true` is a *declaration* that a unit is addressed, not an
//! address: nothing can be fetched from it, so `checkOf .elide = .rederive` had
//! nothing to re-derive.
//!
//! A [`Derivation`] carries the address instead of asserting one. The source is
//! named, the byte range within it is named, and the material that range covers
//! is named — so a consumer who holds the source bytes can recompute the claim
//! and compare. That is what makes the id self-validating rather than
//! self-describing.
//!
//! # Two id profiles, deliberately
//!
//! `source` and `elided` are [`RawContentId`] — they name **opaque byte
//! strings**. `root` is a [`ContentId`] — it names a **canonical structured
//! value** ([`crate::RootEvent`]). `content-addressable` mints these as
//! different types with no `PartialEq` and no `From` between them precisely so
//! this distinction cannot be lost by accident, and the kernel honours it.

use content_addressable::{ContentAddressable, ContentError, ContentId, RawContentId};
use serde::{Deserialize, Serialize};

use crate::op::Op;

/// A half-open byte range `[start, end)` within a source.
///
/// Half-open so that adjacent spans tile without overlap and an empty span is
/// exactly `start == end` — which admission rejects, because a range covering
/// no bytes addresses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    /// First byte of the range, inclusive.
    pub start: u64,
    /// One past the last byte of the range, exclusive.
    pub end: u64,
}

impl Span {
    /// A span over `[start, end)`.
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// Number of bytes this span covers, saturating on an inverted range.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether this span covers no bytes — including the inverted case.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Borrow the bytes this span covers, or `None` if it runs past the end.
    #[must_use]
    pub fn slice<'a>(&self, source: &'a [u8]) -> Option<&'a [u8]> {
        let start = usize::try_from(self.start).ok()?;
        let end = usize::try_from(self.end).ok()?;
        if end < start || end > source.len() {
            return None;
        }
        source.get(start..end)
    }
}

/// What was done, to which material, on whose authority.
///
/// **This is the hashed body of a unit.** Change the source, the span, the
/// operation or the root and you get a different [`ContentId`] — which is the
/// property the whole kernel rests on and is asserted directly by
/// `tests/addressing.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Derivation {
    /// What was done to the material.
    pub op: Op,
    /// The source byte string this derivation is over.
    pub source: RawContentId,
    /// Which byte range of `source`.
    pub span: Span,
    /// The material that range covers.
    ///
    /// Functionally redundant given `source` and `span` — and that is the
    /// point. It is the **claim being checked**: re-derivation recomputes the
    /// hash of the addressed bytes and compares it to this field. A check with
    /// nothing recorded to compare against is not a check, and the earlier
    /// revision shipped the word `rederive` without one.
    pub elided: RawContentId,
    /// The [`crate::RootEvent`] that caused this derivation.
    pub root: ContentId,
}

impl Derivation {
    /// Address a derivation over `source` at `span`, rooted in `root`.
    ///
    /// Returns `None` when the span does not lie within `source` — an address
    /// that does not resolve is not an address, so it cannot be built.
    #[must_use]
    pub fn over(op: Op, source: &[u8], span: Span, root: ContentId) -> Option<Self> {
        let addressed = span.slice(source)?;
        Some(Self {
            op,
            source: RawContentId::from_content(source),
            span,
            elided: RawContentId::from_content(addressed),
            root,
        })
    }

    /// This derivation's address — and therefore its unit's.
    ///
    /// # Errors
    ///
    /// Propagates an encoding failure from the canonical form.
    pub fn id(&self) -> Result<ContentId, ContentError> {
        self.content_id()
    }
}

impl ContentAddressable for Derivation {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}
