//! Reviewed launch profiles in the existing signed, encrypted permission store.
//!
//! Profile history is part of the same atomic payload as current state. The
//! existing journal verifies content identities and causal links on every read;
//! state replay also verifies the before/after relationship. Restoring an older
//! wholly valid ciphertext remains possible, as documented by the parent store.

use super::*;
use crate::config::{PermissionPreset, ToolPermissions};
use crate::event_journal::{verify_chain, Journal, JournalLine};
use crate::{Caveats, Scope};

const MAX_PROFILE_DIRS: usize = 256;

/// A confined startup policy for one canonical workspace. Networking and
/// additional development commands come from the separately selected config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceProfile {
    pub preset: PermissionPreset,
    pub default_cwd: PathBuf,
    pub read_dirs: BTreeSet<PathBuf>,
    pub write_dirs: BTreeSet<PathBuf>,
}

impl WorkspaceProfile {
    /// Resolve existing directories now, before presenting an immutable review.
    /// Relative inputs are resolved from the selected workspace, never process cwd.
    pub fn new(
        workspace: &Path,
        default_cwd: &Path,
        preset: PermissionPreset,
        read_dirs: BTreeSet<PathBuf>,
        write_dirs: BTreeSet<PathBuf>,
    ) -> anyhow::Result<Self> {
        ensure!(
            read_dirs.len() + write_dirs.len() <= MAX_PROFILE_DIRS,
            "too many workspace profile directories (maximum {MAX_PROFILE_DIRS})"
        );
        let workspace = PathBuf::from(workspace_binding(workspace)?);
        let resolve = |path: &Path| canonical_directory(&workspace.join(path));
        let result = Self {
            preset,
            default_cwd: resolve(default_cwd)?,
            read_dirs: read_dirs
                .iter()
                .map(|path| resolve(path))
                .collect::<anyhow::Result<_>>()?,
            write_dirs: write_dirs
                .iter()
                .map(|path| resolve(path))
                .collect::<anyhow::Result<_>>()?,
        };
        result.validate(&workspace)?;
        Ok(result)
    }

    /// Reject raw deserialized paths, vanished directories, and symlink retargets
    /// before a saved profile is applied or a reviewed change is committed.
    pub fn validate(&self, workspace: &Path) -> anyhow::Result<()> {
        self.validate_structure(workspace)?;
        for path in std::iter::once(workspace)
            .chain(std::iter::once(self.default_cwd.as_path()))
            .chain(self.read_dirs.iter().map(PathBuf::as_path))
            .chain(self.write_dirs.iter().map(PathBuf::as_path))
        {
            ensure!(
                canonical_directory(path)?.as_os_str() == path.as_os_str(),
                "workspace profile directory is no longer canonical: {path:?}"
            );
        }
        Ok(())
    }

    /// Lower the existing command preset, confine filesystem roots, and retain
    /// configured networking. This never selects unrestricted/bypass execution.
    pub fn caveats(
        &self,
        workspace: &Path,
        configured: &ToolPermissions,
    ) -> anyhow::Result<Caveats> {
        self.validate(workspace)?;
        let mut permissions = configured.clone();
        permissions.preset = self.preset.clone();
        let workspace = workspace
            .to_str()
            .context("workspace must be valid UTF-8")?;
        let mut caveats = permissions.to_caveats(workspace);
        let mut reads = BTreeSet::from([workspace.to_owned()]);
        reads.extend(
            self.read_dirs
                .iter()
                .chain(&self.write_dirs)
                .map(|path| path.to_str().expect("validate checked UTF-8").to_owned()),
        );
        caveats.fs_read = Scope::Only(minimal_directory_roots(reads));
        if self.preset != PermissionPreset::ReadOnly {
            let mut writes = BTreeSet::from([workspace.to_owned()]);
            writes.extend(
                self.write_dirs
                    .iter()
                    .map(|path| path.to_str().expect("validate checked UTF-8").to_owned()),
            );
            caveats.fs_write = Scope::Only(minimal_directory_roots(writes));
        }
        Ok(caveats)
    }

