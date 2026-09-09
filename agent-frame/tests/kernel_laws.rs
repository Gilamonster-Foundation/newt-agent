//! **The kernel's laws, mirrored from `formal/ContextOps/Basic.lean`.**
//!
//! Each test below names the Lean theorem it corresponds to. This is the point
//! of the crate: the Lean build proves the law and this suite proves the Rust
//! implements the same law. Two implementations that happen to agree would be a
//! coincidence; a law and its executable form is a correspondence, and it is
//! only real while both halves are checked.
//!
//! If you change one side, CI fails on the other. That is intended.

use agent_frame::{Check, Life, Op, Packet, Root, SealError, Unit};
use content_addressable::ContentAddressable;

// ---- the operation algebra -------------------------------------------------

/// Lean: `generation_is_the_only_riser`.
#[test]
fn generation_is_the_only_riser() {
    for d in [0_u32, 1, 7, 4096] {
        assert_eq!(
            Op::Concise.depth_after(d),
            d,
            "concise must not raise depth"
        );
        assert_eq!(Op::Elide.depth_after(d), d, "elide must not raise depth");
        assert_eq!(
            Op::Generate.depth_after(d),
            d + 1,
            "generation is the only riser"
        );
    }
}

/// Lean: `elide_asserts_nothing`. Elision is the only operation that makes no
/// content claim, which is why it is the safe default when grounding is
/// uncertain.
#[test]
fn only_elision_asserts_nothing() {
    assert!(!Op::Elide.asserts());
    assert!(Op::Concise.asserts());
    assert!(Op::Generate.asserts());
}

/// Lean: `elide_is_rederivable` (`checkOf .elide = .rederive`) and
/// `only_generation_needs_attestation`.
///
/// This is the referent for the whole v0 scope line: elision is the only
/// deterministic class, so it is the only thing a party who did not run the
/// build can verify by recomputing.
#[test]
fn the_verification_classes_are_the_reproducibility_boundary() {
    assert_eq!(Op::Elide.check(), Check::Rederive);
    assert_eq!(Op::Concise.check(), Check::Ground);
    assert_eq!(Op::Generate.check(), Check::GroundAttest);

    for op in [Op::Concise, Op::Elide, Op::Generate] {
        assert_eq!(
            op.check() == Check::GroundAttest,
            op == Op::Generate,
            "only generation is non-reproducible, so only generation needs an attestation"
        );
    }
}

// ---- sealing ---------------------------------------------------------------

/// Lean: `seal_depth_le_one` and `seal_is_addressed`.
///
/// The keystone: a unit sealed from source is at depth at most 1, and depth
/// cannot reach 2 because there is no way to seal from a previous packet's
/// prose.
#[test]
fn a_sealed_unit_is_shallow_and_addressed() {
    let u = Unit::seal(Op::Elide, Some(Root::OperatorPrompt)).expect("elide is mintable in v0");
    assert_eq!(u.depth(), 0);
    assert!(u.is_well_formed(), "depth <= 1");
    assert!(u.addressed(), "everything sealed is addressed");
    assert_eq!(u.life(), Life::Live);
}

/// **v0 mints elision only.** The operations exist in the type; the constructor
/// refuses them.
///
/// A mintable `concise` unit whose `Ground` check has no implementation would
/// reproduce "unverified evidence is indistinguishable from no evidence" inside
/// the product.
#[test]
fn v0_refuses_to_mint_anything_it_cannot_check() {
    for op in [Op::Concise, Op::Generate] {
        let err = Unit::seal(op, Some(Root::UserAction)).expect_err("must refuse");
        assert!(
            matches!(err, SealError::NotMintableInV0 { op: got, .. } if got == op),
            "refusal must name the operation, got {err:?}"
        );
        // The refusal is principled, not not-implemented-yet: the message
        // carries the verification class that is missing.
        assert!(
            err.to_string().contains("verification class"),
            "the refusal must say WHY: {err}"
        );
    }
}

