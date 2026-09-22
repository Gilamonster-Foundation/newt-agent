//! #2524 — a pure, unwired view/adaptor over `(DenialKind, target)` grants.
//!
//! Builds a hierarchical [`Node`] tree for display (fs by path component, net
//! by host then path, MCP by `server__tool`) and a [`to_caveat_delta`]
//! adaptor that reuses [`widen_caveats`] rather than re-deriving axis
//! widening. No I/O, no UI, not wired into `recalled_caveats` — see
//! `RESULT-tree.md`.

use crate::agentic::{widen_caveats, DenialKind};
use crate::caveats::Caveats;

/// Session-only or promoted-to-config authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantScope {
    Session,
    Durable,
}

/// Where the grant was decided — the web surface can never itself promote to
/// [`GrantScope::Durable`] (the web-surface law: only a terminal audit
/// promotes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Terminal,
    Web,
}

/// D-E: the two marks are not interchangeable — `Secret` means the harness's
/// own read tools refuse (fs only); `Redacted` means the locator/args are
/// redacted from transcripts while the result still reaches the model (net /
/// MCP only). [`Grant::with_mark`] rejects the cross combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Secret,
    Redacted,
}

/// Read vs write, carried alongside `kind` for display; `kind` alone (via
/// `DenialKind::FsRead`/`FsWrite`) already decides the enforcement axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Read,
    Write,
}

/// Rejected mark/kind combination (D-E: `Secret` is fs-only, `Redacted` is
/// net/MCP-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMark;

/// One authority grant: the DATA half of the tree (kind + hierarchical
/// target); enforcement composition is per-kind and lives in
/// [`to_caveat_delta`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub kind: DenialKind,
    pub target: String,
    pub mode: Mode,
    pub mark: Option<Mark>,
    pub scope: GrantScope,
    pub provenance: Provenance,
}

impl Grant {
    pub fn new(
        kind: DenialKind,
        target: impl Into<String>,
        mode: Mode,
        scope: GrantScope,
        provenance: Provenance,
    ) -> Self {
        Self {
            kind,
            target: target.into(),
            mode,
            mark: None,
            scope,
            provenance,
        }
    }