    fn validate_structure(&self, workspace: &Path) -> anyhow::Result<()> {
        ensure!(
            matches!(
                self.preset,
                PermissionPreset::ReadOnly
                    | PermissionPreset::WorkspaceEdit
                    | PermissionPreset::WorkspaceDev
                    | PermissionPreset::WorkspaceFullAccess
            ),
            "workspace profiles require a confined permission preset"
        );
        ensure!(
            self.read_dirs.len() + self.write_dirs.len() <= MAX_PROFILE_DIRS,
            "too many workspace profile directories (maximum {MAX_PROFILE_DIRS})"
        );
        ensure!(
            self.preset != PermissionPreset::ReadOnly || self.write_dirs.is_empty(),
            "read-only workspace profiles cannot include write directories"
        );
        for path in std::iter::once(workspace)
            .chain(std::iter::once(self.default_cwd.as_path()))
            .chain(self.read_dirs.iter().map(PathBuf::as_path))
            .chain(self.write_dirs.iter().map(PathBuf::as_path))
        {
            validate_stored_directory(path)?;
        }
        ensure!(
            self.default_cwd.starts_with(workspace),
            "default working directory must be inside the workspace"
        );
        Ok(())
    }
}

/// Validated roots are canonical directories. An ancestor already grants the
/// same recursive access, so retain only minimal roots in each emitted scope.
/// The saved profile remains unchanged for review and later editing.
fn minimal_directory_roots(roots: BTreeSet<String>) -> BTreeSet<String> {
    roots
        .iter()
        .filter(|path| {
            !roots.iter().any(|ancestor| {
                ancestor != *path && Path::new(path).starts_with(Path::new(ancestor))
            })
        })
        .cloned()
        .collect()
}

impl ContentAddressable for WorkspaceProfile {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

fn canonical_directory(path: &Path) -> anyhow::Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve profile directory {path:?}"))?;
    ensure!(
        canonical.is_dir(),
        "workspace profile path must be a directory: {path:?}"
    );
    validate_stored_directory(&canonical)?;
    Ok(canonical)
}

fn validate_stored_directory(path: &Path) -> anyhow::Result<()> {
    validate_target(
        path.to_str()
            .context("workspace profile paths must be valid UTF-8")?,
    )?;
    ensure!(
        path.is_absolute(),
        "workspace profile directories must be absolute"
    );
    ensure!(
        !path.components().any(|part| matches!(
            part,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )),
        "workspace profile directories must be normalized"
    );
    Ok(())
}

/// The reversible profile portion of a signed store; legacy exact approvals
/// remain independently managed by the existing additive permission workflow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileState {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, WorkspaceProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_workspace: Option<String>,
}

impl ProfileState {
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.profiles.len() <= MAX_WORKSPACES,
            "too many workspace profiles"
        );
        for (workspace, profile) in &self.profiles {
            profile.validate_structure(Path::new(workspace))?;
        }
        if let Some(default) = &self.default_workspace {
            ensure!(
                self.profiles.contains_key(default),
                "default workspace must have a saved profile"
            );
        }
        Ok(())
    }

    fn validate_changes_from(&self, before: &Self) -> anyhow::Result<()> {
        self.validate()?;
        for (workspace, profile) in &self.profiles {
            if before.profiles.get(workspace) != Some(profile)
                || (self.default_workspace != before.default_workspace
                    && self.default_workspace.as_deref() == Some(workspace))
            {
                profile.validate(Path::new(workspace))?;
            }
        }
        Ok(())
    }
}

impl ContentAddressable for ProfileState {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

/// Before/after state is encrypted with the policy and bound into a Merkle node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileChange {
    pub before: ProfileState,
    pub after: ProfileState,
}

impl ContentAddressable for ProfileChange {
    fn canonical_form(&self) -> Result<Vec<u8>, ContentError> {
        canonical::to_canonical_dagcbor(self)
    }
}

impl Payload {
    pub(super) fn profile_state(&self) -> ProfileState {
        ProfileState {
            profiles: self.profiles.clone(),
            default_workspace: self.default_workspace.clone(),
        }
    }

    pub(super) fn validate_profiles(&self) -> anyhow::Result<()> {
        self.profile_state().validate()?;
        ensure!(
            self.profile_history.len() <= MAX_GRANTS,
            "too many workspace profile changes"
        );
        ensure!(
            verify_chain(&self.profile_history, None).is_empty(),
            "workspace profile history does not verify"
        );
        let mut previous = ProfileState::default();
        for line in &self.profile_history {
            let change = line.node.payload();
            change.before.validate()?;
            change.after.validate()?;
            ensure!(
                change.before == previous,
                "workspace profile history does not match its preceding state"
            );
            previous = change.after.clone();
        }
        ensure!(
            previous == self.profile_state(),
            "workspace profile state does not match its verified history"
        );
        Ok(())
    }
}

