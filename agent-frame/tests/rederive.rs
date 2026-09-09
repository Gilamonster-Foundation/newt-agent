//! **`checkOf .elide = .rederive`, made real.**
//!
//! The previous kernel shipped the word. `Op::Elide.check()` returned
//! `Check::Rederive` and a test asserted that it did — but nothing anywhere
//! re-derived anything, and nothing could have: a unit carried no source, no
//! range, and no recorded claim to compare a recomputation against. The word
//! was decoration.
//!
//! Re-derivation is now three comparisons against the actual bytes, and this
//! file drives each of them to a failure as well as to a pass. A verifier that
//! has only ever been shown passing input is a verifier nobody has tested.

use agent_frame::{
    verify_unit, verify_unit_with, Op, RootEvent, RootKind, SourceResolver, Span, Unit, VerifyError,
};
use content_addressable::{ContentId, RawContentId};

const SRC: &[u8] = b"the quick brown fox jumps over the lazy dog";
const OTHER: &[u8] = b"the quick brown fox jumps over the lazy cat";

fn root() -> ContentId {
    RootEvent::new(RootKind::UserAction, b"esc", 0)
        .id()
        .unwrap()
}

fn unit(span: Span) -> Unit {
    Unit::seal(Op::Elide, SRC, span, root()).expect("mintable")
}

/// The positive case, and what it establishes — not a bare `true`.
#[test]
fn a_unit_re_derives_against_its_own_source() {
    let u = unit(Span::new(4, 19));
    let v = verify_unit(&u, SRC).expect("a unit must verify against the source it was sealed over");

    assert_eq!(v.source, RawContentId::from_content(SRC));
    assert_eq!(v.span, Span::new(4, 19));
    assert_eq!(v.elided, RawContentId::from_content(&SRC[4..19]));
    assert_eq!(v.elided_len, 15);
    assert_eq!(v.source_len, SRC.len() as u64);
    assert_eq!(
        &SRC[4..19],
        b"quick brown fox",
        "the fixture must actually address the bytes the assertion names"
    );
}

/// **Comparison 1.** A verifier that takes the caller's word for which bytes
/// are the source has verified nothing.
#[test]
fn a_near_identical_source_is_rejected() {
    let u = unit(Span::new(4, 19));
    let err = verify_unit(&u, OTHER).expect_err("one byte different is a different source");
    match err {
        VerifyError::SourceMismatch { at, len } => {
            assert_eq!(at.declared, RawContentId::from_content(SRC));
            assert_eq!(at.actual, RawContentId::from_content(OTHER));
            assert_eq!(len, OTHER.len());
            assert_ne!(at.declared, at.actual, "the error must name BOTH sides");
        }
        other => panic!("expected a source mismatch, got {other:?}"),
    }
}

/// **Comparison 2.** An address that does not resolve is not an address. The
/// span is checked against the REAL length, which is why admission cannot do it.
#[test]
fn a_span_past_the_end_of_the_source_is_rejected() {
    // Built by hand: `seal` refuses to mint this, which is itself the point.
    let over = Span::new(4, SRC.len() as u64 + 1);
    assert!(
        Unit::seal(Op::Elide, SRC, over, root()).is_err(),
        "seal must refuse a span it cannot address"
    );

    // The decode path can still present one, so verification must catch it.
    let mut raw = unit(Span::new(4, 19)).to_raw();
    raw.span = over;
    let smuggled = Unit::try_from(raw).expect("admission cannot see the source length");
    let err = verify_unit(&smuggled, SRC).expect_err("the span runs past the end");
    assert!(
        matches!(
            err,
            VerifyError::SpanOutOfBounds { source_len, .. } if source_len == SRC.len()
        ),
        "got {err:?}"
    );
}

/// **Comparison 3 — the re-derivation itself.** The recorded claim is
/// recomputed from the bytes and compared.
#[test]
fn a_forged_elision_claim_is_caught_by_recomputation() {
    let mut raw = unit(Span::new(4, 19)).to_raw();
    // Claim the span holds something it does not. Everything else is untouched
    // and correct, so ONLY the re-derivation can catch this.
    raw.elided = RawContentId::from_content(b"quick brown cat");
    let forged = Unit::try_from(raw).expect("shape is fine; the lie is about the world");

    let err = verify_unit(&forged, SRC).expect_err("recomputation must catch the forged claim");
    match err {
        VerifyError::ElisionMismatch { at, start, end } => {
            assert_eq!(at.declared, RawContentId::from_content(b"quick brown cat"));
            assert_eq!(at.actual, RawContentId::from_content(b"quick brown fox"));
            assert_eq!((start, end), (4, 19));
        }
        other => panic!("expected an elision mismatch, got {other:?}"),
    }
}

/// Every span of a source re-derives — the check is not accidentally passing on
/// one lucky fixture.
#[test]
fn re_derivation_holds_across_every_span_of_a_source() {
    let n = SRC.len() as u64;
    for start in 0..n {
        for end in (start + 1)..=n {
            let u = unit(Span::new(start, end));
            let v = verify_unit(&u, SRC).expect("every real span re-derives");
            assert_eq!(v.elided_len, end - start);
        }
    }
}

// ---- the resolver seam -----------------------------------------------------

/// A resolver that hands back whatever it was given, verified or not — the
/// `get_unverified` half of the split. Verification stays in `verify_unit`.
struct Fixed(Vec<u8>);
impl SourceResolver for Fixed {
    fn resolve_unverified(&self, _id: &RawContentId) -> Result<Vec<u8>, String> {
        Ok(self.0.clone())
    }
}

struct Missing;
impl SourceResolver for Missing {
    fn resolve_unverified(&self, id: &RawContentId) -> Result<Vec<u8>, String> {
        Err(format!("no blob {id}"))
    }
}

/// **D6.** The resolver wrapper reaches the SAME decision as the direct call —
/// it adds a fetch and nothing else.
#[test]
fn the_resolver_wrapper_decides_identically_to_the_direct_call() {
    let u = unit(Span::new(4, 19));

    assert_eq!(
        verify_unit_with(&u, &Fixed(SRC.to_vec())),
        verify_unit(&u, SRC),
        "one verifier: the wrapper must not reach a different verdict"
    );
    assert_eq!(
        verify_unit_with(&u, &Fixed(OTHER.to_vec())),
        verify_unit(&u, OTHER),
        "including on the failing verdict — a wrapper that only agrees when \
         things pass is two verifiers"
    );
}

/// A resolver that cannot supply the source fails CLOSED, and says which blob.
#[test]
fn an_unresolvable_source_fails_closed() {
    let u = unit(Span::new(4, 19));
    let err = verify_unit_with(&u, &Missing).expect_err("absence is a finding, not a pass");
    match err {
        VerifyError::SourceUnavailable { id, reason } => {
            assert_eq!(*id, RawContentId::from_content(SRC));
            assert!(reason.contains("no blob"), "{reason}");
        }
        other => panic!("expected source-unavailable, got {other:?}"),
    }
}
