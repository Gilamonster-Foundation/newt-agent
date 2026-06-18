//! `GitCaveats` — the custom OCAP capability surface for the embedded git tool.
//!
//! Git authority does not map onto the five [`Caveats`](crate::caveats::Caveats)
//! axes (`fs_*`/`exec`/`net` are path/program/host granular; git's boundaries are
//! *operation-class* and *ref/remote-scoped*). So `GitCaveats` is a **separate**
//! small lattice, composed *alongside* the signed `Caveats` wire type — never
//! merged into it (`Caveats` is the signed agent-mesh type, issue #95). It mirrors
//! the same `top`/`leq`/`meet` algebra, so it can only **attenuate**, never
//! amplify, and composes by intersection.
//!
//! The embedded git tool runs an operation iff `GitCaveats` permits the
//! operation *class* AND the session `Caveats` permits the underlying fs/net
//! effect — defense-in-depth by `meet` across two lattices (the tool layer gates
//! the repo path via `permits_fs_*`; this type gates the git op).
//!
//! **Network is deferred / fail-closed.** `remote`/`fetch`/`push`/`clone` are NOT
//! part of [`GitCaveats::top`]: remote git carries credentials + pulls untrusted
//! bytes, exactly what the OCAP deviation ratchet's `b1-os-isolation` /
//! `disclosure-gate-live-path` disable while open (`docs/security/ocap-deviations.md`).
//! The engine layer adds the `OCAP-GATE` fail-closed check; this type keeps the
//! capability off by default so "full local git" never implies any network reach.

use crate::caveats::{Caveats, Scope, ScopeExt};
use serde::{Deserialize, Serialize};

/// `a ⊑ b` for a boolean op-gate: `a` grants no more than `b` (false ⊑ true).
#[inline]
fn gate_leq(a: bool, b: bool) -> bool {
    !a || b
}

/// A capability surface for the embedded git engine. Composed with — never merged
/// into — the session [`Caveats`]. Every axis is a lattice element; the effective
/// surface is always `a.meet(&b)` (attenuation only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCaveats {
    /// Read history: `status`/`log`/`diff`/`show`/`blame`. The floor capability.
    pub read: bool,
    /// Mutate the index / worktree locally: `add`/`reset`/`restore`/`stash`.
    pub stage: bool,
    /// Create commits in the LOCAL object store (`commit`/`commit --amend`).
    pub commit_local: bool,
    /// Ref lifecycle (`branch`/`tag`/`update-ref`), scoped by ref-name pattern.
    pub refs: Scope<String>,
    /// Remote operations scoped by remote NAME (`origin`, `upstream`, …).
    /// **Default `none`** — the network axis is deferred / fail-closed.
    pub remote: Scope<String>,
    /// Remote verb: may `fetch` (read from a remote). Default off.
    pub fetch: bool,
    /// Remote verb: may `push` (write to a remote). Default off.
    pub push: bool,
    /// Remote verb: may `clone` a remote. Default off.
    pub clone: bool,
}

impl GitCaveats {
    /// `⊤` for the **local** surface: all local ops allowed, all refs, but the
    /// **network axis stays closed** (`remote = none`, verbs off). Network is not
    /// part of "top" — it ships fail-closed under the OCAP deviation ratchet.
    #[must_use]
    pub fn top() -> Self {
        Self {
            read: true,
            stage: true,
            commit_local: true,
            refs: Scope::top(),
            remote: Scope::none(),
            fetch: false,
            push: false,
            clone: false,
        }
    }

    /// A read-only surface: history only, no mutation, no network. The natural
    /// reviewer/triage grant.
    #[must_use]
    pub fn read_only() -> Self {
        Self {
            read: true,
            stage: false,
            commit_local: false,
            refs: Scope::none(),
            remote: Scope::none(),
            fetch: false,
            push: false,
            clone: false,
        }
    }

    /// `⊥` — no git authority at all (every gate denied).
    #[must_use]
    pub fn none() -> Self {
        Self {
            read: false,
            stage: false,
            commit_local: false,
            refs: Scope::none(),
            remote: Scope::none(),
            fetch: false,
            push: false,
            clone: false,
        }
    }

    /// `self ⊑ other` — does `self` grant no more than `other` on every axis?
    /// The attenuation check (a delegated git surface must be `⊑` its parent).
    #[must_use]
    pub fn leq(&self, other: &Self) -> bool {
        gate_leq(self.read, other.read)
            && gate_leq(self.stage, other.stage)
            && gate_leq(self.commit_local, other.commit_local)
            && self.refs.leq(&other.refs)
            && self.remote.leq(&other.remote)
            && gate_leq(self.fetch, other.fetch)
            && gate_leq(self.push, other.push)
            && gate_leq(self.clone, other.clone)
    }

    /// `self ⊓ other` — the greatest lower bound, axis by axis. How two git
    /// surfaces compose; it can never amplify.
    #[must_use]
    pub fn meet(&self, other: &Self) -> Self {
        Self {
            read: self.read && other.read,
            stage: self.stage && other.stage,
            commit_local: self.commit_local && other.commit_local,
            refs: self.refs.meet(&other.refs),
            remote: self.remote.meet(&other.remote),
            fetch: self.fetch && other.fetch,
            push: self.push && other.push,
            clone: self.clone && other.clone,
        }
    }

