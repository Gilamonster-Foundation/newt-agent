//! Admission guard for immutable operator-owned workspace policy resources.
//!
//! Paths are runtime locators, never identities or persisted authority. This
//! guard complements the existing object-bound filesystem and kernel boundary;
//! it must also be applied to permission expansions and child launch caveats.

use std::path::{Path, PathBuf};

use crate::config::{normalize_path, resolve_uncreated_path, validate_stable_anchor};
use crate::{Caveats, DenialKind, Scope};

#[derive(Clone, Debug)]
struct Resource {
    named: PathBuf,
    resolved: PathBuf,
}

/// Resources selected by trusted startup, outside model-writable authority.
/// This is a guard on admitted roots, not a filesystem backend or an exclusion
/// that can be subtracted from an unrestricted grant.
#[derive(Clone, Debug)]
pub struct WorkspaceProtection {
    write_protected: Vec<Resource>,
    read_protected: Vec<Resource>,
}

impl WorkspaceProtection {
    /// Resolve policy/store/executable resources and private key resources.
    /// Read-protected resources are also protected against mutation. Resolution
    /// creates no directories or files, including for a future policy store.
    pub fn new(write_protected: &[PathBuf], read_protected: &[PathBuf]) -> anyhow::Result<Self> {
        let resolve = |path: &PathBuf| {
            let absolute = std::path::absolute(path)?;
            let (named, resolved) = resolve_root(&absolute)?;
            Ok(Resource { named, resolved })
        };
        let read_protected = read_protected
            .iter()
            .map(resolve)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut write_protected = write_protected
            .iter()
            .map(resolve)
            .collect::<anyhow::Result<Vec<_>>>()?;
        write_protected.extend(read_protected.iter().cloned());
        anyhow::ensure!(
            !write_protected.is_empty(),
            "workspace protection requires trusted resources"
        );
        Ok(Self {
            write_protected,
            read_protected,
        })
    }

    /// Validate actual filesystem authority, including the existing sandbox's
    /// implicit read/loader/executable roots and writable device sinks.
    pub fn validate_caveats(&self, caveats: &Caveats) -> anyhow::Result<()> {
        self.validate_with_sandbox_policy(caveats, &crate::confined_exec::runtime_sandbox_policy())
    }

    /// Validate against the exact runtime substrate selected by a child launcher.
    pub fn validate_with_sandbox_policy(
        &self,
        caveats: &Caveats,
        policy: &agent_bridle::SandboxPolicy,
    ) -> anyhow::Result<()> {
        for resource in &self.write_protected {
            anyhow::ensure!(
                resolve_uncreated_path(&resource.named)? == resource.resolved,
                "protected workspace resource changed since session startup"
            );
        }
        let mut reads = scope_roots(&caveats.fs_read, !self.read_protected.is_empty())?;
        let mut writes = scope_roots(&caveats.fs_write, !self.write_protected.is_empty())?;
        let devices = policy.device_sink_paths.resolve();
        writes.extend(devices.iter().map(PathBuf::from));
        for roots in [
            policy.base_read_paths.resolve(),
            policy.bin_read_paths.resolve(),
            policy.loader_paths.resolve(),
            devices,
        ] {
            reads.extend(roots.into_iter().map(PathBuf::from));
        }
        // Match smart-frame isolation's conservative executable read inventory.
        if let Scope::Only(commands) = &caveats.exec {
            let path_bearing = |name: &str| {
                let path = Path::new(name);
                path.has_root()
                    || path
                        .parent()
                        .is_some_and(|parent| !parent.as_os_str().is_empty())
            };
            reads.extend(
                commands
                    .iter()
                    .filter(|name| path_bearing(name))
                    .map(PathBuf::from),
            );
            if commands.iter().any(|name| !path_bearing(name)) {
                if let Some(path) = std::env::var_os("PATH") {
                    reads.extend(
                        std::env::split_paths(&path).filter(|root| !root.as_os_str().is_empty()),
                    );
                }
            }
        }
        let writes = writes
            .iter()
            .map(|path| resolve_root(path))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let reads = reads
            .iter()
            .map(|path| resolve_root(path))
            .collect::<anyhow::Result<Vec<_>>>()?;
        validate_disjoint(&writes, &self.write_protected)?;
        validate_disjoint(&reads, &self.read_protected)?;
        // A writable ancestor can replace a protected name or a granted alias
        // before the existing kernel backend opens it. Reuse the same bounded
        // symlink/ancestor inspection that protects private Smart frames.
        for resource in &self.write_protected {
            validate_stable_anchor(&resource.named, &writes)?;
        }
        for (named, _) in reads.iter().chain(&writes) {
            validate_stable_anchor(named, &writes)?;
        }
        Ok(())
    }

