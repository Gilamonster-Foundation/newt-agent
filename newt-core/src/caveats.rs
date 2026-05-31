//! `Caveats` — the local mirror of agent-mesh's attenuated authority lattice.
//!
//! This is the **enforcement-side** copy of the algebra defined in
//! `agent_mesh_core::caveats`. agent-mesh owns the *signed wire format*
//! (the cert chain carries one of these on every `AgentMetadata`); this
//! module owns the *consult-it-from-the-tool-dispatch-site* surface that
//! `newt-coder` and the rest of the in-workspace crates use to decide
//! whether a given tool call is permitted.
//!
//! The two types are structurally identical — same field names, same
//! variant shapes, same `top` / `leq` / `meet` semantics — so the
//! `newt-mesh` crate (which depends on both) can trivially convert one
//! into the other at the worker boundary. We deliberately re-define them
//! here (instead of pulling in agent-mesh) because `newt-mesh` is
//! [intentionally excluded from the default workspace][1]: depending on
//! `agent-mesh-protocol` from a workspace member would break CI.
//!
//! [1]: ../../docs/decisions/mesh_integration.md
//!
//! # Why a lattice
//!
//! See `docs/decisions/agentic_object_capability_security.md`. The short
//! version: delegation must be **attenuation-only** so a confused or
//! compromised sub-principal cannot amplify beyond the authority it was
//! minted with. That property is structural — it falls out of `meet`
//! being the only composition operator and `meet` never being able to
//! produce something above its operands.
//!
//! # What `35b` adds on top of the algebra
//!
//! Per-axis "permits this concrete item?" helpers ([`Scope::permits`],
//! [`Caveats::permits_fs_write`], etc.) — the actual question the
//! dispatch sites in `newt-coder` ask. These don't change the algebra at
//! all; they're convenience adaptors so the call sites can read like
//! prose.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A set-valued authority axis: either unrestricted (`All`, the top of this
/// axis) or exactly the listed items.
///
/// Membership at this layer is **exact**. Path-prefix or host-suffix matching
/// is an enforcement concern that belongs further down the stack (Landlock,
/// network namespace); the lattice itself just deals with set inclusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope<T: Ord + Clone> {
    /// Unrestricted — authorizes every item. The `⊤` of this axis.
    All,
    /// Authorizes exactly the items in the set.
    Only(BTreeSet<T>),
}

impl<T: Ord + Clone> Scope<T> {
    /// The top of this axis (`All`, unrestricted).
    #[must_use]
    pub fn top() -> Self {
        Self::All
    }

    /// The empty authority on this axis — authorizes nothing.
    #[must_use]
    pub fn none() -> Self {
        Self::Only(BTreeSet::new())
    }

    /// Build a bounded scope from an iterator of items.
    pub fn only<I: IntoIterator<Item = T>>(items: I) -> Self {
        Self::Only(items.into_iter().collect())
    }

    /// `self ⊑ other` — does `self` authorize no more than `other`?
    #[must_use]
    pub fn leq(&self, other: &Self) -> bool {
        match (self, other) {
            (_, Self::All) => true,
            (Self::All, Self::Only(_)) => false,
            (Self::Only(a), Self::Only(b)) => a.is_subset(b),
        }
    }

    /// `self ⊓ other` — the greatest lower bound. `All` is the identity;
    /// otherwise intersection.
    #[must_use]
    pub fn meet(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, x) | (x, Self::All) => x.clone(),
            (Self::Only(a), Self::Only(b)) => Self::Only(a.intersection(b).cloned().collect()),
        }
    }

    /// Does this scope authorize `item`?
    ///
    /// `All` permits everything; `Only(s)` permits exactly the members of `s`.
    /// This is the "yes/no" form the tool dispatch sites consult.
    #[must_use]
    pub fn permits(&self, item: &T) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => set.contains(item),
        }
    }
}

/// A numeric upper bound axis (e.g. "at most N tool calls").
///
/// `Unlimited` is the top; `AtMost(n) ⊑ AtMost(m) ⟺ n ≤ m`. The meet is the
/// tighter (smaller) bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountBound {
    /// No bound — the `⊤` of this axis.
    Unlimited,
    /// At most this many.
    AtMost(u64),
}

impl CountBound {
    /// The top of this axis (`Unlimited`).
    #[must_use]
    pub fn top() -> Self {
        Self::Unlimited
    }

