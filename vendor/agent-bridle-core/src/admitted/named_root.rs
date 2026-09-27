//! Explicit root admission and inherited filesystem/network scope evidence.
use std::collections::BTreeSet;
use std::path::{Component, Path};

use content_addressable::{ContentAddressable, ContentError};

use super::{AdmittedFence, AdmittedFenceId, BackendProjection};
use crate::{
    relate, unenforceable_axis, Caveats, ConfinementMechanism, EnforcementFloor, ExecBoundary,
    ResolvedScope, SandboxKind, Scope, ScopeRelation, ToolError, ToolResult,
};

/// Inspectable body of an explicitly admitted root and its actual tree fence.
/// The CID proves content integrity, not an arbitrary sender's authenticity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdmittedFenceBody {
    /// Effective authority received from the Gate, unchanged by this operation.
    pub effective: Caveats,
    /// The same caveats actually passed to the native mechanism.
    pub mechanism_caveats: Caveats,
    /// Exact absolute operand of the actual root spawn.
    pub root: String,
    /// Backend configuration, including the explicit executable proof domain.
    pub mechanism: ConfinementMechanism,
    /// Actual backend projection; descendant executable identities are Unbounded.
    pub projection: BackendProjection,
    /// Canonical trusted inventory against which closure additions were checked.
    /// Explicit delegated access is not revoked by this inventory-relative claim.
    pub protected_roots: BTreeSet<String>,
}

impl ContentAddressable for AdmittedFenceBody {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        content_addressable::canonical::to_canonical_dagcbor(self)
    }
}

impl AdmittedFenceBody {
    /// Recompute the existing typed fence identity from the complete body.
    pub fn fence_id(&self) -> ToolResult<AdmittedFenceId> {
        self.content_id().map(AdmittedFenceId).map_err(|error| {
            ToolError::denied(format!(
                "cannot content-address the named-root fence: {error}"
            ))
        })
    }

    /// Verify the explicit contract and binding to the invocation's authority.
    pub fn verify(
        &self,
        expected_root: &str,
        effective: &Caveats,
        expected_protected_roots: &BTreeSet<String>,
    ) -> ToolResult<()> {
        if self.root != expected_root
            || &self.effective != effective
            || self.mechanism_caveats != self.effective
            || self.protected_roots
                != super::canonicalize_protected_roots(expected_protected_roots)?
        {
            return Err(ToolError::denied(
                "named-root evidence does not match the invocation root and effective authority",
            ));
        }
        validate_named_root(
            effective,
            &self.root,
            &self.protected_roots,
            self.mechanism,
            &self.projection,
            EnforcementFloor::CONFINED,
        )
    }
}

fn validate_named_root(
    delegated: &Caveats,
    root: &str,
    protected_roots: &BTreeSet<String>,
    mechanism: ConfinementMechanism,
    projection: &BackendProjection,
    floor: EnforcementFloor,
) -> ToolResult<()> {
    let path = Path::new(root);
    if !path.is_absolute()
        || path.components().any(|part| part == Component::ParentDir)
        || !matches!(&delegated.exec, Scope::Only(roots) if roots.contains(root))
    {
        return Err(ToolError::denied("named-root admission requires an exact absolute root in the effective exec Only grant, without parent traversal"));
    }
    if mechanism.exec_boundary() != ExecBoundary::NamedRoot
        || mechanism.kind() != SandboxKind::Landlock
    {
        return Err(ToolError::denied(
            "named-root admission requires its explicit supported native mechanism",
        ));
    }
    super::protected_roots::check_closure_inventory(projection, protected_roots, delegated)?;
    // These are TWO obligations, not an ordinary exec subset check with a
    // forged projection. Descendant identities must be disclosed as Unbounded.
    if projection.resolved.exec != ResolvedScope::Unbounded {
        return Err(ToolError::denied("named-root descendant exec projection must be explicitly Unbounded; bounded or unknown evidence refuses"));
    }
    for (axis, resolved, granted, closure) in [
        (
            "fs_read",
            &projection.resolved.fs_read,
            &delegated.fs_read,
            &projection.runtime_closure.fs_read,
        ),
        (
            "fs_write",
            &projection.resolved.fs_write,
            &delegated.fs_write,
            &projection.runtime_closure.fs_write,
        ),
        (
            "net",
            &projection.resolved.net,
            &delegated.net,
            &projection.runtime_closure.net,
        ),
    ] {
        let relation = relate(resolved, &ResolvedScope::from_scope(granted).union(closure));
        if !matches!(relation, ScopeRelation::Equal | ScopeRelation::Subset) {
            return Err(ToolError::denied(format!(
                "named-root {axis} authority refuses: {relation:?} (widening or unknown scope cannot inherit)"
            )));
        }
    }
    if let Some(unmet) =
        unenforceable_axis(delegated, mechanism, floor.join(EnforcementFloor::CONFINED))
    {
        return Err(ToolError::denied(format!(
            "named-root admission refuses: {unmet}"
        )));
    }
    Ok(())
}

impl AdmittedFence {
    /// Admit exact root authority and the separately disclosed descendant domain.
    pub fn admit_named_root(
        delegated: &Caveats,
        root: &str,
        protected_roots: &BTreeSet<String>,
        mechanism: ConfinementMechanism,
        floor: EnforcementFloor,
        project: impl FnOnce(&Caveats) -> BackendProjection,
    ) -> ToolResult<Self> {
        let protected_roots = super::canonicalize_protected_roots(protected_roots)?;
        let projection = project(delegated);
        validate_named_root(
            delegated,
            root,
            &protected_roots,
            mechanism,
            &projection,
            floor,
        )?;
        let body = AdmittedFenceBody {
            effective: delegated.clone(),
            mechanism_caveats: delegated.clone(),
            root: root.to_owned(),
            mechanism,
            projection,
            protected_roots,
        };
        Ok(Self {
            mechanism,
            mechanism_caveats: delegated.clone(),
            fence_id: body.fence_id()?,
            admitted_body: Some(body),
        })
    }

    /// Verify the actual launch operand and freshly rederived backend projection.
    /// The caller must derive these from the native launch being applied, never
    /// from this object's cached body or a second caller-provided root string.
    pub fn verify_named_root_applied(
        &self,
        applied: &Caveats,
        actual_launch_root: &str,
        protected_roots: &BTreeSet<String>,
        mechanism: ConfinementMechanism,
        fresh_projection: BackendProjection,
    ) -> ToolResult<()> {
        let admitted = self.admitted_body.as_ref().ok_or_else(|| {
            ToolError::denied("ordinary fence cannot verify a named-root application")
        })?;
        let actual = AdmittedFenceBody {
            effective: admitted.effective.clone(),
            mechanism_caveats: applied.clone(),
            root: actual_launch_root.to_owned(),
            mechanism,
            projection: fresh_projection,
            protected_roots: super::canonicalize_protected_roots(protected_roots)?,
        };
        actual.verify(
            &admitted.root,
            &admitted.effective,
            &admitted.protected_roots,
        )?;
        if actual.fence_id()? != self.fence_id {
            return Err(ToolError::denied(
                "actual named-root application differs from the admitted fence CID",
            ));
        }
        Ok(())
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
