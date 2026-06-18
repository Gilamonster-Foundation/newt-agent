//! `SshCaveats` — the custom OCAP surface for SSH (the long-haul transport and the
//! carrier for git network ops). Sibling of [`GitCaveats`](crate::git_caveats::GitCaveats):
//! a separate capability lattice composed *alongside* the signed `Caveats`, by `meet`.
//!
//! It gates the SSH **transport** by **hosts + keys** (per the directive — an
//! `actions` axis comes later). SSH is the network, so even [`SshCaveats::top`] is
//! the *ceiling*, not "always allowed": at the transport layer it is still gated by
//! the OCAP deviation ratchet (`b1-os-isolation` — `newt-core::ocap`), exactly like
//! git's network verbs.
//!
//! ## The read/write split for git-over-SSH (why disabling SSH blocks push *and* pull)
//!
//! git push/pull/fetch ride SSH, so two lattices compose:
//!
//! - **`SshCaveats`** gates the **transport** — *which hosts/keys* you may reach.
//!   Disable it (`hosts = none`) and **all** git network is blocked: push *and* pull.
//! - **`GitCaveats`** gates the git **verb** — `fetch` vs `push` are separate gates
//!   ([`GitCaveats`](crate::git_caveats::GitCaveats)). That is the read/write split:
//!   grant `fetch` but not `push` to allow pulls while blocking pushes.
//!
//! A git network op is permitted **iff** `GitCaveats` permits the verb **and**
//! `SshCaveats` permits the host — see [`git_over_ssh_permitted`]. So:
//!
//! | want | `SshCaveats.hosts` | `GitCaveats` |
//! |---|---|---|
//! | block all SSH (push + pull) | `none` | (anything) |
//! | allow pull, block push | host allowed | `fetch=true, push=false` |
//! | full git over SSH | host allowed | `fetch=true, push=true` |
//!
//! A future `SshCaveats` `actions` axis can *also* split at the SSH-command level
//! (`git-upload-pack` = read vs `git-receive-pack` = write) for defense in depth;
//! today the verb split lives in `GitCaveats`.

use crate::caveats::{Scope, ScopeExt};
use crate::git_caveats::GitCaveats;
use serde::{Deserialize, Serialize};

/// A capability surface for SSH: which **hosts** the agent may reach and which
/// **keys** it may use/accept. Composed with — never merged into — the session
/// `Caveats`. Default is fail-closed (`none`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshCaveats {
    /// Hosts this surface may SSH to (git remotes, mesh peers, bastions).
    pub hosts: Scope<String>,
    /// Keys (by fingerprint, e.g. `SHA256:…`) this surface may use or accept.
    pub keys: Scope<String>,
}

impl SshCaveats {
    /// The ceiling: all hosts + keys. Still gated by the OCAP ratchet at the
    /// transport layer — `top()` is the most authority a grant could confer, not
    /// an unconditional allow.
    #[must_use]
    pub fn top() -> Self {
        Self {
            hosts: Scope::top(),
            keys: Scope::top(),
        }
    }

    /// `⊥` — no SSH at all (every host + key denied). The fail-closed default.
    #[must_use]
    pub fn none() -> Self {
        Self {
            hosts: Scope::none(),
            keys: Scope::none(),
        }
    }

    /// `self ⊓ other` — greatest lower bound, per axis (attenuation only).
    #[must_use]
    pub fn meet(&self, other: &Self) -> Self {
        Self {
            hosts: self.hosts.meet(&other.hosts),
            keys: self.keys.meet(&other.keys),
        }
    }

    /// `self ⊑ other` — grants no more than `other` on every axis.
    #[must_use]
    pub fn leq(&self, other: &Self) -> bool {
        self.hosts.leq(&other.hosts) && self.keys.leq(&other.keys)
    }

    /// May the agent open an SSH connection to `host`?
    #[must_use]
    pub fn permits_host(&self, host: &str) -> bool {
        self.hosts.permits(&host.to_string())
    }

    /// May the agent use / accept the key with fingerprint `fp`?
    #[must_use]
    pub fn permits_key(&self, fp: &str) -> bool {
        self.keys.permits(&fp.to_string())
    }
}

impl Default for SshCaveats {
    /// Fail-closed: no SSH until explicitly granted.
    fn default() -> Self {
        Self::none()
    }
}

/// A git network verb (each rides SSH).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitNetVerb {
    /// Read from a remote (`git fetch`/`pull`) — `git-upload-pack` on the wire.
    Fetch,
    /// Write to a remote (`git push`) — `git-receive-pack` on the wire.
    Push,
    /// Clone a remote.
    Clone,
}

