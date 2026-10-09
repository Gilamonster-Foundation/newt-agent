//! Load operator workspace choices once; editing them never reloads live authority.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{ensure, Context};
use newt_core::durable_grants::{self, VerifiedSnapshot, WorkspaceProfile};
use newt_core::workspace_protection::WorkspaceProtection;
use newt_core::{Caveats, Config, ToolPermissions};

/// The same resolved directory is passed to the TUI and its crew runner.
pub struct CodeWorkspace {
    pub path: Option<PathBuf>,
    pub has_profile: bool,
}

/// Workspace authority is operator state. An ambient repository configuration
/// must not shadow the operator store and thereby switch off its launch policy.
pub(crate) fn config_path() -> Option<PathBuf> {
    Config::pinned_config_path().or_else(Config::user_config_path)
}

pub(crate) fn read_snapshot(config: &Path, key: &Path) -> anyhow::Result<VerifiedSnapshot> {
    let store = durable_grants::store_path(config);
    match std::fs::symlink_metadata(&store) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return VerifiedSnapshot::empty();
        }
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let root = newt_identity::load_user_key(key)?;
    let identity = newt_core::secrets::load_identity()?.context(
        "workspace settings need their existing encryption identity; no identity was generated",
    )?;
    durable_grants::load_snapshot(&store, &root.public(), &identity)
}

fn snapshot() -> anyhow::Result<VerifiedSnapshot> {
    match config_path() {
        Some(config) => read_snapshot(&config, &newt_identity::default_key_path()?),
        None => VerifiedSnapshot::empty(),
    }
}

/// Resolve an operator-selected default before freezing launch flags. A saved
/// profile never turns a command-line host bypass into a confined execution.
pub fn resolve_code_workspace(path: Option<&Path>) -> anyhow::Result<CodeWorkspace> {
    let snapshot = snapshot()?;
    select_workspace(path, &std::env::current_dir()?, &snapshot)
}

fn select_workspace(
    explicit: Option<&Path>,
    cwd: &Path,
    snapshot: &VerifiedSnapshot,
) -> anyhow::Result<CodeWorkspace> {
    let selected = explicit.or_else(|| snapshot.default_workspace());
    let root = selected.unwrap_or(cwd);
    // Preserve ordinary launch behavior when the operator has no profiles.
    if snapshot.profiles().is_empty() {
        return Ok(CodeWorkspace {
            path: selected.map(Path::to_path_buf),
            has_profile: false,
        });
    }
    let (canonical, profile) = bound_workspace(&cwd.join(root), snapshot)?;
    Ok(CodeWorkspace {
        path: Some(canonical),
        has_profile: profile.is_some(),
    })
}

/// Validate a saved name before following aliases, so a retarget cannot turn
/// a profile launch into an ordinary configuration fallback.
fn bound_workspace<'a>(
    selected: &Path,
    snapshot: &'a VerifiedSnapshot,
) -> anyhow::Result<(PathBuf, Option<&'a WorkspaceProfile>)> {
    let absolute = std::path::absolute(selected)?;
    let named = newt_core::ocap_store::normalize_fs_path(
        absolute.to_str().context("workspace must be valid UTF-8")?,
    )?;
    if let Some(profile) = snapshot.profiles().get(&named) {
        profile.validate(Path::new(&named))?;
    }
    let canonical = selected
        .canonicalize()
        .context("cannot resolve selected workspace")?;
    // Validate matching saved names too: native case aliases and alternate
    // symlinks must not hide a retarget. Unrelated stale entries remain usable
    // for repair without blocking this selected workspace.
    for (name, profile) in snapshot.profiles() {
        let name = Path::new(name);
        if name == canonical || name.canonicalize().is_ok_and(|path| path == canonical) {
            profile.validate(name)?;
        }
    }
    let profile = snapshot.profiles().get(
        canonical
            .to_str()
            .context("workspace must be valid UTF-8")?,
    );
    Ok((canonical, profile))
}

/// Read-only doctor projection of the same selected profile, fallback policy,
/// and protected operator resources used by interactive startup. No key minting.
pub fn workspace_protection_for_diagnostics(
    config: &Config,
    workspace: &Path,
) -> anyhow::Result<(Caveats, WorkspaceProtection)> {
    let workspace = workspace.canonicalize()?;
    let session = WorkspaceSession::load(&workspace)?;
    let permissions = config
        .tui
        .as_ref()
        .map(|tui| tui.permissions.clone())
        .unwrap_or_default();
    let policy = session
        .policy(&workspace, &permissions)?
        .unwrap_or_else(|| {
            crate::policy_for(crate::resolve_tui(config), &workspace.to_string_lossy())
        });
    let guard = session.make_protection()?;
    guard.validate_caveats(&policy)?;
    Ok((policy, guard))
}

pub(crate) struct WorkspaceSession {
    pub config_path: Option<PathBuf>,
    pub key_path: Option<PathBuf>,
    pub profile: Option<WorkspaceProfile>,
    pub protection: Option<Arc<WorkspaceProtection>>,
    pub unavailable: Option<String>,
}

