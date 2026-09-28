//! The recorded trusted inventory bounds closure additions, not delegated access.
use std::collections::BTreeSet;
use std::path::{Component, Path};

use crate::{
    BackendProjection, Caveats, ResolvedScope, SandboxPolicy, Scope, ToolError, ToolResult,
};

fn canonical_path(path: &str) -> ToolResult<String> {
    let path = Path::new(path);
    let invalid = || {
        ToolError::denied("protected inventory/closure path is relative, unresolved or ambiguous")
    };
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(invalid());
    }
    // Reuse the existing missing-tail canonicalizer, but do not confuse a
    // dangling symlink with a missing ordinary directory. Inspect only the
    // finite ancestor chain, never walk the filesystem. Native races remain.
    for prefix in path.ancestors() {
        match std::fs::symlink_metadata(prefix) {
            Ok(meta) if meta.file_type().is_symlink() && prefix.canonicalize().is_err() => {
                return Err(invalid())
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(invalid()),
            _ => {}
        }
    }
    crate::context::canonicalize_for_check(path)
        .map_err(|_| invalid())?
        .to_str()
        .map(str::to_owned)
        .ok_or_else(invalid)
}

pub(crate) fn canonicalize_protected_roots(
    roots: &BTreeSet<String>,
) -> ToolResult<BTreeSet<String>> {
    if roots.is_empty() {
        return Err(ToolError::denied(
            "NamedRoot requires a nonempty trusted protected-root inventory",
        ));
    }
    roots.iter().map(|root| canonical_path(root)).collect()
}

impl SandboxPolicy {
    /// Resolve the trusted NamedRoot inventory, including ordinary missing tails.
    /// Missing inventory and ambiguous/dangling symlinks fail closed. This does
    /// not discover state stores and does not close path-to-inode races.
    pub fn resolve_named_root_protected_roots(&self) -> ToolResult<BTreeSet<String>> {
        let roots = self.named_root_protected_roots.as_ref().ok_or_else(|| {
            ToolError::denied("NamedRoot requires an explicit trusted protected-root inventory")
        })?;
        canonicalize_protected_roots(roots)
    }
}

fn explicitly_covers(scope: &Scope<String>, path: &Path) -> ToolResult<bool> {
    let Scope::Only(granted) = scope else {
        return Ok(true);
    };
    for entry in granted {
        let canonical = canonical_path(entry)?;
        if crate::context::path_is_within(path, Path::new(&canonical)) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn check_closure_inventory(
    projection: &BackendProjection,
    roots: &BTreeSet<String>,
    delegated: &Caveats,
) -> ToolResult<()> {
    for scope in [
        &projection.runtime_closure.fs_read,
        &projection.runtime_closure.fs_write,
        &projection.runtime_closure.exec,
        &projection.runtime_closure.net,
    ] {
        if !matches!(scope, ResolvedScope::Bounded { classes, .. } if classes.is_empty()) {
            return Err(ToolError::denied("runtime closure requires concrete finite paths/hosts; classes cannot prove inventory disjointness"));
        }
    }
    // The supported Landlock operation adds no executable or network authority.
    // Do not admit speculative proxy/launcher cells just because they are finite.
    if projection.runtime_closure.exec != ResolvedScope::empty()
        || projection.runtime_closure.net != ResolvedScope::empty()
    {
        return Err(ToolError::denied(
            "NamedRoot requires empty exec and net runtime closures",
        ));
    }
    for (axis, scope, granted) in [
        (
            "fs_read",
            &projection.runtime_closure.fs_read,
            &delegated.fs_read,
        ),
        (
            "fs_write",
            &projection.runtime_closure.fs_write,
            &delegated.fs_write,
        ),
    ] {
        let ResolvedScope::Bounded { concrete, .. } = scope else {
            unreachable!()
        };
        for entry in concrete {
            let canonical = canonical_path(entry)?;
            let path = Path::new(&canonical);
            // Only the trusted recorded inventory defines private roots for
            // NamedRoot. Ordinary admission retains its legacy marker policy.
            for root in roots {
                let protected = Path::new(root);
                let intersection = if crate::context::path_is_within(path, protected) {
                    path
                } else if crate::context::path_is_within(protected, path) {
                    protected
                } else {
                    continue;
                };
                if !explicitly_covers(granted, intersection)? {
                    return Err(ToolError::denied(format!(
                        "{axis} runtime closure adds access to a recorded trusted protected root"
                    )));
                }
            }
        }
    }
    Ok(())
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