    /// Attach a mark, honoring the D-E split. `Secret` only on `FsRead`/
    /// `FsWrite`; `Redacted` only on `Net`/`RemoteTool`. Any other pairing is
    /// unrepresentable — rejected rather than silently stored.
    pub fn with_mark(mut self, mark: Mark) -> Result<Self, InvalidMark> {
        let is_fs = matches!(self.kind, DenialKind::FsRead | DenialKind::FsWrite);
        let is_net_or_mcp = matches!(self.kind, DenialKind::Net | DenialKind::RemoteTool);
        match (mark, is_fs, is_net_or_mcp) {
            (Mark::Secret, true, _) | (Mark::Redacted, _, true) => {
                self.mark = Some(mark);
                Ok(self)
            }
            _ => Err(InvalidMark),
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

/// Split a grant's `target` into hierarchy components per D-A/D-B/D-C's
/// natural per-kind shapes: fs = path components, net = host then path
/// segments (scheme stripped; D-F: only the host is a grantable node, the
/// rest is display), MCP = `server` then `tool` (`server__tool`). Any other
/// kind is a single flat component.
fn components(kind: DenialKind, target: &str) -> Vec<String> {
    match kind {
        DenialKind::FsRead | DenialKind::FsWrite => target
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        DenialKind::Net => {
            let without_scheme = target.rsplit_once("://").map_or(target, |(_, rest)| rest);
            without_scheme
                .split('/')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        }
        DenialKind::RemoteTool => target.splitn(2, "__").map(String::from).collect(),
        DenialKind::Exec | DenialKind::GitWrite | DenialKind::Build => vec![target.to_string()],
    }
}

/// Build the display tree: each grant's target is inserted along its
/// hierarchy path, with the grant itself attached at the leaf.
pub fn tree(grants: &[Grant]) -> Vec<Node> {
    let mut roots: Vec<Node> = Vec::new();
    for grant in grants {
        let parts = components(grant.kind, &grant.target);
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
/// [`Caveats`] for the four axes [`widen_caveats`] already covers, plus the
/// `RemoteTool` targets as a separate leash allow-set — [`RemoteTool`]
/// (`DenialKind::RemoteTool`) maps to NO `Caveats` axis (see
/// `widen_caveats`'s doc comment), so it is never folded into `caveats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaveatDelta {
    pub caveats: Caveats,
    pub remote_tool_grants: Vec<String>,
}

/// Compose a grant set on top of `base`, reusing [`widen_caveats`] for the
/// fs/exec/net axes and returning `RemoteTool` grants as the leash allow-set
/// instead of widening any `Caveats` axis.
pub fn to_caveat_delta(base: &Caveats, grants: &[Grant]) -> CaveatDelta {
    let axis_grants: Vec<(DenialKind, String)> = grants
        .iter()
        .filter(|g| g.kind != DenialKind::RemoteTool)
        .map(|g| (g.kind, g.target.clone()))
        .collect();
    let remote_tool_grants = grants
        .iter()
        .filter(|g| g.kind == DenialKind::RemoteTool)
        .map(|g| g.target.clone())
        .collect();
    CaveatDelta {
        caveats: widen_caveats(base, &axis_grants),
        remote_tool_grants,
    }
}

/// D-A / the web-surface law: a grant decided on the web surface can never
/// itself become durable — only a terminal audit promotes. Refuses
/// `Durable` when `provenance == Web`.
pub fn promotion_allowed(grant: &Grant) -> bool {
    !(grant.scope == GrantScope::Durable && grant.provenance == Provenance::Web)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caveats::Scope;

    fn fs_grant(target: &str) -> Grant {
        Grant::new(
            DenialKind::FsRead,
            target,
            Mode::Read,
            GrantScope::Session,
            Provenance::Terminal,
        )
    }

    /// Red test (1): `/a/b` groups under `/a`; `srv__get_x` groups under `srv`.
    #[test]
    fn tree_groups_fs_paths_and_mcp_tools_by_prefix() {
        let grants = vec![
            fs_grant("/a"),
            fs_grant("/a/b"),
            Grant::new(
                DenialKind::RemoteTool,
                "srv__get_x",
                Mode::Read,
                GrantScope::Session,
                Provenance::Terminal,
            ),
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

    /// Red test (2): an `FsRead /a` grant makes `/a/b` readable via the
    /// composed `Caveats` and touches no other axis.
    #[test]
    fn to_caveat_delta_widens_only_fs_read() {
        let base = Caveats {
            fs_read: Scope::none(),
            ..Caveats::default()
        };
        let delta = to_caveat_delta(&base, &[fs_grant("/a")]);

        assert!(matches!(&delta.caveats.fs_read, Scope::Only(set) if set.contains("/a")));
        assert_eq!(delta.caveats.fs_write, base.fs_write);
        assert_eq!(delta.caveats.exec, base.exec);
        assert_eq!(delta.caveats.net, base.net);
    }

    /// Red test (3): `Durable` scope from `Web` provenance is refused.
    #[test]
    fn durable_from_web_provenance_is_refused() {
        let web_durable = Grant::new(
            DenialKind::Net,
            "example.com",
            Mode::Read,
            GrantScope::Durable,
            Provenance::Web,
        );
        assert!(!promotion_allowed(&web_durable));

        let terminal_durable = Grant::new(
            DenialKind::Net,
            "example.com",
            Mode::Read,
            GrantScope::Durable,
            Provenance::Terminal,
        );
        assert!(promotion_allowed(&terminal_durable));
    }

    /// Red test (4): a `RemoteTool` grant never appears in any `Caveats`
    /// axis — it surfaces only as a leash allow-set entry.
    #[test]
    fn remote_tool_grant_never_widens_caveats() {
        let base = Caveats::default();
        let grant = Grant::new(
            DenialKind::RemoteTool,
            "srv__get_x",
            Mode::Read,
            GrantScope::Session,
            Provenance::Terminal,
        );
        let delta = to_caveat_delta(&base, &[grant]);

        assert_eq!(delta.caveats, base, "no axis widened");
        assert_eq!(delta.remote_tool_grants, vec!["srv__get_x".to_string()]);
    }

    /// Red test (5): `Secret` on a net/MCP grant, or `Redacted` on fs, is
    /// rejected rather than silently stored.
    #[test]
    fn cross_kind_marks_are_rejected() {
        let net_grant = Grant::new(
            DenialKind::Net,
            "example.com",
            Mode::Read,
            GrantScope::Session,
            Provenance::Terminal,
        );
        assert!(net_grant.with_mark(Mark::Secret).is_err());

        let fs_grant = fs_grant("/a");
        assert!(fs_grant.clone().with_mark(Mark::Redacted).is_err());

        // The honest pairings still succeed.
        let net_grant = Grant::new(
            DenialKind::Net,
            "example.com",
            Mode::Read,
            GrantScope::Session,
            Provenance::Terminal,
        );
        assert!(net_grant.with_mark(Mark::Redacted).is_ok());
        assert!(fs_grant.with_mark(Mark::Secret).is_ok());
    }
}