    /// Check a prospective grant without changing unrelated authority axes.
    /// Build uses the same projection as the actual executor; a gate must also
    /// validate its caller-supplied baseline and the combined granted caveats.
    pub fn validate_request(&self, kind: DenialKind, target: &str) -> anyhow::Result<()> {
        if kind == DenialKind::Build {
            return self
                .validate_caveats(&crate::confined_exec::build_tool_caveats(Path::new(target)));
        }
        let none = Caveats {
            fs_read: Scope::none(),
            fs_write: Scope::none(),
            exec: Scope::none(),
            net: Scope::none(),
            ..Caveats::top()
        };
        self.validate_caveats(&crate::widen_caveats(&none, &[(kind, target.to_owned())]))
    }
}

fn scope_roots(scope: &Scope<String>, protected: bool) -> anyhow::Result<Vec<PathBuf>> {
    match scope {
        Scope::All if protected => {
            anyhow::bail!("unrestricted filesystem authority reaches protected workspace resources")
        }
        Scope::All => Ok(Vec::new()),
        Scope::Only(roots) => Ok(roots.iter().map(PathBuf::from).collect()),
    }
}

fn resolve_root(path: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    anyhow::ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| part == std::path::Component::ParentDir),
        "protected workspace authority requires absolute paths without parent traversal"
    );
    Ok((normalize_path(path)?, resolve_uncreated_path(path)?))
}