/// There is no way, through the public API, to build a unit that breaks the
/// depth bound. `seal` is the only mint and every field is private.
#[test]
fn the_depth_bound_is_unreachable_not_merely_unchecked() {
    for root in [
        None,
        Some(Root::OperatorPrompt),
        Some(Root::UserAction),
        Some(Root::HarnessEvent),
    ] {
        let u = Unit::seal(Op::Elide, root).expect("elide is mintable");
        assert!(u.is_well_formed());
        assert!(u.depth() <= 1);
    }
}

// ---- provenance ------------------------------------------------------------

/// Lean: `rooted_not_fabricated` / `orphan_is_fabricated`.
///
/// A model's assertion is never a provenance root — enforced by the type, since
/// `Root` has no `ModelOutput` constructor.
#[test]
fn a_unit_with_no_root_is_fabricated() {
    let orphan = Unit::seal(Op::Elide, None).unwrap();
    assert!(
        orphan.fabricated(),
        "no root means it happened because of nothing"
    );

    let rooted = Unit::seal(Op::Elide, Some(Root::HarnessEvent)).unwrap();
    assert!(!rooted.fabricated());
}

/// Lean: `live_not_freely_evictable`. Supersession-driven eviction carries zero
/// fidelity risk: no grounding check, no model call, no judgement.
#[test]
fn eviction_follows_the_root_not_the_operation() {
    let live = Unit::seal(Op::Elide, Some(Root::OperatorPrompt)).unwrap();
    assert!(!live.evictable(), "a live root is not freely evictable");

    for dead in [Life::Superseded, Life::Withdrawn] {
        assert!(
            live.with_life(dead).evictable(),
            "{dead:?} makes the unit freely evictable"
        );
    }
}

// ---- packets ---------------------------------------------------------------

/// Identity is over the **whole body**, prior link included — so a packet
/// cannot be re-parented without changing its id, and a chain cannot be
/// silently re-rooted.
#[test]
fn a_packets_identity_binds_the_chain_it_belongs_to() {
    let u = Unit::seal(Op::Elide, Some(Root::UserAction)).unwrap();
    let uid = u
        .content_id()
        .expect("a unit is content-addressable via serde");

    let genesis = Packet::genesis(vec![uid]);
    let gid = genesis.content_id().unwrap();

    let child = Packet::following(gid, vec![uid]);
    let reparented = Packet::following(child.content_id().unwrap(), vec![uid]);

    assert_ne!(
        genesis.content_id().unwrap(),
        child.content_id().unwrap(),
        "same units, different parent, must differ"
    );
    assert_ne!(
        child.content_id().unwrap(),
        reparented.content_id().unwrap(),
        "re-parenting must change the id"
    );
    assert!(genesis.is_genesis());
    assert!(!child.is_genesis());
}

/// A genesis packet has no parent; that IS the hermetic/resumable distinction.
/// `--hermetic` reduces to "always genesis", so the flag and the data structure
/// say the same thing and cannot get out of sync.
#[test]
fn genesis_is_exactly_the_absence_of_a_parent() {
    let empty: Vec<agent_frame::UnitId> = vec![];
    assert!(Packet::genesis(empty.clone()).is_genesis());

    let g = Packet::genesis(empty.clone()).content_id().unwrap();
    assert!(!Packet::following(g, empty).is_genesis());
}

/// Canonical encoding must round-trip, and the id must be stable across it.
#[test]
fn canonical_bytes_round_trip_and_the_id_is_stable() {
    let u = Unit::seal(Op::Elide, Some(Root::OperatorPrompt)).unwrap();
    let p = Packet::genesis(vec![u.content_id().unwrap()]);

    let bytes = p.canonical_form().expect("encodes");
    // The CHECKED decoder, never the plain one: the plain decoder verifies
    // neither canonical form nor the typed round trip, so the value it returns
    // can carry a different ContentId than the bytes it came from -- which
    // would make this very test vacuous.
    let back: Packet = content_addressable::canonical::from_canonical_dagcbor_checked(&bytes)
        .expect("decodes and re-encodes to identical bytes");

    assert_eq!(p, back, "round-trip must preserve the value");
    assert_eq!(
        p.content_id().unwrap(),
        back.content_id().unwrap(),
        "and its identity"
    );
}
