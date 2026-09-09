//! **The admission boundary: what happens to foreign bytes.**
//!
//! The previous kernel derived `Deserialize` on `Unit`. That made `seal` one
//! construction path of two, and the other one checked nothing: a caller could
//! decode `{"op":"generate","depth":0,...}` and hold a `Unit` the constructor
//! would have refused. The claim that `depth <= 1` was "unreachable" was true
//! of the mint and false of the decoder.
//!
//! Every test here is a NEGATIVE: bytes that decode fine and must still be
//! refused. A boundary is only closed if you can name what it turns away.
//!
//! Encoding round-trip lives in `kernel_laws.rs`, deliberately apart — "these
//! bytes survive a round trip" and "this value is admissible" are different
//! questions, and a file that mixes them lets a pass on one look like a pass on
//! the other.

use agent_frame::{AdmitError, Check, Life, Op, RawUnit, RootEvent, RootKind, Span, Unit};
use content_addressable::{ContentId, RawContentId};

const SRC: &[u8] = b"the quick brown fox jumps over the lazy dog";

fn root() -> ContentId {
    RootEvent::new(RootKind::OperatorPrompt, b"do the thing", 0)
        .id()
        .unwrap()
}

/// A raw unit that WOULD be admitted, so each test below changes exactly one
/// thing and the refusal is attributable to that thing.
fn admissible() -> RawUnit {
    Unit::seal(Op::Elide, SRC, Span::new(4, 9), root())
        .expect("the baseline must be admissible, or these tests prove nothing")
        .to_raw()
}

#[test]
fn the_baseline_is_actually_admitted() {
    // The anti-vacuous guard. If this fails, every negative below passes for
    // the wrong reason.
    let u = Unit::try_from(admissible()).expect("baseline admits");
    assert_eq!(u.op(), Op::Elide);
    assert_eq!(u.depth(), 0);
}

/// **The bypass that motivated the redesign.** `generate` decodes; it must not
/// admit.
#[test]
fn a_decoded_generate_is_refused() {
    let mut raw = admissible();
    raw.op = Op::Generate;
    raw.depth = 1; // the depth `generate` actually implies, so depth is not the reason
    let err = Unit::try_from(raw).expect_err("generate must not cross the boundary");
    assert_eq!(
        err,
        AdmitError::NotMintableInV0 {
            op: Op::Generate,
            check: Check::GroundAttest,
        }
    );
    assert!(
        err.to_string().contains("has no implementation"),
        "the refusal must say WHY, not just no: {err}"
    );
}

/// `concise` too — its `Ground` check has no implementation and no calibrated
/// threshold.
#[test]
fn a_decoded_concise_is_refused() {
    let mut raw = admissible();
    raw.op = Op::Concise;
    let err = Unit::try_from(raw).expect_err("concise must not cross the boundary");
    assert_eq!(
        err,
        AdmitError::NotMintableInV0 {
            op: Op::Concise,
            check: Check::Ground,
        }
    );
}

/// **Depth 2 by decode** — the exact thing the old `Deserialize` derive let
/// through while the docs called it unreachable.
#[test]
fn a_decoded_depth_two_is_refused() {
    for declared in [1_u32, 2, 7, u32::MAX] {
        let mut raw = admissible();
        raw.depth = declared;
        let err = Unit::try_from(raw)
            .unwrap_err_or_panic(&format!("depth {declared} must not cross the boundary"));
        assert_eq!(
            err,
            AdmitError::DepthMismatch {
                op: Op::Elide,
                declared,
                implied: 0,
            },
            "elision does not raise depth, so any declared depth but 0 is a lie"
        );
    }
}

/// **Unaddressed.** `addressed: true` used to be a boolean a caller asserted.
/// Now the address is the span, and a span covering no bytes is not one.
#[test]
fn a_decoded_unaddressed_span_is_refused() {
    for (start, end) in [(0_u64, 0_u64), (9, 9), (9, 4)] {
        let mut raw = admissible();
        raw.span = Span::new(start, end);
        let err = Unit::try_from(raw).expect_err("a span covering no bytes addresses nothing");
        assert_eq!(err, AdmitError::UnaddressedSpan { start, end });
    }
}

/// Lifecycle is data, not a claim: a decoded `superseded` unit admits, because
/// nothing about it is unverifiable.
#[test]
fn lifecycle_is_admitted_as_data() {
    for life in [Life::Live, Life::Superseded, Life::Withdrawn] {
        let mut raw = admissible();
        raw.life = life;
        let u = Unit::try_from(raw).expect("lifecycle asserts nothing, so it admits");
        assert_eq!(u.life(), life);
    }
}

/// A raw unit whose `elided` claim is a lie still ADMITS — and that is correct.
/// Admission is a semantic check on the declaration; whether the declaration is
/// TRUE needs the source bytes and is `verify`'s job. Keeping them apart is why
/// `rederive.rs` exists.
#[test]
fn admission_does_not_pretend_to_verify() {
    let mut raw = admissible();
    raw.elided = RawContentId::from_content(b"not what that span contains");
    let u = Unit::try_from(raw).expect("admission checks the shape, not the world");
    assert!(
        agent_frame::verify_unit(&u, SRC).is_err(),
        "the lie must be caught by verification, which is the half that holds the bytes"
    );
}

/// Small helper so the depth loop reads as one assertion per case.
trait UnwrapErrOrPanic<E> {
    fn unwrap_err_or_panic(self, msg: &str) -> E;
}
impl<T: std::fmt::Debug, E> UnwrapErrOrPanic<E> for Result<T, E> {
    fn unwrap_err_or_panic(self, msg: &str) -> E {
        match self {
            Ok(v) => panic!("{msg}: admitted {v:?}"),
            Err(e) => e,
        }
    }
}
