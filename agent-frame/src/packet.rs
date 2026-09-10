//! Packets: the chain that makes "never derive from a derivation" a shape.
//!
//! # Reusing the Merkle node rather than re-minting one
//!
//! An earlier revision hand-rolled `Packet { prior: Option<PacketId>, units }`
//! and hashed it. That is a hand-rolled Merkle node — a payload plus a parent
//! link, with identity over both — in a workspace whose first-principle rule
//! says every persisted structure derives its identity through
//! `content-addressable`, and whose own decision record cited that rule while
//! breaking it.
//!
//! A packet is now [`MerkleNode<PacketBody>`]: the predecessor is a **parent
//! link**, which is what a parent link is for, and the id is minted by the
//! crate that owns id-minting. Genesis is the empty parent set — the same fact
//! the old `prior: None` encoded, expressed in the vocabulary the rest of the
//! line already speaks.
//!
//! # Borrowing a DAG node does not make v0 a DAG
//!
//! [`MerkleNode`] carries a parent *set*, because a Merkle DAG node does. v0
//! mints a **chain**: the referent is `formal/ContextOps/Basic.lean`, whose
//! `Chain` has exactly two constructors — `genesis (units)` and
//! `sealed (prior : Chain) (units)`. There is no multi-parent constructor to
//! correspond to.
//!
//! So [`Packet`] does **not** implement [`serde::Deserialize`], for the same
//! reason [`crate::Unit`] does not. Foreign bytes decode to [`RawPacket`] — an
//! ordinary untrusted DAG node — and cross into a `Packet` only through the
//! fallible [`TryFrom`] below, which admits zero parents or exactly one.
//!
//! The state that closes is a real one, not a hypothetical: a decoded
//! two-parent node satisfied `is_genesis() == false` *and* `prior() == None`
//! simultaneously. Neither constructor can mint that, and `newt frame parents`
//! rendered it as `outcome: "genesis"` — "no parents: this frame is an origin,
//! and the chain ends here" — of a packet with two. A forensic surface that
//! reports the absence of a link it silently dropped is worse than one that
//! refuses to answer.

use std::collections::BTreeSet;

use content_addressable::{ContentAddressable, ContentError, ContentId, MerkleNode};
use serde::{Deserialize, Serialize};

/// The content address of a [`crate::Unit`].
///
/// A **newtype**, not an alias. As aliases, `UnitId` and `PacketId` were the
/// same type, so a packet id fitted a unit slot silently and the compiler had
/// nothing to say about it. Provenance that can be assembled out of the wrong
/// kind of address is not provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnitId(ContentId);

/// The content address of a [`Packet`]. A newtype for the same reason as
/// [`UnitId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PacketId(ContentId);

macro_rules! id_newtype {
    ($t:ident, $what:literal) => {
        impl $t {
            #[doc = concat!("Borrow the underlying content id of this ", $what, ".")]
            #[must_use]
            pub const fn as_content_id(&self) -> &ContentId {
                &self.0
            }

            #[doc = concat!("Consume this ", $what, " id, yielding the content id.")]
            #[must_use]
            pub const fn into_content_id(self) -> ContentId {
                self.0
            }
        }

        impl From<ContentId> for $t {
            fn from(id: ContentId) -> Self {
                Self(id)
            }
        }

        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl std::str::FromStr for $t {
            type Err = <ContentId as std::str::FromStr>::Err;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                s.parse::<ContentId>().map(Self)
            }
        }
    };
}

id_newtype!(UnitId, "unit");
id_newtype!(PacketId, "packet");

/// What a packet carries: the units it seals, in order.
///
/// The predecessor is **not** here — it is the node's parent link. Putting it
/// in the payload as well would be two encodings of one fact, which is how a
/// chain gets silently re-rooted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacketBody {
    /// The units this packet seals, in order.
    pub units: Vec<UnitId>,
}

