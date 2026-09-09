//! Units: what a derived thing is, and what it owes its source.
//!
//! Mirrors `formal/ContextOps/Basic.lean`. Two properties are structural rather
//! than documented:
//!
//! * **Every field is private and [`Unit::seal`] is the only mint.** A caller
//!   cannot hand-build a `Unit` at depth 2, or an unaddressed one.
//! * **v0 mints elision only.** [`Op::Concise`] and [`Op::Generate`] exist in
//!   the type — the tables in [`crate::op`] are exhaustive — but the constructor
//!   refuses them. `checkOf .concise = .ground` leaves concision inside the
//!   fence with a verification class that has no implementation and no
//!   calibrated threshold; a mintable unit whose check nobody can run is
//!   "unverified evidence is indistinguishable from no evidence" reproduced
//!   inside the product.

use content_addressable::{ContentAddressable, ContentError};
use serde::{Deserialize, Serialize};

use crate::op::Op;

/// The three kinds of thing that can root a unit's provenance.
///
/// Note what is absent: there is **no** `ModelOutput` constructor. A model's
/// assertion is never a provenance root, and that is enforced by the type
/// rather than by a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Root {
    /// The human SAID: intent expressed in language. Needs adjudication.
    OperatorPrompt,
    /// The human DID: intent expressed as control. Unambiguous by construction.
    UserAction,
    /// The machine decided: a budget threshold, a retry, an automatic seal.
    HarnessEvent,
}

/// Lifecycle of a root.
///
/// Operator intent drifts ("do Y instead"), and tracking that per-unit is
/// intractable; tracking it at the ROOT is not, and everything downstream
/// inherits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Life {
    /// The root still stands. Nothing downstream is freely evictable.
    Live,
    /// The operator replaced this intent ("do Y instead").
    Superseded,
    /// The operator retracted this intent outright.
    Withdrawn,
}

/// Why a seal was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    /// v0 mints elision only. The operation exists in the type; the constructor
    /// refuses it until its verification class has an implementation.
    #[error(
        "v0 mints elision only: `{op:?}` has verification class `{check:?}`, which has \
         no implementation and no calibrated threshold. Minting it would produce a unit \
         whose check nobody can run."
    )]
    NotMintableInV0 {
        /// The operation the caller asked to seal.
        op: Op,
        /// Its verification class — the reason the refusal is principled rather
        /// than a not-implemented-yet.
        check: crate::op::Check,
    },
}

/// A single unit of compacted context.
///
/// `depth` is generation provenance depth: 0 for material taken from source,
/// and one more for each generation applied over a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    op: Op,
    depth: u32,
    root: Option<Root>,
    life: Life,
    addressed: bool,
}

impl Unit {
    /// Seal one unit from SOURCE material (depth 0).
    ///
    /// This is the only constructor, and it is why depth cannot reach 2: there
    /// is no way to seal a unit from a previous packet's prose.
    ///
    /// # Errors
    ///
    /// [`SealError::NotMintableInV0`] for any operation other than
    /// [`Op::Elide`] — see the module docs.
    pub fn seal(op: Op, root: Option<Root>) -> Result<Self, SealError> {
        match op {
            Op::Elide => Ok(Self {
                op,
                depth: op.depth_after(0),
                root,
                life: Life::Live,
                addressed: true,
            }),
            Op::Concise => Err(SealError::NotMintableInV0 {
                op,
                check: op.check(),
            }),
            Op::Generate => Err(SealError::NotMintableInV0 {
                op,
                check: op.check(),
            }),
        }
    }

    /// What was done to the source material.
    #[must_use]
    pub const fn op(&self) -> Op {
        self.op
    }

    /// Generation provenance depth: 0 for source material, one more per generation.
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// Why this unit exists. `None` means it is fabricated.
    #[must_use]
    pub const fn root(&self) -> Option<Root> {
        self.root
    }

    /// The lifecycle of this unit's root.
    #[must_use]
    pub const fn life(&self) -> Life {
        self.life
    }

    /// Everything sealed is addressed — the precondition for elision to be
    /// redeemable.
    #[must_use]
    pub const fn addressed(&self) -> bool {
        self.addressed
    }

    /// The depth bound: `depth <= 1`.
    ///
    /// A summary of a summary is depth 2 and is unreachable through [`seal`],
    /// so this predicate is a check on decoded data, not on anything this crate
    /// can mint.
    ///
    /// [`seal`]: Unit::seal
    #[must_use]
    pub const fn is_well_formed(&self) -> bool {
        self.depth <= 1
    }

    /// A unit with no root is fabricated — it exists because of nothing that
    /// happened.
    #[must_use]
    pub const fn fabricated(&self) -> bool {
        self.root.is_none()
    }

    /// A unit is freely evictable when its root is no longer live.
    ///
    /// Deterministic: no grounding check, no model call, no judgement. When the
    /// operator says "do Y instead", everything that existed only to serve X
    /// becomes evictable by construction, regardless of which operation
    /// produced it.
    #[must_use]
    pub const fn evictable(&self) -> bool {
        match self.life {
            Life::Live => false,
            Life::Superseded => true,
            Life::Withdrawn => true,
        }
    }

    /// Mark this unit's root as no longer live.
    #[must_use]
    pub const fn with_life(mut self, life: Life) -> Self {
        self.life = life;
        self
    }
}

impl ContentAddressable for Unit {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}
