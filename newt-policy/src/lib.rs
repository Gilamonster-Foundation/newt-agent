//! The policy layer: what an agent may do, and the words for asking.
//!
//! Caveats (the attenuated authority lattice and its checks), permission
//! requests and decisions, danger tiers, postures and permission presets, and
//! the path and network rules they rest on. Nothing here runs a turn, stores a
//! conversation or reads a user's settings, so a crate can depend on this one
//! without the rest of `newt-core`.
//!
//! One crate boundary replaces the module-level gate that kept these modules
//! apart (#3140): this crate cannot name `newt-core`.

pub mod caveats;
pub mod danger;
#[cfg(unix)]
pub mod fs_aliases;
pub mod netguard;
pub mod paths;
pub mod permission_challenge;
pub mod permissions;
pub mod posture;
pub mod preset;

pub use caveats::{permits_path, CaveatsExt, CountBoundExt, ScopeExt};
pub use permissions::{
    append_denial, load_denials, widen_caveats, DenialKind, HumanQuestionOutcome, PermissionAction,
    PermissionDecision, PermissionGate, PermissionRecord, PermissionRequest, Verdict,
    BOUND_REASON_PREFIX,
};
pub use preset::{CaveatProfile, NamedPermissionPreset, ScopeKeyword, ScopeSpec};