    /// `self ⊑ other` — is `self` at least as tight a bound as `other`?
    #[must_use]
    pub fn leq(&self, other: &Self) -> bool {
        match (self, other) {
            (_, Self::Unlimited) => true,
            (Self::Unlimited, Self::AtMost(_)) => false,
            (Self::AtMost(a), Self::AtMost(b)) => a <= b,
        }
    }

    /// `self ⊓ other` — the tighter bound.
    #[must_use]
    pub fn meet(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Unlimited, x) | (x, Self::Unlimited) => *x,
            (Self::AtMost(a), Self::AtMost(b)) => Self::AtMost((*a).min(*b)),
        }
    }

    /// Does this bound permit one more call when `used_so_far` calls have
    /// already been counted against it?
    ///
    /// `Unlimited` always permits; `AtMost(n)` permits iff `used_so_far < n`.
    #[must_use]
    pub fn permits_one_more(&self, used_so_far: u64) -> bool {
        match self {
            Self::Unlimited => true,
            Self::AtMost(n) => used_so_far < *n,
        }
    }
}

/// The capability set an agent holds — one element of the authority
/// meet-semilattice. See the [module docs](crate::caveats).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caveats {
    /// Filesystem paths the agent may read.
    pub fs_read: Scope<String>,
    /// Filesystem paths the agent may write.
    pub fs_write: Scope<String>,
    /// Commands the agent may execute.
    pub exec: Scope<String>,
    /// Network hosts the agent may reach.
    pub net: Scope<String>,
    /// Upper bound on tool calls this authority permits.
    pub max_calls: CountBound,
    /// Generation counters this authority is valid for (causal, not
    /// wall-clock — a caveat keys on "flight N", never on time).
    pub valid_for_generation: Scope<u64>,
}

impl Caveats {
    /// `⊤` — unrestricted authority on every axis. Equivalent to "no caveats",
    /// i.e. the user's full authority. This is the back-compat default the
    /// existing call sites pass while 35c (per-peer attenuation) is in flight.
    #[must_use]
    pub fn top() -> Self {
        Self {
            fs_read: Scope::top(),
            fs_write: Scope::top(),
            exec: Scope::top(),
            net: Scope::top(),
            max_calls: CountBound::top(),
            valid_for_generation: Scope::top(),
        }
    }

    /// `self ⊑ other` — does `self` grant no more authority than `other` on
    /// *every* axis?
    #[must_use]
    pub fn leq(&self, other: &Self) -> bool {
        self.fs_read.leq(&other.fs_read)
            && self.fs_write.leq(&other.fs_write)
            && self.exec.leq(&other.exec)
            && self.net.leq(&other.net)
            && self.max_calls.leq(&other.max_calls)
            && self.valid_for_generation.leq(&other.valid_for_generation)
    }

    /// `self ⊓ other` — the greatest lower bound, axis by axis.
    #[must_use]
    pub fn meet(&self, other: &Self) -> Self {
        Self {
            fs_read: self.fs_read.meet(&other.fs_read),
            fs_write: self.fs_write.meet(&other.fs_write),
            exec: self.exec.meet(&other.exec),
            net: self.net.meet(&other.net),
            max_calls: self.max_calls.meet(&other.max_calls),
            valid_for_generation: self.valid_for_generation.meet(&other.valid_for_generation),
        }
    }

    /// Does this authority permit reading `path`?
    ///
    /// `path` is matched by exact string equality against the members of
    /// `fs_read` (or `All` accepts everything). Path-prefix semantics are
    /// out of scope at this layer — see the module docs.
    #[must_use]
    pub fn permits_fs_read(&self, path: &str) -> bool {
        self.fs_read.permits(&path.to_string())
    }

    /// Does this authority permit writing `path`?
    #[must_use]
    pub fn permits_fs_write(&self, path: &str) -> bool {
        self.fs_write.permits(&path.to_string())
    }

    /// Does this authority permit executing `cmd`?
    #[must_use]
    pub fn permits_exec(&self, cmd: &str) -> bool {
        self.exec.permits(&cmd.to_string())
    }

    /// Does this authority permit a network call to `host`?
    #[must_use]
    pub fn permits_net(&self, host: &str) -> bool {
        self.net.permits(&host.to_string())
    }
}

