//! #2524 — a pure, unwired view/adaptor over hierarchical grants.
//!
//! Builds a hierarchical [`Node`] tree for display (fs by path component, net
//! by host then path, MCP by server then tool) and a [`to_caveat_delta`]
//! adaptor that reuses [`widen_caveats`] rather than re-deriving axis
//! widening. No I/O, no UI, not wired into `recalled_caveats`.
//!
//! Round 2 (#2534): the properties below used to be enforced by a builder
//! every caller could skip (all-`pub` fields). They are now unconstructible:
//! [`Target`] is a per-kind enum, so a mark or a mode that doesn't belong to
//! a kind is a type error, not a runtime check; [`GrantScope::Durable`]
//! carries no [`Provenance`], so "durable + web" cannot be built at all.
//!
//! "Attenuate-only" in the lattice sense is the CALLER's job — this module
//! returns a delta that widens the named axis and touches nothing else;
//! meeting that delta against a ceiling (`recalled_caveats`) is where
//! attenuation is enforced.

use crate::agentic::{widen_caveats, DenialKind};
use crate::caveats::Caveats;

/// Where the grant was decided — the web surface can never itself promote to
/// durable (the web-surface law: only a terminal audit promotes). Carried
/// only by [`GrantScope::Session`]: [`GrantScope::Durable`] has no
/// provenance, because nothing but [`Grant::promote`] (itself gated on
/// `Session(Terminal)`) may produce one. This makes "durable + web" a type
/// error rather than a runtime check — compare the old
/// `promotion_allowed` predicate, which the doc-comment example below shows
/// no longer has anything to reject:
///
/// ```compile_fail
/// use newt_core::grant_tree::{GrantScope, Provenance};
/// // A durable-with-web-provenance grant scope cannot be named: `Durable`
/// // takes no `Provenance` argument.
/// let _ = GrantScope::Durable(Provenance::Web);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Terminal,
    Web,
}

/// Session-only or promoted-to-config authority. `Durable` carries no
/// [`Provenance`] — only [`Grant::promote`] mints one, and only from
/// `Session(Provenance::Terminal)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantScope {
    Session(Provenance),
    Durable,
}

/// [`Grant::promote`] refused: the grant is not `Session(Terminal)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotPromotable;

/// Per-kind grant target. Replaces a flat `(kind, target, mode, mark)`
/// tuple: `kind` is DERIVED via [`Target::denial_kind`] rather than a
/// second field that could disagree with the variant, and a mark or
/// path/host distinction that doesn't apply to a kind simply has no field
/// to set. D-E: `secret` is fs-only (the harness's own read tools refuse),
/// `redacted` is net/MCP-only (locator/args redacted from transcripts, the
/// result still reaches the model) — putting either flag on the wrong kind
/// is now a compile error, not a rejected runtime call:
///
/// ```compile_fail
/// use newt_core::grant_tree::Target;
/// // `Target::Exec` has no `secret` field to set.
/// let _ = Target::Exec { program: "ls".into(), secret: true };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    FsRead {
        path: String,
        secret: bool,
    },
    FsWrite {
        path: String,
        secret: bool,
    },
    /// D-F: only the host is a grantable/enforceable node (net enforcement
    /// is exact host membership); `path` is display-only and never reaches
    /// [`to_caveat_delta`] — see `net` below for the scheme/path split.
    Net {
        host: String,
        path: Vec<String>,
        redacted: bool,
    },
    /// Maps to `DenialKind::RemoteTool`, which widens no `Caveats` axis
    /// (see [`widen_caveats`]'s doc comment) — surfaced by
    /// [`to_caveat_delta`] as a leash allow-set entry instead.
    Mcp {
        server: String,
        tool: String,
        redacted: bool,
    },
    Exec {
        program: String,
    },
    /// #1056: a local git write. Maps to no `Caveats` axis; surfaced
    /// beside `remote_tool_grants` rather than silently dropped (#2534
    /// item 3 — it used to vanish: routed into `axis_grants`, where
    /// `widen_caveats` `continue`s on it, and appearing in no call-grant
    /// list either).
    GitWrite {
        target: String,
    },
    /// Same shape and same fix as `GitWrite`.
    Build {
        target: String,
    },
}

impl Target {
    #[must_use]
    pub fn fs_read(path: impl Into<String>) -> Self {
        Self::FsRead {
            path: path.into(),
            secret: false,
        }
    }

    #[must_use]
    pub fn fs_read_secret(path: impl Into<String>) -> Self {
        Self::FsRead {
            path: path.into(),
            secret: true,
        }
    }

    #[must_use]
    pub fn fs_write(path: impl Into<String>) -> Self {
        Self::FsWrite {
            path: path.into(),
            secret: false,
        }
    }