    /// Derive a git surface from the session [`Caveats`] (the zero-config MVP
    /// wiring): readable always (history is low-risk and the tool layer still
    /// gates the repo path via `fs_read`); local mutation allowed iff the session
    /// can write *somewhere* (`fs_write` is non-empty); network always denied
    /// (deferred). A config-declared clamp can attenuate this further later.
    #[must_use]
    pub fn from_session(c: &Caveats) -> Self {
        let can_write = match &c.fs_write {
            Scope::All => true,
            Scope::Only(set) => !set.is_empty(),
        };
        Self {
            read: true,
            stage: can_write,
            commit_local: can_write,
            refs: if can_write {
                Scope::top()
            } else {
                Scope::none()
            },
            remote: Scope::none(),
            fetch: false,
            push: false,
            clone: false,
        }
    }

    // --- enforcement adaptors (read like prose at the dispatch site) ---

    #[must_use]
    pub fn permits_read(&self) -> bool {
        self.read
    }
    #[must_use]
    pub fn permits_stage(&self) -> bool {
        self.stage
    }
    #[must_use]
    pub fn permits_commit(&self) -> bool {
        self.commit_local
    }
    /// Does this surface permit creating/writing the ref `name` (e.g. `refs/heads/feat/x`)?
    #[must_use]
    pub fn permits_ref(&self, name: &str) -> bool {
        self.refs.permits(&name.to_string())
    }
    /// Does this surface permit operating on the remote named `name`?
    #[must_use]
    pub fn permits_remote(&self, name: &str) -> bool {
        self.remote.permits(&name.to_string())
    }
    #[must_use]
    pub fn permits_fetch(&self, remote: &str) -> bool {
        self.fetch && self.permits_remote(remote)
    }
    #[must_use]
    pub fn permits_push(&self, remote: &str) -> bool {
        self.push && self.permits_remote(remote)
    }
    #[must_use]
    pub fn permits_clone(&self) -> bool {
        self.clone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_allows_local_but_no_network() {
        let g = GitCaveats::top();
        assert!(g.permits_read() && g.permits_stage() && g.permits_commit());
        assert!(g.permits_ref("refs/heads/anything"));
        // Network axis is closed even at "top".
        assert!(!g.permits_push("origin"));
        assert!(!g.permits_fetch("origin"));
        assert!(!g.permits_clone());
        assert!(!g.permits_remote("origin"));
    }

    #[test]
    fn read_only_denies_all_mutation() {
        let g = GitCaveats::read_only();
        assert!(g.permits_read());
        assert!(!g.permits_stage());
        assert!(!g.permits_commit());
        assert!(!g.permits_ref("refs/heads/x"));
        assert!(!g.permits_push("origin"));
    }

    #[test]
    fn meet_attenuates_never_amplifies() {
        let local = GitCaveats::top();
        let ro = GitCaveats::read_only();
        let m = local.meet(&ro);
        // meet with read-only collapses to read-only authority.
        assert!(m.permits_read());
        assert!(!m.permits_commit());
        assert!(!m.permits_ref("refs/heads/x"));
        // meet result is ⊑ both operands.
        assert!(m.leq(&local) && m.leq(&ro));
    }

    #[test]
    fn leq_is_the_attenuation_order() {
        assert!(GitCaveats::none().leq(&GitCaveats::read_only()));
        assert!(GitCaveats::read_only().leq(&GitCaveats::top()));
        assert!(GitCaveats::none().leq(&GitCaveats::top()));
        // top is NOT ⊑ read_only (it grants more locally).
        assert!(!GitCaveats::top().leq(&GitCaveats::read_only()));
    }

    #[test]
    fn ref_scope_is_pattern_bounded() {
        let g = GitCaveats {
            refs: Scope::only(["refs/heads/feat/a".to_string()]),
            ..GitCaveats::top()
        };
        assert!(g.permits_ref("refs/heads/feat/a"));
        assert!(!g.permits_ref("refs/heads/main"));
    }

    #[test]
    fn from_session_writable_grants_local_no_network() {
        let g = GitCaveats::from_session(&Caveats::top());
        assert!(g.permits_read() && g.permits_stage() && g.permits_commit());
        assert!(g.permits_ref("refs/heads/x"));
        // Network is always denied regardless of the session grant.
        assert!(!g.permits_push("origin") && !g.permits_clone());
    }

    #[test]
    fn from_session_readonly_when_no_fs_write() {
        let ro_session = Caveats {
            fs_write: Scope::none(),
            ..Caveats::top()
        };
        let g = GitCaveats::from_session(&ro_session);
        assert!(g.permits_read());
        assert!(!g.permits_stage(), "no fs_write -> no staging");
        assert!(!g.permits_commit());
        assert!(!g.permits_ref("refs/heads/x"));
    }

    #[test]
    fn remote_verb_requires_both_verb_and_remote_name() {
        // A surface that names origin AND allows push — both gates must hold.
        let full = GitCaveats {
            remote: Scope::only(["origin".to_string()]),
            push: true,
            ..GitCaveats::top()
        };
        assert!(full.permits_push("origin"));
        assert!(!full.permits_push("upstream"), "remote name not in scope");

        let verb_only = GitCaveats {
            push: true,
            ..GitCaveats::top()
        }; // remote stays none()
        assert!(
            !verb_only.permits_push("origin"),
            "verb without a remote name is inert"
        );
    }

    #[test]
    fn serde_roundtrip() {
        let g = GitCaveats::top();
        let json = serde_json::to_string(&g).unwrap();
        let back: GitCaveats = serde_json::from_str(&json).unwrap();
        assert_eq!(g, back);
    }
}