/// Is a git-over-SSH network op permitted? The **read/write split**: the git
/// `verb` (via `GitCaveats`) AND the SSH `host` (via `SshCaveats`) must *both*
/// allow it. `remote` is the git remote NAME (e.g. `origin`); `host` is the
/// network host (e.g. `github.com`).
#[must_use]
pub fn git_over_ssh_permitted(
    git: &GitCaveats,
    ssh: &SshCaveats,
    verb: GitNetVerb,
    remote: &str,
    host: &str,
) -> bool {
    let git_ok = match verb {
        GitNetVerb::Fetch => git.permits_fetch(remote),
        GitNetVerb::Push => git.permits_push(remote),
        GitNetVerb::Clone => git.permits_clone(),
    };
    git_ok && ssh.permits_host(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_git() -> GitCaveats {
        // A git surface that names origin and allows both verbs.
        GitCaveats {
            remote: Scope::only(["origin".to_string()]),
            fetch: true,
            push: true,
            ..GitCaveats::top()
        }
    }

    #[test]
    fn default_and_none_are_fail_closed() {
        assert_eq!(SshCaveats::default(), SshCaveats::none());
        let n = SshCaveats::none();
        assert!(!n.permits_host("github.com"));
        assert!(!n.permits_key("SHA256:abc"));
    }

    #[test]
    fn top_permits_hosts_and_keys() {
        let t = SshCaveats::top();
        assert!(t.permits_host("github.com"));
        assert!(t.permits_key("SHA256:abc"));
    }

    #[test]
    fn host_scope_is_bounded() {
        let s = SshCaveats {
            hosts: Scope::only(["github.com".to_string()]),
            keys: Scope::top(),
        };
        assert!(s.permits_host("github.com"));
        assert!(!s.permits_host("evil.example.com"));
    }

    #[test]
    fn meet_attenuates_and_leq_orders() {
        let bounded = SshCaveats {
            hosts: Scope::only(["github.com".to_string()]),
            keys: Scope::none(),
        };
        let m = SshCaveats::top().meet(&bounded);
        assert!(m.permits_host("github.com") && !m.permits_host("x"));
        assert!(!m.permits_key("SHA256:abc"));
        assert!(m.leq(&SshCaveats::top()));
        assert!(SshCaveats::none().leq(&bounded));
    }

    // --- the read/write split (the operator's requirement) ---

    #[test]
    fn disabling_ssh_blocks_push_and_pull() {
        let git = full_git();
        let ssh = SshCaveats::none(); // human disabled SSH entirely
        assert!(!git_over_ssh_permitted(
            &git,
            &ssh,
            GitNetVerb::Fetch,
            "origin",
            "github.com"
        ));
        assert!(!git_over_ssh_permitted(
            &git,
            &ssh,
            GitNetVerb::Push,
            "origin",
            "github.com"
        ));
    }

    #[test]
    fn allow_pull_block_push() {
        // host reachable over SSH, but the git surface grants fetch and NOT push.
        let ssh = SshCaveats {
            hosts: Scope::only(["github.com".to_string()]),
            keys: Scope::top(),
        };
        let read_git = GitCaveats {
            remote: Scope::only(["origin".to_string()]),
            fetch: true,
            push: false,
            ..GitCaveats::top()
        };
        assert!(
            git_over_ssh_permitted(&read_git, &ssh, GitNetVerb::Fetch, "origin", "github.com"),
            "fetch/pull is allowed"
        );
        assert!(
            !git_over_ssh_permitted(&read_git, &ssh, GitNetVerb::Push, "origin", "github.com"),
            "push is blocked"
        );
    }

    #[test]
    fn full_grant_allows_both() {
        let ssh = SshCaveats {
            hosts: Scope::only(["github.com".to_string()]),
            keys: Scope::top(),
        };
        assert!(git_over_ssh_permitted(
            &full_git(),
            &ssh,
            GitNetVerb::Fetch,
            "origin",
            "github.com"
        ));
        assert!(git_over_ssh_permitted(
            &full_git(),
            &ssh,
            GitNetVerb::Push,
            "origin",
            "github.com"
        ));
    }

    #[test]
    fn host_must_be_in_scope_even_with_git_grant() {
        // git allows push, but the SSH host isn't permitted -> blocked (transport gate).
        let ssh = SshCaveats {
            hosts: Scope::only(["github.com".to_string()]),
            keys: Scope::top(),
        };
        assert!(!git_over_ssh_permitted(
            &full_git(),
            &ssh,
            GitNetVerb::Push,
            "origin",
            "gitlab.com"
        ));
    }
}