    #[must_use]
    pub fn fs_write_secret(path: impl Into<String>) -> Self {
        Self::FsWrite {
            path: path.into(),
            secret: true,
        }
    }

    /// Splits `raw` into a host (the only part enforcement or the caveat
    /// delta ever sees, D-F) and a display-only path — scheme stripped,
    /// path split on `/`.
    #[must_use]
    pub fn net(raw: &str) -> Self {
        Self::net_impl(raw, false)
    }

    #[must_use]
    pub fn net_redacted(raw: &str) -> Self {
        Self::net_impl(raw, true)
    }

    fn net_impl(raw: &str, redacted: bool) -> Self {
        let without_scheme = raw.rsplit_once("://").map_or(raw, |(_, rest)| rest);
        let mut parts = without_scheme.split('/').filter(|s| !s.is_empty());
        let host = parts.next().unwrap_or("").to_string();
        let path = parts.map(String::from).collect();
        Self::Net {
            host,
            path,
            redacted,
        }
    }

    #[must_use]
    pub fn mcp(server: impl Into<String>, tool: impl Into<String>) -> Self {
        Self::Mcp {
            server: server.into(),
            tool: tool.into(),
            redacted: false,
        }
    }

    #[must_use]
    pub fn mcp_redacted(server: impl Into<String>, tool: impl Into<String>) -> Self {
        Self::Mcp {
            server: server.into(),
            tool: tool.into(),
            redacted: true,
        }
    }

    #[must_use]
    pub fn exec(program: impl Into<String>) -> Self {
        Self::Exec {
            program: program.into(),
        }
    }

    #[must_use]
    pub fn git_write(target: impl Into<String>) -> Self {
        Self::GitWrite {
            target: target.into(),
        }
    }

    #[must_use]
    pub fn build(target: impl Into<String>) -> Self {
        Self::Build {
            target: target.into(),
        }
    }

    /// The enforcement axis this target belongs to — derived, never a
    /// second field that could disagree with the variant.
    #[must_use]
    pub fn denial_kind(&self) -> DenialKind {
        match self {
            Self::FsRead { .. } => DenialKind::FsRead,
            Self::FsWrite { .. } => DenialKind::FsWrite,
            Self::Net { .. } => DenialKind::Net,
            Self::Mcp { .. } => DenialKind::RemoteTool,
            Self::Exec { .. } => DenialKind::Exec,
            Self::GitWrite { .. } => DenialKind::GitWrite,
            Self::Build { .. } => DenialKind::Build,
        }
    }

    /// Display-tree hierarchy: fs = path components (split on `/` or `\`,
    /// so a Windows-style path doesn't collapse to one node), net = host
    /// then display path, MCP = server then tool, everything else a single
    /// flat component. A degenerate fs path (e.g. `/`) still shows: it
    /// falls back to one node labeled with the raw path rather than
    /// vanishing from the tree.
    fn components(&self) -> Vec<String> {
        match self {
            Self::FsRead { path, .. } | Self::FsWrite { path, .. } => {
                let parts: Vec<String> = path
                    .split(['/', '\\'])
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                if parts.is_empty() {
                    vec![path.clone()]
                } else {
                    parts
                }
            }
            Self::Net { host, path, .. } => std::iter::once(host.clone())
                .chain(path.iter().cloned())
                .collect(),
            Self::Mcp { server, tool, .. } => vec![server.clone(), tool.clone()],
            Self::Exec { program } => vec![program.clone()],
            Self::GitWrite { target } | Self::Build { target } => vec![target.clone()],
        }
    }
}

/// One authority grant: the DATA half of the tree (per-kind target);
/// enforcement composition is per-kind and lives in [`to_caveat_delta`].
///
/// `scope` is private: [`GrantScope::Durable`] is reachable only through
/// [`Grant::promote`] (#2534 round 3 — the transition, not just the state,
/// is now the only way in). A caller can construct only a session grant:
///
/// ```compile_fail
/// use newt_core::grant_tree::{Grant, GrantScope, Target};
/// // `Grant::new` takes a `Provenance`, not a `GrantScope` — `Durable`
/// // cannot be named here.
/// let _ = Grant::new(Target::exec("ls"), GrantScope::Durable);
/// ```
///
/// ```compile_fail
/// use newt_core::grant_tree::{Grant, GrantScope, Provenance, Target};
/// let mut g = Grant::new(Target::exec("ls"), Provenance::Terminal);
/// // `scope` is private outside the module — cannot be reassigned.
/// g.scope = GrantScope::Durable;
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub target: Target,
    scope: GrantScope,
}

impl Grant {
    /// Constructs a session grant. `Durable` is not constructible here —
    /// only [`Grant::promote`] mints one.
    #[must_use]
    pub fn new(target: Target, provenance: Provenance) -> Self {
        Self {
            target,
            scope: GrantScope::Session(provenance),
        }
    }