/// A complete signature-verified store snapshot. No keys are created by load.
#[derive(Debug, Clone)]
pub struct VerifiedSnapshot {
    pub(super) payload: Payload,
    id: ContentId,
}

impl VerifiedSnapshot {
    /// The content-addressed empty state, without filesystem or key I/O.
    pub fn empty() -> anyhow::Result<Self> {
        Self::from_payload(Payload::empty())
    }

    pub(super) fn from_payload(payload: Payload) -> anyhow::Result<Self> {
        payload.validate()?;
        Ok(Self {
            id: payload.content_id()?,
            payload,
        })
    }

    pub fn content_id(&self) -> &ContentId {
        &self.id
    }
    pub fn profiles(&self) -> &BTreeMap<String, WorkspaceProfile> {
        &self.payload.profiles
    }
    pub fn default_workspace(&self) -> Option<&Path> {
        self.payload.default_workspace.as_deref().map(Path::new)
    }
    pub fn default_workspace_key(&self) -> Option<&str> {
        self.payload.default_workspace.as_deref()
    }
    pub fn grants(&self) -> &BTreeMap<String, GrantSet> {
        &self.payload.workspaces
    }
    pub fn grants_for(&self, workspace: &Path) -> anyhow::Result<GrantSet> {
        Ok(self
            .payload
            .workspaces
            .get(&workspace_binding(workspace)?)
            .cloned()
            .unwrap_or_default())
    }
    pub fn profile_history(&self) -> &[JournalLine<ProfileChange>] {
        &self.payload.profile_history
    }

    pub fn edit(&self) -> WorkspaceEdit {
        WorkspaceEdit {
            base: self.clone(),
            desired: self.payload.profile_state(),
        }
    }
}

/// Load the complete verified policy without requiring any workspace to exist.
/// Live directory validation is mandatory when applying or saving a profile;
/// keeping load structural allows removal of a vanished saved workspace.
pub fn load_snapshot(
    path: &Path,
    trusted_root: &UserPublic,
    encryption: &TokenIdentity,
) -> anyhow::Result<VerifiedSnapshot> {
    VerifiedSnapshot::from_payload(read_payload(path, trusted_root, encryption)?)
}

/// A mutable operator draft. It grants no authority until reviewed and committed.
#[derive(Debug, Clone)]
pub struct WorkspaceEdit {
    base: VerifiedSnapshot,
    desired: ProfileState,
}

impl WorkspaceEdit {
    pub fn set_profile(
        &mut self,
        workspace: &Path,
        profile: WorkspaceProfile,
    ) -> anyhow::Result<()> {
        let workspace = workspace_binding(workspace)?;
        profile.validate(Path::new(&workspace))?;
        ensure!(
            self.desired.profiles.contains_key(&workspace)
                || self.desired.profiles.len() < MAX_WORKSPACES,
            "too many workspace profiles"
        );
        self.desired.profiles.insert(workspace, profile);
        Ok(())
    }

    /// Remove only the profile and its default selection; independent legacy
    /// approvals and the active session's authority remain unchanged.
    pub fn remove_profile(&mut self, workspace: &str) -> anyhow::Result<()> {
        ensure!(
            self.desired.profiles.remove(workspace).is_some(),
            "saved workspace profile was not found"
        );
        if self.desired.default_workspace.as_deref() == Some(workspace) {
            self.desired.default_workspace = None;
        }
        Ok(())
    }

    pub fn set_default_workspace(&mut self, workspace: Option<&Path>) -> anyhow::Result<()> {
        let workspace = workspace.map(workspace_binding).transpose()?;
        if let Some(workspace) = &workspace {
            ensure!(
                self.desired.profiles.contains_key(workspace),
                "default workspace must have a saved profile"
            );
        }
        self.desired.default_workspace = workspace;
        Ok(())
    }