impl Default for Caveats {
    /// Absence of caveats is `⊤` (unrestricted) — the back-compatible default
    /// so existing call sites that don't yet know about caveats keep today's
    /// behavior.
    fn default() -> Self {
        Self::top()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_all_permits_everything() {
        let s: Scope<String> = Scope::All;
        assert!(s.permits(&"anything".to_string()));
        assert!(s.permits(&"".to_string()));
    }

    #[test]
    fn scope_only_permits_exact_members() {
        let s = Scope::only(["a".to_string(), "b".to_string()]);
        assert!(s.permits(&"a".to_string()));
        assert!(s.permits(&"b".to_string()));
        assert!(!s.permits(&"c".to_string()));
        assert!(!s.permits(&"".to_string()));
    }

    #[test]
    fn scope_none_permits_nothing() {
        let s: Scope<String> = Scope::none();
        assert!(!s.permits(&"a".to_string()));
    }

    #[test]
    fn scope_lattice_order() {
        let small = Scope::only(["a".to_string()]);
        let big = Scope::only(["a".to_string(), "b".to_string()]);
        assert!(small.leq(&big));
        assert!(small.leq(&Scope::All));
        assert!(!Scope::<String>::All.leq(&small));
        assert!(!big.leq(&small));
    }

    #[test]
    fn scope_meet_intersects() {
        let a = Scope::only(["a".to_string(), "b".to_string()]);
        let b = Scope::only(["b".to_string(), "c".to_string()]);
        assert_eq!(a.meet(&b), Scope::only(["b".to_string()]));
        assert_eq!(a.meet(&Scope::All), a);
    }

    #[test]
    fn count_bound_permits_one_more() {
        assert!(CountBound::Unlimited.permits_one_more(0));
        assert!(CountBound::Unlimited.permits_one_more(99_999));
        assert!(CountBound::AtMost(3).permits_one_more(0));
        assert!(CountBound::AtMost(3).permits_one_more(2));
        assert!(!CountBound::AtMost(3).permits_one_more(3));
        assert!(!CountBound::AtMost(3).permits_one_more(99));
        // Edge: AtMost(0) refuses immediately.
        assert!(!CountBound::AtMost(0).permits_one_more(0));
    }

    #[test]
    fn count_bound_meet_is_tighter() {
        assert_eq!(
            CountBound::AtMost(5).meet(&CountBound::AtMost(3)),
            CountBound::AtMost(3)
        );
        assert_eq!(
            CountBound::Unlimited.meet(&CountBound::AtMost(7)),
            CountBound::AtMost(7)
        );
    }

    #[test]
    fn caveats_top_permits_everything() {
        let c = Caveats::top();
        assert!(c.permits_fs_read("/anywhere"));
        assert!(c.permits_fs_write("/anywhere"));
        assert!(c.permits_exec("rm"));
        assert!(c.permits_net("evil.example.com"));
        assert!(c.max_calls.permits_one_more(1_000_000));
    }

    #[test]
    fn caveats_default_is_top() {
        assert_eq!(Caveats::default(), Caveats::top());
    }

    #[test]
    fn caveats_attenuated_denies_outside_scope() {
        let c = Caveats {
            fs_write: Scope::only(["allowed.rs".to_string()]),
            net: Scope::only(["allowed.example.com".to_string()]),
            max_calls: CountBound::AtMost(2),
            ..Caveats::top()
        };
        assert!(c.permits_fs_write("allowed.rs"));
        assert!(!c.permits_fs_write("forbidden.rs"));
        assert!(c.permits_net("allowed.example.com"));
        assert!(!c.permits_net("evil.example.com"));
        assert!(c.max_calls.permits_one_more(1));
        assert!(!c.max_calls.permits_one_more(2));
    }

    #[test]
    fn caveats_leq_per_axis() {
        let restricted = Caveats {
            fs_write: Scope::only(["a.rs".to_string()]),
            ..Caveats::top()
        };
        assert!(restricted.leq(&Caveats::top()));
        assert!(!Caveats::top().leq(&restricted));
    }

    #[test]
    fn caveats_meet_attenuates_each_axis() {
        let a = Caveats {
            fs_read: Scope::only(["/repo".to_string(), "/tmp".to_string()]),
            max_calls: CountBound::AtMost(10),
            ..Caveats::top()
        };
        let b = Caveats {
            fs_read: Scope::only(["/repo".to_string()]),
            max_calls: CountBound::AtMost(4),
            ..Caveats::top()
        };
        let m = a.meet(&b);
        assert_eq!(m.fs_read, Scope::only(["/repo".to_string()]));
        assert_eq!(m.max_calls, CountBound::AtMost(4));
        assert!(m.leq(&a) && m.leq(&b));
    }

    #[test]
    fn caveats_serde_roundtrip() {
        let c = Caveats {
            exec: Scope::only(["git".to_string(), "cargo".to_string()]),
            max_calls: CountBound::AtMost(3),
            valid_for_generation: Scope::only([42u64]),
            ..Caveats::top()
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: Caveats = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }
}