    #[must_use]
    pub fn scope(&self) -> GrantScope {
        self.scope
    }

    #[must_use]
    pub fn kind(&self) -> DenialKind {
        self.target.denial_kind()
    }

    /// D-A / the web-surface law: promote to [`GrantScope::Durable`]. Only
    /// a terminal audit may — succeeds only from
    /// `Session(Provenance::Terminal)`, so a web-provenance or already-
    /// durable grant refuses rather than silently promoting.
    pub fn promote(mut self) -> Result<Self, NotPromotable> {
        match self.scope {
            GrantScope::Session(Provenance::Terminal) => {
                self.scope = GrantScope::Durable;
                Ok(self)
            }
            _ => Err(NotPromotable),
        }
    }
}

/// One node in the display tree — a path/host/server component, optionally
/// carrying the grant that lands exactly there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Node {
    pub label: String,
    pub grant: Option<Grant>,
    pub children: Vec<Self>,
}

impl Node {
    fn leaf(label: String) -> Self {
        Self {
            label,
            grant: None,
            children: Vec::new(),
        }
    }

    fn child_mut(&mut self, label: &str) -> &mut Self {
        if let Some(pos) = self.children.iter().position(|c| c.label == label) {
            &mut self.children[pos]
        } else {
            self.children.push(Self::leaf(label.to_string()));
            self.children.last_mut().expect("just pushed")
        }
    }
}

/// Build the display tree: each grant's target is inserted along its
/// hierarchy path, with the grant itself attached at the leaf.
pub fn tree(grants: &[Grant]) -> Vec<Node> {
    let mut roots: Vec<Node> = Vec::new();
    for grant in grants {
        let parts = grant.target.components();
        if parts.is_empty() {
            continue;
        }
        let mut cursor = if let Some(pos) = roots.iter().position(|n| n.label == parts[0]) {
            &mut roots[pos]
        } else {
            roots.push(Node::leaf(parts[0].clone()));
            roots.last_mut().expect("just pushed")
        };
        for part in &parts[1..] {
            cursor = cursor.child_mut(part);
        }
        cursor.grant = Some(grant.clone());
    }
    roots
}

/// The result of composing a grant set into enforcement: a widened
/// [`Caveats`] for the fs/exec/net axes [`widen_caveats`] covers, plus the
/// grant kinds that map to no `Caveats` axis, surfaced as their own leash
/// allow-sets instead of silently dropped (#2534 item 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaveatDelta {
    pub caveats: Caveats,
    pub remote_tool_grants: Vec<String>,
    pub git_write_grants: Vec<String>,
    pub build_grants: Vec<String>,
}