impl WorkspaceSession {
    pub(crate) fn load(workspace: &Path) -> anyhow::Result<Self> {
        let config_path = config_path();
        let key_path = newt_identity::default_key_path().ok();
        let snapshot = match (&config_path, &key_path) {
            (Some(config), Some(key)) => read_snapshot(config, key)?,
            _ => VerifiedSnapshot::empty()?,
        };
        let (root, profile) = bound_workspace(workspace, &snapshot)?;
        let profile = profile.cloned();
        if let Some(profile) = &profile {
            profile.validate(&root)?;
            let launch = newt_core::launch_authority::current();
            ensure!(
                !launch.ocap_disabled() && !launch.full_access() && !launch.unsafe_host_exec(),
                "saved workspace settings require a confined launch; remove global access and host-bypass flags"
            );
        }
        Ok(Self {
            config_path,
            key_path,
            profile,
            protection: None,
            unavailable: None,
        })
    }

    pub(crate) fn policy(
        &self,
        workspace: &Path,
        permissions: &ToolPermissions,
    ) -> anyhow::Result<Option<Caveats>> {
        self.profile
            .as_ref()
            .map(|profile| {
                let mut profile = profile.clone();
                if newt_core::launch_authority::current().workspace_access() {
                    profile.preset = newt_core::PermissionPreset::WorkspaceFullAccess;
                }
                let mut caveats = profile.caveats(workspace, permissions)?;
                // Explicit launch grants remain per-invocation operator choices.
                newt_core::caveats::apply_cli_fs_grants(&mut caveats, &workspace.to_string_lossy());
                if let newt_core::Scope::Only(commands) = &mut caveats.exec {
                    commands.extend(crate::scan_cli_exec_grants());
                }
                Ok(caveats)
            })
            .transpose()
    }

    /// Install before starting any model tools or MCP children. If an existing
    /// unconfined session cannot protect the editor, leave that legacy session
    /// alone and make the editor unavailable. An active profile fails closed.
    pub(crate) fn protect(&mut self, active: &Caveats) -> anyhow::Result<()> {
        let result = self.make_protection().and_then(|guard| {
            let launch = newt_core::launch_authority::current();
            ensure!(
                !launch.ocap_disabled() && !launch.full_access() && !launch.unsafe_host_exec(),
                "workspace settings are unavailable in global-access or host-bypass sessions"
            );
            guard.validate_caveats(active)?;
            Ok(Arc::new(guard))
        });
        match result {
            Ok(guard) => self.protection = Some(guard),
            Err(error) if self.profile.is_some() => {
                return Err(error.context("workspace settings cannot protect operator authority"))
            }
            Err(error) => self.unavailable = Some(format!("{error:#}")),
        }
        Ok(())
    }

    fn make_protection(&self) -> anyhow::Result<WorkspaceProtection> {
        let config = self
            .config_path
            .as_ref()
            .context("no operator configuration directory")?;
        let key = self
            .key_path
            .as_ref()
            .context("no operator signing key path")?;
        let user = Config::user_config_dir().context("no operator configuration directory")?;
        // Computing locators must not create a secrets directory during a read.
        let secrets = user.join("secrets");
        let mut writes = vec![
            config
                .parent()
                .context("operator configuration needs a parent directory")?
                .to_path_buf(),
            user.clone(),
            config.clone(),
            durable_grants::store_path(config),
            config.with_file_name("ocap"),
            user.join("config.toml"),
            user.join("ocap"),
            std::env::current_exe()?,
        ];
        // Additional operator settings can select launch/identity resources.
        writes.push(user.join("settings.toml"));
        // Protect containing directories, including future key names and their
        // case aliases on case-insensitive filesystems. The operator host can
        // still read/edit these; model tools cannot receive these roots.
        WorkspaceProtection::new(
            &writes,
            &[
                key.parent()
                    .context("operator signing key needs a parent directory")?
                    .to_path_buf(),
                key.clone(),
                user,
                secrets.join("identity.txt"),
                secrets,
            ],
        )
    }

    pub(crate) fn validate_candidate(
        &self,
        workspace: &Path,
        profile: &WorkspaceProfile,
        permissions: &ToolPermissions,
    ) -> Result<(), String> {
        let guard = self.protection.as_ref().ok_or_else(|| {
            self.unavailable
                .clone()
                .unwrap_or_else(|| "workspace settings were not protected at startup".into())
        })?;
        let policy = profile
            .caveats(workspace, permissions)
            .map_err(|error| error.to_string())?;
        guard
            .validate_caveats(&policy)
            .map_err(|error| error.to_string())?;
        // The executor's calibrated build projection is part of the usable
        // coding boundary, including implicit runtime/toolchain reads.
        guard
            .validate_request(
                newt_core::DenialKind::Build,
                workspace.to_str().ok_or("workspace must be valid UTF-8")?,
            )
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
#[path = "workspace_launch_tests.rs"]
mod launch_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_store_is_read_only_and_does_not_generate_keys_or_directories() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("absent/config.toml");
        let key = temp.path().join("absent/identity.pem");
        assert!(read_snapshot(&config, &key).unwrap().profiles().is_empty());
        assert!(!config.parent().unwrap().exists());
    }

    #[test]
    fn empty_snapshot_retains_explicit_workspace_or_current_directory() {
        let snapshot = VerifiedSnapshot::empty().unwrap();
        let cwd = Path::new("/current");
        assert!(select_workspace(None, cwd, &snapshot)
            .unwrap()
            .path
            .is_none());
        let explicit = select_workspace(Some(Path::new("relative")), cwd, &snapshot).unwrap();
        assert_eq!(explicit.path, Some(PathBuf::from("relative")));
        assert!(!explicit.has_profile);
    }
}
