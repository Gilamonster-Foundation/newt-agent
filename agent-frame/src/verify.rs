//! **One verifier. Every surface calls it.**
//!
//! [`verify_unit`] is the only place in this line where the question *"is this
//! unit what it says it is?"* is decided. The harness calls it. `newt frame
//! verify` calls it. A second implementation of a verification decision is two
//! answers waiting to disagree, and the one that disagrees quietly is the one
//! that ships.
//!
//! # What re-derivation actually means
//!
//! `checkOf .elide = .rederive` is a claim with content only if something can
//! be recomputed and compared. Here it is three comparisons, in the order a
//! sceptic would make them:
//!
//! 1. **Is this the source it names?** Hash the supplied bytes; compare to
//!    [`Derivation::source`]. A verifier that trusts the caller's word about
//!    which bytes are the source has verified nothing.
//! 2. **Does the address resolve?** The span must lie within those bytes.
//! 3. **Is the elided material what it claims?** Hash the addressed range;
//!    compare to [`Derivation::elided`]. *This* is the re-derivation — the step
//!    the previous revision had no recorded claim to perform.
//!
//! All three must hold. Any failure names both sides, because "verification
//! failed" that does not say what did not match sends the reader back to a
//! debugger.
//!
//! # One link is the whole obligation
//!
//! [`verify_unit`] checks **one** derivation and stops. It does not resolve the
//! source's own provenance, and it does not fail because the source is itself a
//! derived thing — that is the source's business, not this unit's.
//!
//! That is complete verification, not a pragmatic cut, and the reason is
//! `Unit.wf`. `depth <= 1` says nothing is more than one derivation away from
//! source material; a frame with no parents is genesis, the defined bottom. So
//! there is no deeper chain to regress into **by construction**. The forensic
//! obligation and the depth bound are the same constraint seen from two
//! directions — which is also why refusing `concise` and `generate` in v0 is
//! not merely caution: it is what keeps regress impossible.
//!
//! **And it does not resolve the root.** `verify_unit` recomputes the three
//! claims that the source bytes can settle — source, span, elided — and no
//! others. A unit naming a root id nothing ever minted verifies exactly like
//! one naming a real event, because the bytes in hand cannot tell them apart.
//! [`Verified`] therefore carries no root field: reporting one would imply this
//! function had established something about it. Root resolution belongs to
//! whoever holds the store, and `newt frame` reports it as a separate,
//! separately-failable fact.
//!
//! There is deliberately no walker here. [`Verified`] reports the source id
//! that was checked, so a caller who wants the next link calls again with it.
//! The recursion lives in the caller; nothing in this design forbids depth, it
//! simply does not implement an unbounded walk over data we do not control on
//! anybody's behalf.
//!
//! # Why not `NodeStore`
//!
//! `content-addressable`'s [`NodeStore`] is the right seam for units and
//! packets, and its `get` / `get_unverified` split is the distinction this
//! module honours. It cannot serve *source* bytes: it is keyed by [`ContentId`]
//! (the dag-cbor profile) and a source is an opaque byte string keyed by
//! [`RawContentId`] (the raw profile). The two are deliberately different types
//! with no conversion between them. [`SourceResolver`] is that second profile's
//! lookup, not a second copy of the first.
//!
//! [`NodeStore`]: https://docs.rs/content-addressable
//! [`Derivation::source`]: crate::Derivation::source
//! [`Derivation::elided`]: crate::Derivation::elided

use content_addressable::RawContentId;

use crate::derivation::Span;
use crate::unit::Unit;

/// Where source bytes come from.
///
/// Split verified/unverified in the same shape as `NodeStore`: an implementor
/// supplies the raw fetch, and verification is the caller's — here,
/// [`verify_unit`]'s — job. An implementation that "helpfully" verified inside
/// `resolve` would make the mismatch case untestable.
pub trait SourceResolver {
    /// Fetch the bytes named by `id`, **without** checking that they hash to it.
    ///
    /// # Errors
    ///
    /// Implementation-defined; surfaced as [`VerifyError::SourceUnavailable`].
    fn resolve_unverified(&self, id: &RawContentId) -> Result<Vec<u8>, String>;
}