/// Compose a grant set on top of `base`, reusing [`widen_caveats`] for the
/// fs/exec/net axes. A net target's display `path` never reaches the axis
/// grant — only `host` does (D-F): enforcement is exact host membership, so
/// a path-bearing net target would otherwise render as granted and never
/// match.
pub fn to_caveat_delta(base: &Caveats, grants: &[Grant]) -> CaveatDelta {
    let mut axis_grants: Vec<(DenialKind, String)> = Vec::new();
    let mut remote_tool_grants = Vec::new();
    let mut git_write_grants = Vec::new();
    let mut build_grants = Vec::new();

    for grant in grants {
        match &grant.target {
            Target::FsRead { path, .. } => axis_grants.push((DenialKind::FsRead, path.clone())),
            Target::FsWrite { path, .. } => axis_grants.push((DenialKind::FsWrite, path.clone())),
            Target::Net { host, .. } => axis_grants.push((DenialKind::Net, host.clone())),
            Target::Exec { program } => axis_grants.push((DenialKind::Exec, program.clone())),
            Target::Mcp { server, tool, .. } => {
                remote_tool_grants.push(format!("{server}__{tool}"));
            }
            Target::GitWrite { target } => git_write_grants.push(target.clone()),
            Target::Build { target } => build_grants.push(target.clone()),
        }
    }

    CaveatDelta {
        caveats: widen_caveats(base, &axis_grants),
        remote_tool_grants,
        git_write_grants,
        build_grants,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caveats::Scope;

    fn session_terminal(target: Target) -> Grant {
        Grant::new(target, Provenance::Terminal)
    }

    /// Red test (1): `/a/b` groups under `/a`; `srv`/`get_x` groups under `srv`.
    #[test]
    fn tree_groups_fs_paths_and_mcp_tools_by_prefix() {
        let grants = vec![
            session_terminal(Target::fs_read("/a")),
            session_terminal(Target::fs_read("/a/b")),
            session_terminal(Target::mcp("srv", "get_x")),
        ];
        let roots = tree(&grants);

        let a = roots.iter().find(|n| n.label == "a").expect("a root");
        assert!(a.grant.is_some(), "/a itself carries a grant");
        let b = a
            .children
            .iter()
            .find(|n| n.label == "b")
            .expect("b child of a");
        assert!(b.grant.is_some(), "/a/b carries a grant");

        let srv = roots.iter().find(|n| n.label == "srv").expect("srv root");
        let get_x = srv
            .children
            .iter()
            .find(|n| n.label == "get_x")
            .expect("get_x child of srv");
        assert!(get_x.grant.is_some());
    }

    /// Red test (2): a Windows-style path splits into components on `\`
    /// instead of collapsing to one node.
    #[test]
    fn tree_splits_windows_paths_on_backslash() {
        let grants = vec![session_terminal(Target::fs_read(r"C:\a\b"))];
        let roots = tree(&grants);

        let c = roots.iter().find(|n| n.label == "C:").expect("C: root");
        let a = c.children.iter().find(|n| n.label == "a").expect("a child");
        assert!(a.children.iter().any(|n| n.label == "b"));
    }

    /// Red test (3): an `FsRead /a` grant makes `/a/b` readable via the
    /// composed `Caveats` and touches no other axis.
    #[test]
    fn to_caveat_delta_widens_only_fs_read() {
        let base = Caveats {
            fs_read: Scope::none(),
            ..Caveats::default()
        };
        let delta = to_caveat_delta(&base, &[session_terminal(Target::fs_read("/a"))]);

        assert!(matches!(&delta.caveats.fs_read, Scope::Only(set) if set.contains("/a")));
        assert_eq!(delta.caveats.fs_write, base.fs_write);
        assert_eq!(delta.caveats.exec, base.exec);
        assert_eq!(delta.caveats.net, base.net);
    }

    /// Red test (4): a net grant's delta contains only the host — a
    /// path/query on the target never reaches the axis (D-F).
    #[test]
    fn net_grant_delta_contains_only_host() {
        let base = Caveats {
            net: Scope::none(),
            ..Caveats::default()
        };
        let delta = to_caveat_delta(
            &base,
            &[session_terminal(Target::net("https://example.com/a/b?x=1"))],
        );

        match &delta.caveats.net {
            Scope::Only(set) => {
                assert_eq!(set.len(), 1, "only the host reaches the axis");
                assert!(set.contains("example.com"));
            }
            Scope::All => panic!("expected a narrowed net scope"),
        }
    }

    /// Red test (5): a `RemoteTool` grant never appears in any `Caveats`
    /// axis — it surfaces only as a leash allow-set entry.
    #[test]
    fn remote_tool_grant_never_widens_caveats() {
        let base = Caveats::default();
        let delta = to_caveat_delta(&base, &[session_terminal(Target::mcp("srv", "get_x"))]);

        assert_eq!(delta.caveats, base, "no axis widened");
        assert_eq!(delta.remote_tool_grants, vec!["srv__get_x".to_string()]);
    }

    /// Red test (6): `GitWrite`/`Build` grants widen no axis but surface
    /// beside `remote_tool_grants` instead of vanishing (#2534 item 3).
    #[test]
    fn git_write_and_build_grants_surface_outside_caveats() {
        let base = Caveats::default();
        let delta = to_caveat_delta(
            &base,
            &[
                session_terminal(Target::git_write("commit")),
                session_terminal(Target::build("cargo build")),
            ],
        );

        assert_eq!(delta.caveats, base, "no axis widened");
        assert_eq!(delta.git_write_grants, vec!["commit".to_string()]);
        assert_eq!(delta.build_grants, vec!["cargo build".to_string()]);
    }

    /// Red test (7): `promote` succeeds only from `Session(Terminal)` — a
    /// web-provenance grant refuses (the web-surface law), and the
    /// forbidden "durable from web" state is not just refused at runtime
    /// but unconstructible (see the doc-comment `compile_fail` examples on
    /// [`Provenance`] and [`Target`]).
    #[test]
    fn promote_succeeds_only_from_session_terminal() {
        let web = Grant::new(Target::net("example.com"), Provenance::Web);
        assert_eq!(web.promote(), Err(NotPromotable));

        let terminal = Grant::new(Target::net("example.com"), Provenance::Terminal);
        let promoted = terminal.promote().expect("terminal session promotes");
        assert_eq!(promoted.scope(), GrantScope::Durable);

        let already_durable = Grant::new(Target::net("example.com"), Provenance::Terminal)
            .promote()
            .expect("terminal session promotes");
        assert_eq!(
            already_durable.promote(),
            Err(NotPromotable),
            "already-durable does not re-promote"
        );
    }
}
