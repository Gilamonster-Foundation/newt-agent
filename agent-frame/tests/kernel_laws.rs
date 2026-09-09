//! **The kernel's laws, mirrored from `formal/ContextOps/Basic.lean`.**
//!
//! Each test names the Lean theorem it corresponds to. The Lean build proves
//! the law; this suite proves the Rust implements the same one. Two
//! implementations that happen to agree would be a coincidence; a law and its
//! executable form is a correspondence, and it is only real while both halves
//! are checked.
//!
//! The *addressing* properties live in `addressing.rs`, admission in
//! `admission.rs`, and re-derivation in `rederive.rs` — those are claims about
//! this crate's design rather than about the formal core, and mixing them here
//! would let a pass on one read as a pass on the other.

use agent_frame::{
    Check, Life, Op, Packet, RawUnit, RootEvent, RootKind, SealError, Span, Unit, UnitId,
};
use content_addressable::{ContentAddressable, ContentId};

const SRC: &[u8] = b"the quick brown fox jumps over the lazy dog";

fn root() -> ContentId {
    RootEvent::new(RootKind::OperatorPrompt, b"summarise the log", 0)
        .id()
        .unwrap()
}

fn sealed() -> Unit {
    Unit::seal(Op::Elide, SRC, Span::new(4, 9), root()).expect("elide is mintable in v0")
}

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

/// Lean: `elide_asserts_nothing`.
#[test]
fn only_elision_asserts_nothing() {
    assert!(!Op::Elide.asserts());
    assert!(Op::Concise.asserts());
    assert!(Op::Generate.asserts());
}

/// Lean: `elide_is_rederivable` (`checkOf .elide = .rederive`) and
/// `only_generation_needs_attestation`.
///
/// The classes are the reproducibility boundary. That `Rederive` is a class an
/// implementation actually runs — rather than a label — is `rederive.rs`.
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
/// "Addressed" is no longer a boolean the constructor sets to `true`. A sealed
/// unit is addressed because it *carries an address* — a source and a range
/// within it — and `seal` cannot produce one without the bytes.
#[test]
fn a_sealed_unit_is_shallow_and_addressed() {
    let u = sealed();
    assert_eq!(u.depth(), 0);
    assert!(u.is_well_formed(), "depth <= 1");
    assert_eq!(u.life(), Life::Live);

    let d = u.derivation();
    assert_eq!(d.span, Span::new(4, 9));
    assert_eq!(d.root, root());
    assert!(
        agent_frame::verify_unit(&u, SRC).is_ok(),
        "the address must resolve — that is what 'addressed' now means"
    );
}

/// **v0 mints elision only**, on BOTH paths — the mint and the decoder.
///
/// The old kernel enforced this on the mint alone; `Unit` derived
/// `Deserialize`, so the decoder was an unchecked second constructor.
#[test]
fn v0_refuses_to_mint_anything_it_cannot_check() {
    for op in [Op::Concise, Op::Generate] {
        let err = Unit::seal(op, SRC, Span::new(4, 9), root()).expect_err("must refuse");
        assert!(
            matches!(err, SealError::NotMintableInV0 { op: got, .. } if got == op),
            "refusal must name the operation, got {err:?}"
        );
        assert!(
            err.to_string().contains("verification class"),
            "the refusal must say WHY: {err}"
        );
    }
}

/// The depth bound holds on the only two ways a `Unit` can exist.
#[test]
fn the_depth_bound_holds_on_every_construction_path() {
    let minted = sealed();
    assert!(minted.is_well_formed() && minted.depth() <= 1);

    let admitted = Unit::try_from(minted.to_raw()).expect("its own bytes admit");
    assert!(admitted.is_well_formed() && admitted.depth() <= 1);
    assert_eq!(minted, admitted);

    // And there is no third path: `Unit` does not implement `Deserialize`, so
    // bytes can only arrive as a `RawUnit` and go through `TryFrom`.
    // `admission.rs` drives the refusals.
}

// ---- provenance ------------------------------------------------------------

