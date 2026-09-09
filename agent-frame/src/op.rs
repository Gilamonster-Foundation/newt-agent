//! The operation algebra, mirrored from `formal/ContextOps/Basic.lean`.
//!
//! Every table here is exhaustive and carries **no wildcard arm**. That is
//! deliberate: a `_` arm silently absorbs a future variant, and the whole point
//! of this kernel is that adding an operation forces you to answer what it
//! asserts, how it is checked, and whether it raises depth.
//!
//! Substitution — the untyped fusion of all three — is deliberately **not** a
//! constructor. This kernel exists to make it unrepresentable.

use serde::{Deserialize, Serialize};

/// What was done to the source material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Fewer words, same referents. Asserts nothing new; extractively checkable.
    Concise,
    /// Removed, marked, addressed. Asserts nothing at all.
    Elide,
    /// New sentences not present in the source. The only fabrication surface.
    Generate,
}

/// How a unit can be verified by a party that did not run the build — the
/// supply-chain reproducibility boundary.
///
/// Sealing invokes a model, so a packet is an artifact of a NON-reproducible
/// build and verification cannot rest on re-derivation alone. The fidelity
/// typing hands us the boundary for free: **only elision is deterministic.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// Deterministic selection: recompute and compare.
    Rederive,
    /// Model-generated but extractive: check against the sealed source.
    Ground,
    /// Model-generated and non-reproducible: grounding plus an attestation.
    GroundAttest,
}

impl Op {
    /// Whether this operation makes a content claim about the material it
    /// replaces.
    ///
    /// Elision is the only one that does not, which is why it is the safe
    /// default whenever grounding is uncertain.
    #[must_use]
    pub const fn asserts(self) -> bool {
        match self {
            Op::Concise => true,
            Op::Elide => false,
            Op::Generate => true,
        }
    }

    /// The verifiability class of this operation.
    #[must_use]
    pub const fn check(self) -> Check {
        match self {
            Op::Elide => Check::Rederive,
            Op::Concise => Check::Ground,
            Op::Generate => Check::GroundAttest,
        }
    }

    /// The depth produced by applying this operation to material already at
    /// depth `d`.
    ///
    /// Concision and elision never raise generation depth; only generation
    /// does. This is the arithmetic core of the depth bound.
    #[must_use]
    pub const fn depth_after(self, d: u32) -> u32 {
        match self {
            Op::Generate => d.saturating_add(1),
            Op::Concise => d,
            Op::Elide => d,
        }
    }
}
