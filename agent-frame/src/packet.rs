//! Packets: the chain that makes "never derive from a derivation" a shape.
//!
//! A packet names its units and its predecessor. Identity is over the **whole
//! body** — prior link included — so a packet cannot be re-parented without
//! changing its id, and a chain cannot be silently re-rooted.
//!
//! `prior: None` is genesis. That is the same fact the `--hermetic` flag states
//! from the other direction: a hermetic run is one that always starts at
//! genesis, and a resumable frame is one whose packet has a parent. The flag
//! and the data structure say the same thing, so there is no third state to get
//! out of sync.

use content_addressable::{ContentAddressable, ContentError, ContentId};
use serde::{Deserialize, Serialize};

/// The content address of a [`crate::Unit`].
pub type UnitId = ContentId;

/// The content address of a [`Packet`].
pub type PacketId = ContentId;

/// One sealed packet: an ordered list of units, rooted in a predecessor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packet {
    /// The predecessor packet, or `None` for genesis.
    ///
    /// Part of the hashed body: a packet's id binds the chain it belongs to.
    pub prior: Option<PacketId>,
    /// The units this packet seals, in order.
    pub units: Vec<UnitId>,
}

impl Packet {
    /// A packet with no predecessor.
    #[must_use]
    pub const fn genesis(units: Vec<UnitId>) -> Self {
        Self { prior: None, units }
    }

    /// A packet linking `prior` as its predecessor.
    #[must_use]
    pub const fn following(prior: PacketId, units: Vec<UnitId>) -> Self {
        Self {
            prior: Some(prior),
            units,
        }
    }

    /// Whether this packet starts a chain.
    ///
    /// A genesis packet is not resumable: there is nothing behind it to replay
    /// from.
    #[must_use]
    pub const fn is_genesis(&self) -> bool {
        self.prior.is_none()
    }
}

impl ContentAddressable for Packet {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}
