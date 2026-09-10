//! **A unit's address distinguishes what it represents.**
//!
//! This file is the regression suite for the defect that motivated the
//! redesign. The previous kernel hashed `{ op, depth, root_kind, life,
//! addressed }` — no field of which names any material — so every elision with
//! the same root kind minted the SAME content id no matter what it elided. The
//! distinguishing information never entered the hash, and `rederive` had
//! nothing to re-derive.
//!
//! Each test below fails on that design and passes on this one.

use agent_frame::{Op, RootEvent, RootKind, Span, Unit, UnitId};
use content_addressable::ContentId;

const SRC_A: &[u8] = b"the quick brown fox jumps over the lazy dog";
const SRC_B: &[u8] = b"a completely different source with other bytes!!";

fn root(seq: u64) -> ContentId {
    RootEvent::new(RootKind::OperatorPrompt, b"run the tests", seq)
        .id()
        .expect("a root event addresses")
}

fn seal(source: &[u8], span: Span, root: ContentId) -> Unit {
    Unit::seal(Op::Elide, source, span, root).expect("elide over a real span is mintable")
}

/// **The core property.** Two elisions over different sources must not collide.
#[test]
fn different_sources_mint_different_addresses() {
    let r = root(0);
    let a = seal(SRC_A, Span::new(4, 9), r);
    let b = seal(SRC_B, Span::new(4, 9), r);
    assert_ne!(
        a.id().unwrap(),
        b.id().unwrap(),
        "same op, same span, same root, DIFFERENT source — the ids must differ, \
         or the address does not name the material"
    );
}

/// **The core property, second axis.** Different ranges of one source must not
/// collide either.
#[test]
fn different_spans_of_one_source_mint_different_addresses() {
    let r = root(0);
    let ids: Vec<UnitId> = [
        Span::new(0, 3),
        Span::new(4, 9),
        Span::new(4, 10),
        Span::new(5, 10),
        Span::new(0, SRC_A.len() as u64),
    ]
    .into_iter()
    .map(|s| seal(SRC_A, s, r).id().unwrap())
    .collect();

    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        ids.len(),
        "five distinct spans of one source must mint five distinct ids, got {ids:?}"
    );
}

/// A root is an ADDRESSED EVENT, not a category: two operator turns with the
/// same words are different roots, and units under them are different units.
#[test]
fn the_root_identifies_which_turn_not_merely_which_kind() {
    let first = root(0);
    let second = root(1);
    assert_ne!(
        first, second,
        "the same words typed twice are two turns — a root event must distinguish them"
    );
    assert_ne!(
        seal(SRC_A, Span::new(4, 9), first).id().unwrap(),
        seal(SRC_A, Span::new(4, 9), second).id().unwrap(),
        "same elision, different operator turn — the unit ids must differ"
    );

    // Kind is still carried, but it is a field of the event, not the identity.
    let action = RootEvent::new(RootKind::UserAction, b"run the tests", 0)
        .id()
        .unwrap();
    assert_ne!(
        first, action,
        "same material, same seq, different kind — still different events"
    );
}

/// Identity is the DERIVATION. Lifecycle is a fact about a derivation, not a
/// different one — so superseding must not fork every reference to the unit.
#[test]
fn lifecycle_does_not_change_the_address() {
    let u = seal(SRC_A, Span::new(4, 9), root(0));
    let before = u.id().unwrap();
    for life in [
        agent_frame::Life::Superseded,
        agent_frame::Life::Withdrawn,
        agent_frame::Life::Live,
    ] {
        assert_eq!(
            u.with_life(life).id().unwrap(),
            before,
            "{life:?} is a fact ABOUT this derivation, not a different derivation"
        );
    }
    assert!(!u.evictable());
    assert!(u.with_life(agent_frame::Life::Superseded).evictable());
}

/// **D4.** `UnitId` and `PacketId` are distinct types, so a packet id cannot
/// silently fill a unit slot.
///
/// The compile-time half is the real assertion and it cannot be written as a
/// runtime `assert!`: `Packet::genesis(vec![packet_id])` does not compile, and
/// `trybuild` is not a dependency this leaf crate is taking. What is asserted
/// here is that they are not the same type by construction — a shared alias
/// would make `UnitId::from` and `PacketId::from` the same function.
#[test]
fn unit_ids_and_packet_ids_are_not_interchangeable() {
    use agent_frame::{Packet, PacketId};
    let u = seal(SRC_A, Span::new(4, 9), root(0)).id().unwrap();
    let p: PacketId = Packet::genesis(vec![u]).id().unwrap();

    // Same underlying content id, two incompatible wrappers.
    let round: UnitId = UnitId::from(*p.as_content_id());
    assert_eq!(round.as_content_id(), p.as_content_id());
    assert_ne!(
        std::any::TypeId::of::<UnitId>(),
        std::any::TypeId::of::<PacketId>(),
        "aliases would make these one type, and a packet id would fit a unit slot"
    );
}
