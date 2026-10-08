//! Pin session executable basenames without trusting model-writable PATH entries.
use crate::{Caveats, Scope};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The same PATH (venv, explicit exec roots, developer tools) used by dispatch.
pub fn dispatch_path() -> Option<std::ffi::OsString> {
    crate::agentic::tools::dispatch_exec_path()
}

pub(crate) fn trusted_program(path: &OsStr, caveats: &Caveats, name: &str) -> Result<PathBuf, ()> {
    if std::env::split_paths(path).any(|dir| !dir.is_absolute()) {
        return Err(());
    }
    let program = crate::git_hardening::resolve_trusted_program(Path::new("."), Some(path), name)
        .map_err(|_| ())?;
    if !program.is_absolute() {
        return Err(());
    }
    let resolved = program.canonicalize().map_err(|_| ())?;
    let mut trust = crate::git_staging::TrustContext::bind(&caveats.fs_write).map_err(|_| ())?;
    if name == "git" {
        trust = trust.with_git(&resolved);
    }
    crate::git_staging::trust_check(&program, &trust).map_err(|_| ())?;
    Ok(resolved)
}

/// Executable paths captured at session startup, never re-resolved on reload.
#[derive(Default)]
pub struct ExecPins(std::collections::BTreeMap<String, String>);

impl ExecPins {
    /// Retain a pinned path only while its original basename remains granted.
    pub fn apply(&self, caveats: &mut Caveats) {
        if let Scope::Only(grants) = &mut caveats.exec {
            for (name, path) in &self.0 {
                if grants.contains(name) {
                    grants.insert(path.clone());
                }
            }
        }
    }
}

/// Add exact trusted paths to a session's basename grants. Return pins for
/// narrowing-only refresh and unresolved names for one startup notice.
pub fn resolve_basenames(caveats: &mut Caveats, path: Option<&OsStr>) -> (ExecPins, Vec<String>) {
    let mut pins = ExecPins::default();
    let mut unresolved = Vec::new();
    if let Scope::Only(grants) = &caveats.exec {
        for name in grants
            .iter()
            .filter(|name| !name.is_empty() && !name.contains(['/', '\\']))
        {
            // Unlike the Git broker's explicitly ambient trust policy, a
            // restricted exec pin must be outside ALL effective write authority.
            match path
                .filter(|_| !matches!(caveats.fs_write, Scope::All))
                .and_then(|path| trusted_program(path, caveats, name).ok())
                .and_then(|path| path.to_str().map(str::to_owned))
            {
                Some(path) => {
                    pins.0.insert(name.clone(), path);
                }
                None => unresolved.push(name.clone()),
            }
        }
    }
    pins.apply(caveats);
    (pins, unresolved)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// PR #2816: ambient writes do not make a restricted executable trustworthy.
    #[test]
    fn unrestricted_writes_decline_exec_pins() {
        let mut policy = Caveats {
            exec: Scope::only(["sh".into()]),
            ..Caveats::top()
        };
        let before = policy.clone();
        assert_eq!(
            resolve_basenames(&mut policy, Some(OsStr::new("/bin"))).1,
            ["sh"]
        );
        assert_eq!(policy, before);
    }

    /// agent-bridle #421: a logical basename must gain its exact trusted path,
    /// but writable/relative lookup must never become a kernel exec grant.
    #[test]
    fn basename_grants_pin_only_trusted_dispatch_paths() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = temp.path().join("bin");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o700)).unwrap();
        let program = bin.join("gh-real");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&program, bin.join("gh")).unwrap();
        let base = Caveats {
            exec: Scope::only(["gh".into()]),
            fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
            net: Scope::none(),
            ..Caveats::top()
        };
        let mut granted = base.clone();
        let (pins, missing) = resolve_basenames(&mut granted, Some(bin.as_os_str()));
        assert!(missing.is_empty());
        assert_eq!(
            granted.exec,
            Scope::only([
                "gh".into(),
                program
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            ])
        );
        assert_eq!(granted.fs_write, base.fs_write);
        assert_eq!(granted.fs_read, base.fs_read);
        assert_eq!(granted.net, base.net);
        let mut refreshed = base.clone();
        pins.apply(&mut refreshed);
        assert_eq!(refreshed, granted);
        refreshed.exec = Scope::none();
        pins.apply(&mut refreshed);
        assert_eq!(refreshed.exec, Scope::none());
        for path in [
            Some(std::env::join_paths([Path::new("."), &bin]).unwrap()),
            None,
        ] {
            let mut denied = base.clone();
            assert_eq!(resolve_basenames(&mut denied, path.as_deref()).1, ["gh"]);
            assert_eq!(denied, base);
        }
        let mut writable = base.clone();
        writable.fs_write = Scope::only([bin.to_string_lossy().into_owned()]);
        let original = writable.clone();
        assert_eq!(
            resolve_basenames(&mut writable, Some(bin.as_os_str())).1,
            ["gh"]
        );
        assert_eq!(writable, original);
        // An earlier writable executable must not be skipped for a later trusted one.
        let shadow = workspace.join("gh");
        std::fs::write(&shadow, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o700)).unwrap();
        let shadow_path = std::env::join_paths([&workspace, &bin]).unwrap();
        let mut denied = base.clone();
        assert_eq!(resolve_basenames(&mut denied, Some(&shadow_path)).1, ["gh"]);
        assert_eq!(denied, base);
        // Repointing PATH's symlink cannot change a previously captured pin.
        std::fs::remove_file(bin.join("gh")).unwrap();
        symlink(&shadow, bin.join("gh")).unwrap();
        let mut reloaded = base.clone();
        pins.apply(&mut reloaded);
        assert_eq!(reloaded, granted);
        let mut symlink_denied = base.clone();
        assert_eq!(
            resolve_basenames(&mut symlink_denied, Some(bin.as_os_str())).1,
            ["gh"]
        );
        assert_eq!(symlink_denied, base);
        std::fs::remove_file(bin.join("gh")).unwrap();
        symlink(&program, bin.join("gh")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o777)).unwrap();
        let mut untrusted = base.clone();
        assert_eq!(
            resolve_basenames(&mut untrusted, Some(bin.as_os_str())).1,
            ["gh"]
        );
        assert_eq!(untrusted, base);
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    /// #421: unrestricted, empty, and already explicit grants are not expanded.
    #[test]
    fn non_basename_grants_are_unchanged() {
        for exec in [
            Scope::All,
            Scope::none(),
            Scope::only(["/explicit/tool".into(), "./relative/tool".into()]),
        ] {
            let mut caveats = Caveats {
                exec,
                ..Caveats::top()
            };
            let before = caveats.clone();
            let (_, missing) = resolve_basenames(&mut caveats, None);
            assert!(missing.is_empty());
            assert_eq!(caveats, before);
        }
    }
}
