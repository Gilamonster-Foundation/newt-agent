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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Packet(MerkleNode<PacketBody>);

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

impl ContentAddressable for Packet {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        self.0.canonical_form()
    }
}
