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

use agent_frame::{
    AdmitError, Check, Life, Op, Packet, PacketAdmitError, PacketBody, RawPacket, RawUnit,
    RootEvent, RootKind, Span, Unit,
};
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

/// **Neither boundary establishes root provenance, and that is stated rather
/// than assumed.** A unit naming a root id nothing ever minted admits AND
/// verifies, identically to one naming a real event.
///
/// This is the executable form of the trust boundary: admission proves `root`
/// is present and is a well-formed dag-cbor id; re-derivation proves the
/// source, span and elided claims. *Resolving* the id needs a store, which this
/// crate does not have and must not grow. The layer that owns it is the store —
/// `newt frame` reports `root_kind`/`root_seq` when it resolves and
/// `root_unresolved` when it does not, never a silent absence.
#[test]
fn neither_admission_nor_verification_resolves_the_root() {
    let real = admissible();
    let mut forged = real;
    // A syntactically valid id for an event that was never minted: address a
    // DIFFERENT structured value and use its id as the root.
    forged.root = RootEvent::new(RootKind::HarnessEvent, b"never happened", u64::MAX)
        .id()
        .unwrap();
    assert_ne!(forged.root, real.root, "the forged root must differ");

    let u = Unit::try_from(forged)
        .expect("admission proves the root is a well-formed id, not that anything answers to it");
    assert_eq!(
        u.root(),
        forged.root,
        "the unresolvable id is carried as-is"
    );
    let v = agent_frame::verify_unit(&u, SRC)
        .expect("re-derivation settles source, span and elided — the root is not among them");

    // And the report says nothing about the root, so no caller can read a
    // successful verification as a statement about provenance.
    assert_eq!(v.source, RawContentId::from_content(SRC));
    assert_eq!(v.span, Span::new(4, 9));
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

// ---- packets: the same boundary, on the chain ------------------------------
//
// `Packet` is a `MerkleNode`, which carries a parent SET. v0 mints a CHAIN —
// `formal/ContextOps/Basic.lean`'s `Chain` is `genesis (units)` or
// `sealed (prior : Chain) (units)`, with no multi-parent constructor. While
// `Packet` derived `Deserialize`, foreign bytes could hand it two parents, and
// the resulting value satisfied `is_genesis() == false` AND `prior() == None`
// at the same time: a third state neither constructor can mint. `newt frame
// parents` rendered it as `outcome: "genesis"`.
//
// These cross the REAL foreign-byte boundary — `serde_json` over hand-written
// text — rather than calling the constructor, because the bypass was the
// decoder and only the decoder.

/// The wire text of a packet with the given parent ids.
fn packet_json(parents: &[String]) -> String {
    let links = parents
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"{{"payload":{{"units":[]}},"parents":[{links}]}}"#)
}

/// Two distinct, real packet addresses to hang parent links off.
fn two_addresses() -> (String, String) {
    let uid = Unit::try_from(admissible())
        .expect("baseline admits")
        .id()
        .unwrap();
    let a = Packet::genesis(vec![uid]).id().unwrap().to_string();
    let b = Packet::genesis(vec![]).id().unwrap().to_string();
    assert_ne!(
        a, b,
        "the two parents must differ, or the case is not multiparent"
    );
    (a, b)
}

/// **The anti-vacuous guard.** Both legitimate chain shapes must decode and
/// admit, or the refusal below passes for the wrong reason — a decoder that
/// rejects everything refuses two parents for free.
#[test]
fn both_legitimate_chain_shapes_are_admitted() {
    let (a, _) = two_addresses();

    let genesis: RawPacket = serde_json::from_str(&packet_json(&[])).expect("genesis decodes");
    let genesis = Packet::try_from(genesis).expect("zero parents is genesis, and admits");
    assert!(genesis.is_genesis());
    assert_eq!(genesis.prior(), None);

    let one: RawPacket = serde_json::from_str(&packet_json(std::slice::from_ref(&a)))
        .expect("a one-parent node decodes");
    let one = Packet::try_from(one).expect("exactly one parent is a chain link, and admits");
    assert!(!one.is_genesis());
    assert_eq!(
        one.prior().map(|p| p.to_string()),
        Some(a),
        "the admitted link must be the one the bytes named"
    );
}

/// **The bypass.** Two parents decode into a `RawPacket` — they are a valid DAG
/// node — and must not become a `Packet`.
#[test]
fn a_decoded_two_parent_node_is_refused() {
    let (a, b) = two_addresses();

    let raw: RawPacket = serde_json::from_str(&packet_json(&[a, b]))
        .expect("a two-parent DAG node is well-formed bytes; that is why admission must judge it");
    assert_eq!(raw.parents().len(), 2, "the decode must really carry two");

    let err = Packet::try_from(raw).expect_err("a DAG node must not become a v0 packet");
    assert_eq!(err, PacketAdmitError::NotAChain { parents: 2 });
    assert!(
        err.to_string().contains("CHAIN"),
        "the refusal must say WHY, not just no: {err}"
    );
}

/// The contradictory state itself, named: no admitted packet may report
/// "not genesis" and "no prior" at once. That pair is what the CLI rendered as
/// an origin.
#[test]
fn no_admitted_packet_is_both_non_genesis_and_priorless() {
    let (a, b) = two_addresses();
    for n in 0..=3 {
        let parents: Vec<String> = [a.clone(), b.clone()].into_iter().cycle().take(n).collect();
        // `cycle` repeats, and parents are a SET, so n>=3 collapses back to 2.
        let raw: RawPacket = serde_json::from_str(&packet_json(&parents)).expect("decodes");
        let seen = raw.parents().len();
        match Packet::try_from(raw) {
            Ok(p) => {
                assert!(seen <= 1, "{seen} parents must not admit");
                assert_eq!(
                    p.is_genesis(),
                    p.prior().is_none(),
                    "genesis and priorless must be the SAME fact"
                );
            }
            Err(e) => assert_eq!(e, PacketAdmitError::NotAChain { parents: seen }),
        }
    }
}

/// The positive half of the closed boundary: a `Packet` still SERIALISES, and
/// its own bytes re-admit — so closing the decoder did not make the type
/// unstorable.
///
/// **What this file does NOT cover, said plainly.** The two negatives above
/// exercise the admission *function*; they would go on passing if
/// `#[derive(Deserialize)]` came back on `Packet`, because they route through
/// `RawPacket` either way. The absence of that derive is a compile-time
/// property with no runtime witness — `serde_json::from_str::<Packet>` simply
/// stops type-checking — and asserting a negative trait bound needs a
/// compile-fail harness this crate does not carry. So the guard is the derive
/// line itself plus every caller going through `TryFrom`, and this comment is
/// here because an untested invariant that reads as tested is how the `Unit`
/// bypass survived review the first time.
#[test]
fn a_packets_own_bytes_still_round_trip_through_admission() {
    let p = Packet::following(Packet::genesis(vec![]).id().unwrap(), vec![]);
    let wire = serde_json::to_vec(&p).expect("a packet serialises");
    let raw: RawPacket = serde_json::from_slice(&wire).expect("the wire form is a RawPacket");
    assert_eq!(
        Packet::try_from(raw).expect("its own bytes admit"),
        p,
        "what this process writes must be what another process admits"
    );

    // The payload type is public, so a caller can build the wire form by hand
    // — and it lands in `RawPacket`, never straight in a `Packet`.
    let by_hand = RawPacket::genesis(PacketBody { units: vec![] });
    assert!(Packet::try_from(by_hand)
        .expect("a hand-built genesis node admits")
        .is_genesis());
}
