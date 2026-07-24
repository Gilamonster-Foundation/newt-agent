//! Portable, human-authored permission profiles.
//!
//! A profile is configuration, not authority. In particular an
//! [`PermissionProfileVerdict::ApproveCandidate`] imported from another
//! machine is deliberately inert: the local operator must review it and grant
//! it for this session or sign it into the local OCAP store. Deny/ask rules can
//! be applied immediately because they only narrow authority.

use crate::{CaveatProfile, PermissionPreset};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Current on-disk profile schema.
pub const PERMISSION_PROFILE_SCHEMA_VERSION: u32 = 1;

/// One capability family addressable by a portable profile rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAuthority {
    Exec,
    FsRead,
    FsWrite,
    Net,
    RemoteTool,
    GitWrite,
    ShellConstruct,
}

/// A portable rule's requested disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfileVerdict {
    /// Always refuse the exact target.
    Deny,
    /// Always ask the local operator about the exact target.
    Ask,
    /// A shareable suggestion only. Importing/applying it grants nothing.
    ApproveCandidate,
}

/// One exact-match rule in a permission profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionProfileRule {
    pub verdict: PermissionProfileVerdict,
    pub authority: PermissionAuthority,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A portable operator permission profile.
///
/// `base_preset` selects Newt's existing base tool policy. `clamp` is then
/// meet-ed with that base and with the live session capability, so applying a
/// profile in-process can only narrow. A requested widening is reported and
/// takes effect only after an explicit restart under that profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionProfile {
    pub schema_version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub base_preset: PermissionPreset,
    #[serde(default)]
    pub prompt: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default)]
    pub clamp: CaveatProfile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PermissionProfileRule>,
}

impl PermissionProfile {
    /// Parse and validate a profile.
    pub fn from_toml(input: &str) -> Result<Self, String> {
        let profile: Self = toml::from_str(input).map_err(|e| e.to_string())?;
        profile.validate()?;
        Ok(profile)
    }

    /// Render a stable, shareable TOML representation.
    pub fn to_toml(&self) -> Result<String, String> {
        self.validate()?;
        toml::to_string_pretty(self).map_err(|e| e.to_string())
    }

    /// Validate schema, path-safe name, and exact rule targets.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != PERMISSION_PROFILE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported permission profile schema {} (expected {})",
                self.schema_version, PERMISSION_PROFILE_SCHEMA_VERSION
            ));
        }
        validate_profile_name(&self.name)?;
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.target.trim().is_empty() {
                return Err(format!("rule {} has an empty target", index + 1));
            }
            if rule.target.contains('\0') {
                return Err(format!("rule {} target contains NUL", index + 1));
            }
        }
        Ok(())
    }
}

fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("permission profile name is empty".to_string());
    }
    if name.len() > 64 {
        return Err("permission profile name is longer than 64 bytes".to_string());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(
            "permission profile name may contain only ASCII letters, digits, '-' and '_'"
                .to_string(),
        );
    }
    Ok(())
}

/// Built-in templates. They are returned as ordinary profiles so a user can
/// save an editable copy and share the resulting TOML.
pub fn builtin_permission_profiles() -> BTreeMap<String, PermissionProfile> {
    use crate::{ScopeKeyword, ScopeSpec};

    let mut profiles = BTreeMap::new();
    let developer = PermissionProfile {
        schema_version: PERMISSION_PROFILE_SCHEMA_VERSION,
        name: "developer".to_string(),
        description: "Workspace development with human prompts for new authority.".to_string(),
        base_preset: PermissionPreset::WorkspaceDev,
        prompt: true,
        persona: None,
        clamp: CaveatProfile::default(),
        rules: Vec::new(),
    };
    profiles.insert(developer.name.clone(), developer);

    let coach = PermissionProfile {
        schema_version: PERMISSION_PROFILE_SCHEMA_VERSION,
        name: "coach".to_string(),
        description: "Advice and review: read access only, with mutation disabled.".to_string(),
        base_preset: PermissionPreset::ReadOnly,
        prompt: true,
        persona: Some("coach".to_string()),
        clamp: CaveatProfile {
            fs_read: ScopeSpec::Keyword(ScopeKeyword::All),
            fs_write: ScopeSpec::Keyword(ScopeKeyword::None),
            exec: ScopeSpec::Keyword(ScopeKeyword::None),
            net: ScopeSpec::Keyword(ScopeKeyword::None),
            max_calls: None,
        },
        rules: Vec::new(),
    };
    profiles.insert(coach.name.clone(), coach);

    let autonomous = PermissionProfile {
        schema_version: PERMISSION_PROFILE_SCHEMA_VERSION,
        name: "autonomous-developer".to_string(),
        description: "Workspace development without routine prompts; structural guards and \
                      explicit network limits remain in force."
            .to_string(),
        base_preset: PermissionPreset::WorkspaceDev,
        prompt: false,
        persona: None,
        clamp: CaveatProfile::default(),
        rules: Vec::new(),
    };
    profiles.insert(autonomous.name.clone(), autonomous);
    profiles
}