    /// Apply the inverse of a verified change without overwriting interleaved
    /// edits. A conflicting later edit requires a fresh explicit operator choice.
    pub fn revert_profile_change(&mut self, id: &ContentId) -> anyhow::Result<()> {
        let change = self
            .base
            .payload
            .profile_history
            .iter()
            .find(|line| line.id == id.to_string())
            .context("workspace profile change was not found")?
            .node
            .payload();
        let mut candidate = self.desired.clone();
        let keys: BTreeSet<_> = change
            .before
            .profiles
            .keys()
            .chain(change.after.profiles.keys())
            .collect();
        for key in keys {
            let before = change.before.profiles.get(key);
            let after = change.after.profiles.get(key);
            if before == after {
                continue;
            }
            ensure!(
                candidate.profiles.get(key) == after,
                "workspace profile changed after the change being reverted"
            );
            if let Some(before) = before {
                candidate.profiles.insert(key.clone(), before.clone());
            } else {
                candidate.profiles.remove(key);
            }
        }
        if change.before.default_workspace != change.after.default_workspace {
            ensure!(
                candidate.default_workspace == change.after.default_workspace,
                "default workspace changed after the change being reverted"
            );
            candidate
                .default_workspace
                .clone_from(&change.before.default_workspace);
        }
        candidate.validate()?;
        self.desired = candidate;
        Ok(())
    }

    pub fn profiles(&self) -> &BTreeMap<String, WorkspaceProfile> {
        &self.desired.profiles
    }
    pub fn default_workspace(&self) -> Option<&Path> {
        self.desired.default_workspace.as_deref().map(Path::new)
    }

    /// Seal the complete candidate before confirmation. Saving does not add a
    /// timestamp, merge another draft, or change the reviewed content identity.
    pub fn review(self) -> anyhow::Result<ReviewedWorkspaceEdit> {
        let mut payload = self.base.payload;
        let before = payload.profile_state();
        // Unchanged missing profiles must not prevent repairing another entry.
        // Selected startup profiles still receive strict live validation.
        self.desired.validate_changes_from(&before)?;
        if before != self.desired {
            let mut journal = match payload.profile_history.last() {
                Some(last) => Journal::resuming_from(last.node.id()?),
                None => Journal::new(),
            };
            payload.profile_history.push(journal.append(ProfileChange {
                before,
                after: self.desired.clone(),
            })?);
            payload.profiles = self.desired.profiles;
            payload.default_workspace = self.desired.default_workspace;
        }
        payload.validate()?;
        ensure!(
            payload.canonical_form()?.len() <= MAX_STORE_BYTES,
            "permission store exceeds byte limit"
        );
        Ok(ReviewedWorkspaceEdit {
            expected: self.base.id,
            id: payload.content_id()?,
            payload,
        })
    }
}

/// An immutable exact reviewed candidate, including its expected prior identity.
#[derive(Debug, Clone)]
pub struct ReviewedWorkspaceEdit {
    expected: ContentId,
    id: ContentId,
    pub(super) payload: Payload,
}

impl ReviewedWorkspaceEdit {
    pub fn expected_content_id(&self) -> &ContentId {
        &self.expected
    }
    pub fn content_id(&self) -> &ContentId {
        &self.id
    }
    pub fn profiles(&self) -> &BTreeMap<String, WorkspaceProfile> {
        &self.payload.profiles
    }
    pub fn default_workspace(&self) -> Option<&Path> {
        self.payload.default_workspace.as_deref().map(Path::new)
    }
    pub fn grants(&self) -> &BTreeMap<String, GrantSet> {
        &self.payload.workspaces
    }
}

/// Compare-and-set the whole verified store under the existing atomic lock.
/// Stale previews fail without merging or overwriting concurrent approvals.
/// Caller must additionally validate protected resources before this operation.
pub fn commit(
    path: &Path,
    reviewed: &ReviewedWorkspaceEdit,
    root: &UserKey,
    encryption: &TokenIdentity,
) -> anyhow::Result<VerifiedSnapshot> {
    let destination = crate::atomic_fs::ResolvedPath::resolve(path)?;
    let _lock = crate::atomic_fs::acquire_lock(&destination.lock_path())?;
    let current = read_payload(destination.as_path(), &root.public(), encryption)?;
    ensure!(
        current.content_id()? == reviewed.expected,
        "workspace settings changed since review; reload before saving"
    );
    reviewed
        .payload
        .profile_state()
        .validate_changes_from(&current.profile_state())?;
    reviewed.payload.ensure_content_id(&reviewed.id)?;
    if reviewed.expected == reviewed.id {
        return VerifiedSnapshot::from_payload(current);
    }
    let ciphertext = encode(&reviewed.payload, root, encryption)?;
    destination.atomic_write_private(ciphertext.as_bytes())?;
    let saved = read_payload(destination.as_path(), &root.public(), encryption)?;
    saved.ensure_content_id(&reviewed.id)?;
    VerifiedSnapshot::from_payload(saved)
}