/// One sealed packet: an ordered list of units, rooted in a predecessor.
///
/// Identity is over the whole node — payload and parent links together — so a
/// packet cannot be re-parented without changing its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Packet(MerkleNode<PacketBody>);

/// The transparent decode target: **an untrusted DAG node**.
///
/// Deliberately the [`MerkleNode`] itself rather than a fresh DTO. A `RawUnit`
/// had to be minted because a `Unit` is not structurally a plain record of its
/// own fields; a raw packet *is* exactly a Merkle node with a `PacketBody`
/// payload, and standing a second type up beside one that already has the right
/// shape and the right wire form is the sprawl this crate is written against.
///
/// It asserts nothing: the parent set may hold any number of links, which is
/// what gives `TryFrom<RawPacket> for Packet` something to refuse.
pub type RawPacket = MerkleNode<PacketBody>;

/// Why a decoded packet was refused admission.
///
/// Named apart from [`crate::AdmitError`] rather than folded into it: a reader
/// of a log needs to know whether the thing that failed the v0 contract was a
/// unit or a packet, and a shared enum answers that only by which variant fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PacketAdmitError {
    /// The node has more than one parent. v0 mints a chain, not a DAG.
    #[error(
        "decoded packet has {parents} parents: v0 mints a CHAIN, so a packet has zero \
         parents (genesis) or exactly one. A node with several has no single `prior`, \
         so it is neither genesis nor following — a third state no constructor can mint \
         and no forensic reader can name."
    )]
    NotAChain {
        /// How many parent links the decoded node carried.
        parents: usize,
    },
}

impl Packet {
    /// A packet with no predecessor.
    #[must_use]
    pub fn genesis(units: Vec<UnitId>) -> Self {
        Self(MerkleNode::genesis(PacketBody { units }))
    }

    /// A packet linking `prior` as its predecessor.
    #[must_use]
    pub fn following(prior: PacketId, units: Vec<UnitId>) -> Self {
        Self(MerkleNode::new(
            PacketBody { units },
            [prior.into_content_id()],
        ))
    }

    /// The units this packet seals, in order.
    #[must_use]
    pub fn units(&self) -> &[UnitId] {
        &self.0.payload().units
    }

    /// The predecessor links.
    #[must_use]
    pub fn parents(&self) -> &BTreeSet<ContentId> {
        self.0.parents()
    }

    /// The single predecessor, when there is exactly one.
    ///
    /// v0 mints chains, so a packet has zero parents (genesis) or one.
    #[must_use]
    pub fn prior(&self) -> Option<PacketId> {
        let mut it = self.0.parents().iter();
        match (it.next(), it.next()) {
            (Some(one), None) => Some(PacketId::from(*one)),
            _ => None,
        }
    }

    /// Whether this packet starts a chain.
    #[must_use]
    pub fn is_genesis(&self) -> bool {
        self.0.parents().is_empty()
    }

    /// This packet's address.
    ///
    /// # Errors
    ///
    /// Propagates an encoding failure from the canonical form.
    pub fn id(&self) -> Result<PacketId, ContentError> {
        self.0.id().map(PacketId::from)
    }
}

impl TryFrom<RawPacket> for Packet {
    type Error = PacketAdmitError;

    /// **The only way in from foreign bytes.**
    ///
    /// One rule, because the chain has one shape: at most one parent. Nothing
    /// else about a decoded packet is a claim this layer can check — whether the
    /// units it names exist, or admit, needs a store, and inventing one here
    /// would be v0 growing a storage service to answer a structural question.
    ///
    /// # Errors
    ///
    /// [`PacketAdmitError::NotAChain`] when the node carries two or more parents.
    fn try_from(raw: RawPacket) -> Result<Self, Self::Error> {
        let parents = raw.parents().len();
        if parents > 1 {
            return Err(PacketAdmitError::NotAChain { parents });
        }
        Ok(Self(raw))
    }
}

impl ContentAddressable for Packet {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        self.0.canonical_form()
    }
}