/// Both sides of a hash comparison.
///
/// Boxed inside [`VerifyError`] rather than inlined: a [`RawContentId`] is ~96
/// bytes, so two of them in a variant made every `Result` from [`verify_unit`]
/// 200 bytes wide — on the success path too. `clippy::result_large_err` is
/// right about that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatch {
    /// What the unit claims.
    pub declared: RawContentId,
    /// What recomputation actually produced.
    pub actual: RawContentId,
}

impl std::fmt::Display for Mismatch {
    /// Always names BOTH sides. "It did not match" that does not say what did
    /// not match sends the reader to a debugger.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "declared {}, recomputed {}", self.declared, self.actual)
    }
}

/// Why a unit did not verify.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The supplied bytes are not the source the unit names.
    #[error(
        "source mismatch ({at}) over {len} supplied bytes: this is not the material \
         the unit was derived from"
    )]
    SourceMismatch {
        /// Declared versus recomputed source id.
        at: Box<Mismatch>,
        /// How many bytes were supplied.
        len: usize,
    },
    /// The span runs past the end of the source.
    #[error(
        "span [{start}, {end}) does not lie within a source of {source_len} bytes: the \
         address does not resolve, so there is nothing to re-derive"
    )]
    SpanOutOfBounds {
        /// Start of the offending span.
        start: u64,
        /// End of the offending span.
        end: u64,
        /// Length of the source.
        source_len: usize,
    },
    /// The addressed bytes do not hash to what the unit claims. **The
    /// re-derivation failed.**
    #[error("re-derivation mismatch over source[{start}..{end}] ({at})")]
    ElisionMismatch {
        /// Declared versus recomputed elided-material id.
        at: Box<Mismatch>,
        /// Start of the span.
        start: u64,
        /// End of the span.
        end: u64,
    },
    /// The source could not be fetched at all.
    #[error("source {id} could not be resolved: {reason}")]
    SourceUnavailable {
        /// The source that could not be fetched.
        id: Box<RawContentId>,
        /// Why not.
        reason: String,
    },
}

/// What a successful verification establishes — the facts a report renders.
///
/// Returned rather than a bare `bool` so a caller (and `newt frame explain`)
/// reports what was checked, not merely that something was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// The source, confirmed by hashing the supplied bytes.
    pub source: RawContentId,
    /// The range within it that resolved.
    pub span: Span,
    /// The re-derived material, confirmed equal to the unit's claim.
    pub elided: RawContentId,
    /// How many bytes the span covered.
    pub elided_len: u64,
    /// Total size of the source.
    pub source_len: u64,
}

/// **The verification decision.** Recompute the unit's claims from the source
/// bytes and compare.
///
/// # Errors
///
/// See [`VerifyError`]; each variant names both the declared and the actual
/// value.
pub fn verify_unit(unit: &Unit, source: &[u8]) -> Result<Verified, VerifyError> {
    let d = unit.derivation();

    let actual_source = RawContentId::from_content(source);
    if actual_source != d.source {
        return Err(VerifyError::SourceMismatch {
            at: Box::new(Mismatch {
                declared: d.source,
                actual: actual_source,
            }),
            len: source.len(),
        });
    }

    let addressed = d.span.slice(source).ok_or(VerifyError::SpanOutOfBounds {
        start: d.span.start,
        end: d.span.end,
        source_len: source.len(),
    })?;

    let actual_elided = RawContentId::from_content(addressed);
    if actual_elided != d.elided {
        return Err(VerifyError::ElisionMismatch {
            at: Box::new(Mismatch {
                declared: d.elided,
                actual: actual_elided,
            }),
            start: d.span.start,
            end: d.span.end,
        });
    }

    Ok(Verified {
        source: actual_source,
        span: d.span,
        elided: actual_elided,
        elided_len: d.span.len(),
        source_len: source.len() as u64,
    })
}

/// [`verify_unit`], fetching the source through a resolver.
///
/// A thin wrapper on purpose: the decision stays in one function, and this adds
/// only the fetch. The CLI uses this; the harness may use either.
///
/// # Errors
///
/// [`VerifyError::SourceUnavailable`] if the fetch fails, else whatever
/// [`verify_unit`] returns.
pub fn verify_unit_with(
    unit: &Unit,
    resolver: &dyn SourceResolver,
) -> Result<Verified, VerifyError> {
    let id = unit.derivation().source;
    let bytes =
        resolver
            .resolve_unverified(&id)
            .map_err(|reason| VerifyError::SourceUnavailable {
                id: Box::new(id),
                reason,
            })?;
    verify_unit(unit, &bytes)
}