/// Load one profile file. Imported approval candidates remain inert by type;
/// this function never writes to or signs the OCAP store.
pub fn load_permission_profile(path: &Path) -> Result<PermissionProfile, String> {
    let input = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    PermissionProfile::from_toml(&input)
        .map_err(|e| format!("invalid permission profile {}: {e}", path.display()))
}

/// Load all `*.toml` profiles in a directory, layered over the built-ins.
/// A user file with a built-in name intentionally supplies the editable local
/// override. Invalid files are skipped and returned as warnings.
pub fn load_permission_profiles(dir: &Path) -> (BTreeMap<String, PermissionProfile>, Vec<String>) {
    let mut profiles = builtin_permission_profiles();
    let mut warnings = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (profiles, warnings),
        Err(e) => {
            warnings.push(format!("cannot read {}: {e}", dir.display()));
            return (profiles, warnings);
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    for path in paths {
        match load_permission_profile(&path) {
            Ok(profile) => {
                profiles.insert(profile.name.clone(), profile);
            }
            Err(e) => warnings.push(e),
        }
    }
    (profiles, warnings)
}

/// Save an editable/shareable profile as `<dir>/<name>.toml`.
pub fn save_permission_profile(dir: &Path, profile: &PermissionProfile) -> Result<PathBuf, String> {
    profile.validate()?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.toml", profile.name));
    write_permission_profile(&path, profile)?;
    Ok(path)
}

/// Atomically write one validated profile to an explicit path.
///
/// The caller owns any overwrite confirmation. The replacement itself is
/// staged in a mode-0600 sibling, synced, and renamed so interruption cannot
/// leave a truncated profile behind.
pub fn write_permission_profile(path: &Path, profile: &PermissionProfile) -> Result<(), String> {
    profile.validate()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let body = profile.to_toml()?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("permission-profile"),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|e| format!("cannot create {}: {e}", temp.display()))?;
        file.write_all(body.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", temp.display()))?;
        file.sync_all()
            .map_err(|e| format!("cannot sync {}: {e}", temp.display()))?;
        atomic_replace(&temp, path).map_err(|e| {
            format!(
                "cannot replace {} with {}: {e}",
                path.display(),
                temp.display()
            )
        })?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both buffers are live and NUL-terminated for the call. The
    // staging file is a sibling of the destination, so replacement remains
    // on one volume and can be atomic.
    let success = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if success != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        match std::fs::File::open(path).and_then(|directory| directory.sync_all()) {
            Ok(()) => Ok(()),
            // Directory fsync is unavailable on some otherwise usable Unix
            // filesystems. The profile file itself has already been synced.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(format!("cannot sync directory {}: {error}", path.display())),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_cover_requested_operator_roles() {
        let profiles = builtin_permission_profiles();
        assert!(profiles.contains_key("developer"));
        assert!(profiles.contains_key("coach"));
        assert!(profiles.contains_key("autonomous-developer"));
        assert_eq!(
            profiles["coach"].clamp.fs_write,
            crate::ScopeSpec::Keyword(crate::ScopeKeyword::None)
        );
        assert!(
            !profiles["autonomous-developer"].prompt,
            "autonomous disables routine prompting, not structural guards"
        );
    }

    #[test]
    fn profile_round_trips_and_candidate_does_not_become_authority() {
        let mut profile = builtin_permission_profiles().remove("developer").unwrap();
        profile.rules.push(PermissionProfileRule {
            verdict: PermissionProfileVerdict::ApproveCandidate,
            authority: PermissionAuthority::Exec,
            target: "cargo".to_string(),
            note: Some("review locally".to_string()),
        });
        let reparsed = PermissionProfile::from_toml(&profile.to_toml().unwrap()).unwrap();
        assert_eq!(reparsed, profile);
        assert_eq!(
            reparsed.rules[0].verdict,
            PermissionProfileVerdict::ApproveCandidate
        );
    }

    #[test]
    fn rejects_path_traversal_names_and_unknown_schema() {
        let mut profile = builtin_permission_profiles().remove("developer").unwrap();
        profile.name = "../escape".to_string();
        assert!(profile.validate().unwrap_err().contains("may contain only"));
        profile.name = "safe".to_string();
        profile.schema_version += 1;
        assert!(profile.validate().unwrap_err().contains("unsupported"));
    }

    #[test]
    fn loads_user_overrides_and_skips_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = builtin_permission_profiles().remove("developer").unwrap();
        profile.description = "local override".to_string();
        save_permission_profile(dir.path(), &profile).unwrap();
        std::fs::write(dir.path().join("bad.toml"), "not = [toml").unwrap();

        let (profiles, warnings) = load_permission_profiles(dir.path());
        assert_eq!(profiles["developer"].description, "local override");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn profile_replacement_is_complete_and_leaves_no_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = builtin_permission_profiles().remove("developer").unwrap();
        save_permission_profile(dir.path(), &profile).unwrap();
        profile.description = "replacement".to_string();
        let path = save_permission_profile(dir.path(), &profile).unwrap();

        assert_eq!(
            load_permission_profile(&path).unwrap().description,
            "replacement"
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
    }
}