fn validate_disjoint(roots: &[(PathBuf, PathBuf)], protected: &[Resource]) -> anyhow::Result<()> {
    let overlaps = |a: &Path, b: &Path| a.starts_with(b) || b.starts_with(a);
    for (named, resolved) in roots {
        anyhow::ensure!(
            !protected
                .iter()
                .any(|resource| overlaps(named, &resource.named)
                    || overlaps(resolved, &resource.resolved)),
            "filesystem authority overlaps protected workspace resources"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CountBound, Scope};

    fn authority(read: &[&Path], write: &[&Path]) -> Caveats {
        Caveats {
            fs_read: Scope::only(read.iter().map(|path| path.to_string_lossy().into_owned())),
            fs_write: Scope::only(write.iter().map(|path| path.to_string_lossy().into_owned())),
            exec: Scope::All,
            net: Scope::All,
            max_calls: CountBound::AtMost(7),
            valid_for_generation: Scope::All,
        }
    }

    #[test]
    fn policy_keys_and_executable_keep_distinct_read_and_write_protection() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let operator = temp.path().join("operator");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&operator).unwrap();
        let policy = operator.join("workspace-policy.age");
        let key = operator.join("identity.pem");
        let executable = operator.join("newt");
        for path in [&policy, &key, &executable] {
            std::fs::write(path, "fixture").unwrap();
        }
        let protection = WorkspaceProtection::new(
            &[policy.clone(), executable.clone()],
            std::slice::from_ref(&key),
        )
        .unwrap();
        let safe = authority(&[&workspace, &policy, &executable], &[&workspace]);
        protection.validate_caveats(&safe).unwrap();
        assert_eq!(safe.net, Scope::All, "network is independent");
        for path in [&policy, &key, &executable, &operator, temp.path()] {
            assert!(
                protection
                    .validate_caveats(&authority(&[], &[path]))
                    .is_err(),
                "write grant must not replace resource or ancestor: {path:?}"
            );
        }
        for path in [&key, &operator, temp.path()] {
            assert!(protection
                .validate_caveats(&authority(&[path], &[]))
                .is_err());
        }
        let mut all_reads = safe.clone();
        all_reads.fs_read = Scope::All;
        assert!(protection.validate_caveats(&all_reads).is_err());
        let mut all_writes = safe;
        all_writes.fs_write = Scope::All;
        assert!(protection.validate_caveats(&all_writes).is_err());
    }

    #[test]
    fn absent_policy_is_protected_without_creating_it_or_its_parent() {
        let temp = tempfile::tempdir().unwrap();
        let policy = temp.path().join("future/settings/profiles.age");
        let sibling = temp.path().join("workspace");
        std::fs::create_dir(&sibling).unwrap();
        let protection = WorkspaceProtection::new(std::slice::from_ref(&policy), &[]).unwrap();
        protection
            .validate_caveats(&authority(&[], &[&sibling]))
            .unwrap();
        assert!(protection
            .validate_request(
                DenialKind::FsWrite,
                policy.parent().unwrap().to_str().unwrap()
            )
            .is_err());
        assert!(!temp.path().join("future").exists());
    }

    #[test]
    fn build_fence_is_checked_without_becoming_an_exec_or_network_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let protection =
            WorkspaceProtection::new(&[workspace.join("operator-policy")], &[]).unwrap();
        assert!(protection
            .validate_request(DenialKind::Build, workspace.to_str().unwrap())
            .is_err());
        protection
            .validate_request(DenialKind::Exec, "cargo")
            .unwrap();
        protection
            .validate_request(DenialKind::Net, "example.test")
            .unwrap();
    }

    #[test]
    fn implicit_sandbox_read_roots_cannot_expose_private_keys() {
        let policy = crate::confined_exec::runtime_sandbox_policy();
        let Some(root) = policy.base_read_paths.resolve().into_iter().next() else {
            return;
        };
        let key = Path::new(&root).join("newt-protected-key-fixture");
        let protection = WorkspaceProtection::new(&[], &[key]).unwrap();
        assert!(protection.validate_caveats(&authority(&[], &[])).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn aliases_to_policy_and_keys_are_not_alternative_grants() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let operator = temp.path().join("operator");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&operator).unwrap();
        std::fs::write(operator.join("identity.pem"), "key fixture").unwrap();
        symlink(&operator, &alias).unwrap();
        let protection = WorkspaceProtection::new(
            &[operator.join("policy.age")],
            &[operator.join("identity.pem")],
        )
        .unwrap();
        assert!(protection
            .validate_caveats(&authority(&[&alias.join("identity.pem")], &[]))
            .is_err());
        assert!(protection
            .validate_caveats(&authority(&[], &[&alias.join("policy.age")]))
            .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn writable_alias_anchor_cannot_be_retargeted_toward_keys() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let public = temp.path().join("public");
        let operator = temp.path().join("operator");
        for path in [&workspace, &public, &operator] {
            std::fs::create_dir(path).unwrap();
        }
        let alias = workspace.join("allowed");
        symlink(&public, &alias).unwrap();
        let protection = WorkspaceProtection::new(&[], &[operator.join("identity.pem")]).unwrap();
        assert!(protection
            .validate_caveats(&authority(&[&alias], &[&workspace]))
            .is_err());
        assert_eq!(std::fs::read_link(&alias).unwrap(), public);
    }

    #[cfg(unix)]
    #[test]
    fn trusted_resource_alias_change_invalidates_the_startup_snapshot() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        for path in [&first, &second] {
            std::fs::create_dir(path).unwrap();
        }
        let alias = temp.path().join("operator");
        symlink(&first, &alias).unwrap();
        let protection = WorkspaceProtection::new(&[alias.join("policy.age")], &[]).unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(&second, &alias).unwrap();
        assert!(protection.validate_caveats(&authority(&[], &[])).is_err());
    }
}