/// Lean: `rooted_not_fabricated`.
///
/// A model's assertion is never a provenance root — enforced by the type, since
/// [`RootKind`] has no `ModelOutput` constructor. A unit's root is now
/// mandatory *and* addressed: there is no `None`, so there is no fabricated
/// unit to represent.
#[test]
fn every_unit_names_the_event_that_caused_it() {
    let u = sealed();
    assert_eq!(u.root(), root());

    let kinds = [
        RootKind::OperatorPrompt,
        RootKind::UserAction,
        RootKind::HarnessEvent,
    ];
    let mut ids: Vec<ContentId> = kinds
        .iter()
        .map(|k| RootEvent::new(*k, b"same words", 0).id().unwrap())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), kinds.len(), "each kind is a distinct event");
}

/// Lean: `live_not_freely_evictable`. Supersession-driven eviction carries zero
/// fidelity risk: no grounding check, no model call, no judgement.
#[test]
fn eviction_follows_the_root_not_the_operation() {
    let live = sealed();
    assert!(!live.evictable(), "a live root is not freely evictable");
    for dead in [Life::Superseded, Life::Withdrawn] {
        assert!(
            live.with_life(dead).evictable(),
            "{dead:?} makes the unit freely evictable"
        );
    }
}

// ---- packets ---------------------------------------------------------------

/// Identity is over the whole node — payload and parent links together — so a
/// packet cannot be re-parented without changing its id.
#[test]
fn a_packets_identity_binds_the_chain_it_belongs_to() {
    let uid = sealed().id().unwrap();

    let genesis = Packet::genesis(vec![uid]);
    let gid = genesis.id().unwrap();
    let child = Packet::following(gid, vec![uid]);
    let reparented = Packet::following(child.id().unwrap(), vec![uid]);

    assert_ne!(
        gid,
        child.id().unwrap(),
        "same units, different parent, must differ"
    );
    assert_ne!(
        child.id().unwrap(),
        reparented.id().unwrap(),
        "re-parenting must change the id"
    );
    assert_eq!(child.prior(), Some(gid));
    assert!(genesis.is_genesis() && genesis.prior().is_none());
    assert!(!child.is_genesis());
}

/// Genesis is exactly the empty parent set — the same fact the old
/// `prior: None` field encoded, now expressed as a Merkle parent link.
#[test]
fn genesis_is_exactly_the_absence_of_a_parent() {
    let empty: Vec<UnitId> = vec![];
    let g = Packet::genesis(empty.clone());
    assert!(g.is_genesis() && g.parents().is_empty());

    let child = Packet::following(g.id().unwrap(), empty);
    assert!(!child.is_genesis());
    assert_eq!(child.parents().len(), 1);
}

// ---- encoding --------------------------------------------------------------

/// **Canonical encoding round-trips, and the id is stable across it.**
///
/// Deliberately separate from admission: this asks "do these bytes survive",
/// admission asks "should this value be admitted". A file that mixed them would
/// let a pass on one read as a pass on the other.
#[test]
fn canonical_bytes_round_trip_and_the_id_is_stable() {
    let u = sealed();
    let bytes = u.canonical_form().expect("a unit encodes");
    let id = u.id().expect("a unit addresses");

    // A unit's bytes ARE its derivation's, so the id agrees with both.
    assert_eq!(
        id.as_content_id(),
        &u.derivation().id().unwrap(),
        "the unit's id is its derivation's id"
    );
    assert_eq!(
        content_addressable::ContentId::from_canonical_bytes(&bytes),
        u.derivation().id().unwrap()
    );

    // The wire form decodes to a RawUnit and re-admits to an equal Unit.
    let wire = serde_json::to_vec(&u).expect("a unit serialises");
    let raw: RawUnit = serde_json::from_slice(&wire).expect("the wire form is a RawUnit");
    let back = Unit::try_from(raw).expect("its own bytes admit");
    assert_eq!(back, u);
    assert_eq!(back.id().unwrap(), id);

    // Packets round-trip too, parent links included.
    let p = Packet::following(Packet::genesis(vec![id]).id().unwrap(), vec![id]);
    let pw = serde_json::to_vec(&p).unwrap();
    let pb: Packet = serde_json::from_slice(&pw).unwrap();
    assert_eq!(pb, p);
    assert_eq!(pb.id().unwrap(), p.id().unwrap());
}
