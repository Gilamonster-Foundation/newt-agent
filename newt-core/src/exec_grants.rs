//! Pin session executable basenames without trusting model-writable PATH entries.
use crate::{Caveats, Scope};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The candidate PATH dispatch resolves against: the venv, explicit exec roots, the carried
/// command pack beside newt (#3151) and developer tools, ahead of the host's own.
pub fn dispatch_path() -> Option<std::ffi::OsString> {
    crate::agentic::tools::dispatch_exec_path()
}

/// Whether a session holding `exec` may run the carried command pack at `dir` (#3151). On
/// macOS a bare-name grant covers only the system directories, so a pack tool runs only when
/// its exact path was pinned at startup, and pinning refuses an untrusted directory (group-
/// or other-writable, or inside a tree the session can write). A pack ahead of the host's
/// `PATH` that was not pinned would shadow a tool the kernel lets run with one it refuses.
#[must_use]
pub(crate) fn pack_is_granted(dir: &Path, exec: &Scope<String>) -> bool {
    match exec {
        Scope::All => true,
        Scope::Only(grants) => grants.iter().any(|grant| Path::new(grant).starts_with(dir)),
    }
}

/// The startup notice for a session: [`startup_notice`], plus one sentence when a carried
/// pack sits beside newt, trust refused a pin, and none of the pack could be pinned, so the
/// host's own commands run instead.
#[must_use]
pub fn session_exec_notice(unpinned: &[Unpinned], exec: &Scope<String>) -> Option<String> {
    session_exec_notice_with(
        unpinned,
        exec,
        crate::agentic::tools::carried_tools_dir().as_deref(),
    )
}

fn session_exec_notice_with(
    unpinned: &[Unpinned],
    exec: &Scope<String>,
    pack: Option<&Path>,
) -> Option<String> {
    let base = startup_notice(unpinned);
    let refused = unpinned.iter().any(|u| {
        matches!(
            u.reason,
            UnpinnedReason::Refused(_) | UnpinnedReason::Untrusted
        )
    });
    let Some(dir) = pack.filter(|dir| refused && !pack_is_granted(dir, exec)) else {
        return base;
    };
    let sentence = format!(
        "The carried commands in {} are not on the shell's PATH: none could be pinned for \
         this session, so the host's own run instead.",
        dir.display()
    );
    Some(base.map_or(sentence.clone(), |base| format!("{base} {sentence}")))
}

pub(crate) fn trusted_program(path: &OsStr, caveats: &Caveats, name: &str) -> Result<PathBuf, ()> {
    classify_program(path, caveats, name).map_err(|_| ())
}

/// Why a program could not be trusted for exec (#3152).
#[derive(Debug)]
enum ProgramFailure {
    /// No program of that name on the PATH, or one that cannot be resolved.
    NotFound,
    /// Found, and trust refused it, but with no writable directory to name: it
    /// sits inside a tree this session can write, or could not be inspected, or
    /// the PATH has a relative entry.
    Untrusted,
    /// Found, and trust refused it because these path components are group- or
    /// other-writable.
    Writable(Vec<PathBuf>),
}

/// [`trusted_program`], keeping why it failed.
fn classify_program(
    path: &OsStr,
    caveats: &Caveats,
    name: &str,
) -> Result<PathBuf, ProgramFailure> {
    use ProgramFailure::{NotFound, Untrusted, Writable};
    if std::env::split_paths(path).any(|dir| !dir.is_absolute()) {
        return Err(Untrusted);
    }
    let program = crate::git_hardening::resolve_trusted_program(Path::new("."), Some(path), name)
        .map_err(|_| NotFound)?;
    if !program.is_absolute() {
        return Err(Untrusted);
    }
    let resolved = program.canonicalize().map_err(|_| NotFound)?;
    let mut trust =
        crate::git_staging::TrustContext::bind(&caveats.fs_write).map_err(|_| Untrusted)?;
    if name == "git" {
        trust = trust.with_git(&resolved);
    }
    crate::git_staging::trust_check(&program, &trust).map_err(|refusal| {
        let dirs: Vec<PathBuf> = refusal
            .hints()
            .iter()
            .map(|hint| hint.path.clone())
            .collect();
        if dirs.is_empty() {
            Untrusted
        } else {
            Writable(dirs)
        }
    })?;
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

/// A granted basename that stayed a basename, and why (#3152).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpinned {
    pub name: String,
    pub reason: UnpinnedReason,
}

/// Why a granted basename was not pinned to a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnpinnedReason {
    /// No trustworthy program of that name on the dispatch PATH (not installed,
    /// or the PATH has a relative entry).
    NotFound,
    /// A program was found, and trust refused it: these path components are
    /// group- or other-writable.
    Refused(Vec<PathBuf>),
    /// A program was found, and trust refused it with no writable directory to
    /// name: it sits inside a tree this session can write, could not be
    /// inspected, or the PATH has a relative entry. `newt doctor` cannot repair
    /// this with a `chmod`.
    Untrusted,
    /// Pins are declined outright under unrestricted writes.
    Declined,
}

impl Unpinned {
    /// The writable components that refused this name, if it was refused.
    #[must_use]
    pub fn writable(&self) -> &[PathBuf] {
        match &self.reason {
            UnpinnedReason::Refused(dirs) => dirs,
            _ => &[],
        }
    }
}

/// Add exact trusted paths to a session's basename grants. Return pins for
/// narrowing-only refresh and unresolved names for one startup notice.
pub fn resolve_basenames(caveats: &mut Caveats, path: Option<&OsStr>) -> (ExecPins, Vec<String>) {
    let (pins, unpinned) = resolve_basenames_detailed(caveats, path);
    (pins, unpinned.into_iter().map(|u| u.name).collect())
}

/// [`resolve_basenames`], keeping why each name stayed a basename.
pub fn resolve_basenames_detailed(
    caveats: &mut Caveats,
    path: Option<&OsStr>,
) -> (ExecPins, Vec<Unpinned>) {
    let mut pins = ExecPins::default();
    let mut unresolved = Vec::new();
    if let Scope::Only(grants) = &caveats.exec {
        for name in grants
            .iter()
            .filter(|name| !name.is_empty() && !name.contains(['/', '\\']))
        {
            // Unlike the Git broker's explicitly ambient trust policy, a
            // restricted exec pin must be outside ALL effective write authority.
            let found = path
                .filter(|_| !matches!(caveats.fs_write, Scope::All))
                .map(|path| classify_program(path, caveats, name));
            let reason = match found {
                Some(Ok(path)) if path.to_str().is_some() => {
                    pins.0
                        .insert(name.clone(), path.to_string_lossy().into_owned());
                    continue;
                }
                // Trusted, but not representable as a grant path.
                Some(Ok(_)) | Some(Err(ProgramFailure::Untrusted)) => UnpinnedReason::Untrusted,
                Some(Err(ProgramFailure::Writable(dirs))) => UnpinnedReason::Refused(dirs),
                _ if matches!(caveats.fs_write, Scope::All) => UnpinnedReason::Declined,
                _ => UnpinnedReason::NotFound,
            };
            unresolved.push(Unpinned {
                name: name.clone(),
                reason,
            });
        }
    }
    pins.apply(caveats);
    (pins, unresolved)
}

/// rustup's proxies: wrappers that re-exec the selected toolchain's binary. A
/// pinned wrapper path admits the wrapper, not what it runs, and Seatbelt allows
/// only literal exec paths, so neither a path nor a `chmod` makes them run in a
/// confined `run_command`; builds and tests go through `lifecycle action=build`.
/// Pure DATA (three-Cs): a new proxy is a data edit.
pub const TOOLCHAIN_PROXY_EXEC: &[&str] = &["cargo", "clippy-driver", "rustc", "rustfmt", "rustup"];

/// Is `target` one of [`TOOLCHAIN_PROXY_EXEC`]? Compares the file-name component.
#[must_use]
pub fn is_toolchain_proxy(target: &str) -> bool {
    let target = target.trim();
    let name = Path::new(target)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(target);
    TOOLCHAIN_PROXY_EXEC.contains(&name)
}

/// The startup notice for grants that stayed basenames (#3152). A name refused
/// for a writable directory gets that directory and `newt doctor`; a toolchain
/// proxy gets the build lane instead, since no path or `chmod` would make it run
/// confined; names not installed, and names declined under unrestricted writes,
/// each get their own short clause so the causes are not mixed in one list.
#[must_use]
pub fn startup_notice(unpinned: &[Unpinned]) -> Option<String> {
    let blocked = unpinned.iter().filter(|u| {
        matches!(
            u.reason,
            UnpinnedReason::Refused(_) | UnpinnedReason::Untrusted
        )
    });
    let (proxies, others): (Vec<_>, Vec<_>) = blocked.partition(|u| is_toolchain_proxy(&u.name));
    let (writable, untrusted): (Vec<_>, Vec<_>) = others
        .into_iter()
        .partition(|u| matches!(u.reason, UnpinnedReason::Refused(_)));
    let names = |group: &[&Unpinned]| {
        group
            .iter()
            .map(|u| u.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let with = |reason: fn(&UnpinnedReason) -> bool| {
        unpinned
            .iter()
            .filter(|u| reason(&u.reason))
            .collect::<Vec<_>>()
    };
    let mut parts = Vec::new();
    if !writable.is_empty() {
        let dirs: std::collections::BTreeSet<String> = writable
            .iter()
            .flat_map(|u| u.writable())
            .map(|dir| dir.display().to_string())
            .collect();
        parts.push(format!(
            "Executable grants kept as basenames because a directory on their path is group- or other-writable: {} (writable: {}). `newt doctor` shows the repair.",
            names(&writable),
            dirs.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if !untrusted.is_empty() {
        parts.push(format!(
            "Executable grants kept as basenames because the program is not trusted for this session (inside a tree it can write, a relative PATH entry, or a path that could not be inspected): {}.",
            names(&untrusted)
        ));
    }
    if !proxies.is_empty() {
        parts.push(format!(
            "{}: toolchain wrappers do not run confined from a path; run builds and tests with `lifecycle action=build`.",
            names(&proxies)
        ));
    }
    let missing = with(|r| matches!(r, UnpinnedReason::NotFound));
    if !missing.is_empty() {
        parts.push(format!("Not found on PATH: {}.", names(&missing)));
    }
    let declined = with(|r| matches!(r, UnpinnedReason::Declined));
    if !declined.is_empty() {
        parts.push(format!(
            "Executable grants stay basenames under unrestricted writes: {}.",
            names(&declined)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

#[cfg(test)]
mod pack_tests {
    use super::*;

    fn only(grants: &[&str]) -> Scope<String> {
        Scope::only(grants.iter().map(|grant| (*grant).to_string()))
    }

    /// #3151: on macOS a bare name grants only the system directories, so the pack is
    /// usable only through a pinned exact path inside it.
    #[test]
    fn a_pack_is_granted_only_by_a_pinned_path_inside_it() {
        let dir = Path::new("/opt/newt/tools");
        assert!(pack_is_granted(dir, &Scope::All));
        assert!(pack_is_granted(
            dir,
            &only(&["tail", "/opt/newt/tools/tail"])
        ));
        assert!(
            !pack_is_granted(dir, &only(&["tail", "grep"])),
            "bare names do not reach it"
        );
        assert!(!pack_is_granted(dir, &only(&["/usr/bin/tail"])));
        assert!(
            !pack_is_granted(dir, &only(&["/opt/newt/tools2/tail"])),
            "a sibling directory is not the pack"
        );
        assert!(!pack_is_granted(dir, &Scope::none()));
    }

    /// #3151: the startup notice says an ungranted pack is off the PATH only when trust
    /// refused a pin, and leaves the existing notice untouched otherwise.
    #[test]
    fn the_notice_names_an_ungranted_pack_only_after_a_refused_pin() {
        let pack = Path::new("/opt/newt/tools");
        let refused = [Unpinned {
            name: "tail".into(),
            reason: UnpinnedReason::Refused(vec!["/opt/homebrew/Cellar".into()]),
        }];
        let notice = session_exec_notice_with(&refused, &only(&["tail"]), Some(pack)).unwrap();
        assert!(
            notice.starts_with(&startup_notice(&refused).unwrap()),
            "{notice}"
        );
        assert!(
            notice.contains("/opt/newt/tools") && notice.contains("host's own"),
            "{notice}"
        );

        let pinned = only(&["tail", "/opt/newt/tools/tail"]);
        assert_eq!(
            session_exec_notice_with(&refused, &pinned, Some(pack)),
            startup_notice(&refused),
            "a granted pack is not mentioned"
        );
        let missing = [Unpinned {
            name: "rg".into(),
            reason: UnpinnedReason::NotFound,
        }];
        assert_eq!(
            session_exec_notice_with(&missing, &only(&["rg"]), Some(pack)),
            startup_notice(&missing),
            "nothing was refused, so the pack is not mentioned"
        );
        assert_eq!(
            session_exec_notice_with(&refused, &only(&["tail"]), None),
            startup_notice(&refused),
            "no pack, the notice is unchanged"
        );
        assert_eq!(
            session_exec_notice_with(&[], &only(&["tail"]), Some(pack)),
            None
        );
    }
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

    /// Grounds `a_pack_is_granted_only_by_a_pinned_path_inside_it` (and the confined
    /// gate built on it, #3151): with real directories and modes, pinning grants a pack
    /// in an owner-only directory, and leaves one ungranted when a group-writable
    /// directory or a session write root refuses it, so the gate keeps it off the PATH.
    #[test]
    #[ignore = "real filesystem and permissions; weekly/release real-resource tier"]
    fn pinning_grants_a_trusted_pack_and_refuses_an_untrusted_one() {
        let temp = tempfile::tempdir().unwrap();
        let mode = |path: &Path, bits: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).unwrap();
        };
        mode(temp.path(), 0o700);
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        mode(&workspace, 0o700);
        // A pack at `<root>/tools/tail`, the directory made with `directory_mode`.
        let pack = |root: &Path, directory_mode: u32| {
            std::fs::create_dir_all(root.join("tools")).unwrap();
            mode(root, directory_mode);
            mode(&root.join("tools"), directory_mode);
            let tool = root.join("tools/tail");
            std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
            mode(&tool, 0o700);
            std::fs::canonicalize(root.join("tools")).unwrap()
        };
        let trusted = pack(&temp.path().join("trusted"), 0o700);
        let group_writable = pack(&temp.path().join("keg"), 0o770);
        let under_workspace = pack(&workspace.join("newt"), 0o700);

        let granted = |dir: &Path| {
            let mut caveats = Caveats {
                exec: Scope::only(["tail".into()]),
                fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
                net: Scope::none(),
                ..Caveats::top()
            };
            resolve_basenames(&mut caveats, Some(dir.as_os_str()));
            pack_is_granted(dir, &caveats.exec)
        };
        assert!(
            granted(&trusted),
            "an owner-only pack is pinned, so it is granted"
        );
        assert!(
            !granted(&group_writable),
            "a group-writable keg refuses the pin"
        );
        assert!(
            !granted(&under_workspace),
            "a pack inside a write root refuses the pin"
        );
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

/// #3152: why a granted name stayed a basename, and what the startup notice
/// tells the operator about it.
#[cfg(all(test, unix))]
mod unpinned_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn dir(path: &Path, mode: u32) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A name with no program on PATH and a name refused for a group-writable
    /// directory are different: only the second carries the directory.
    #[test]
    fn an_unpinned_name_says_missing_or_names_the_writable_directory() {
        let temp = tempfile::tempdir().unwrap();
        dir(temp.path(), 0o700);
        let (bin, workspace) = (temp.path().join("bin"), temp.path().join("workspace"));
        dir(&bin, 0o775);
        dir(&workspace, 0o700);
        let tool = bin.join("tool");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut caveats = Caveats {
            exec: Scope::only(["absent".into(), "tool".into()]),
            fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
            net: Scope::none(),
            ..Caveats::top()
        };
        let (_, unpinned) = resolve_basenames_detailed(&mut caveats, Some(bin.as_os_str()));
        assert_eq!(unpinned.len(), 2, "{unpinned:?}");
        assert_eq!(unpinned[0].name, "absent");
        assert_eq!(unpinned[0].reason, UnpinnedReason::NotFound);
        assert_eq!(unpinned[1].name, "tool");
        assert!(
            unpinned[1]
                .writable()
                .iter()
                .any(|p| p == &bin || p == &bin.canonicalize().unwrap()),
            "the group-writable directory is named: {unpinned:?}"
        );
        // The plain-name view is unchanged.
        let (_, names) = resolve_basenames(&mut caveats, Some(bin.as_os_str()));
        assert_eq!(names, ["absent", "tool"]);
        // A program inside a tree the SESSION can write is found but untrusted: it
        // is not "not found", and there is no writable directory to name (#3152
        // review). A relative PATH entry is untrusted too.
        let inner = workspace.join("inner");
        std::fs::write(&inner, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut writable_tree = Caveats {
            exec: Scope::only(["inner".into()]),
            fs_write: Scope::only([workspace.to_string_lossy().into_owned()]),
            net: Scope::none(),
            ..Caveats::top()
        };
        let (_, untrusted) =
            resolve_basenames_detailed(&mut writable_tree, Some(workspace.as_os_str()));
        assert_eq!(
            untrusted[0].reason,
            UnpinnedReason::Untrusted,
            "{untrusted:?}"
        );
        let (_, relative) = resolve_basenames_detailed(
            &mut writable_tree,
            Some(
                std::env::join_paths([Path::new("."), &bin])
                    .unwrap()
                    .as_os_str(),
            ),
        );
        assert_eq!(
            relative[0].reason,
            UnpinnedReason::Untrusted,
            "{relative:?}"
        );
        // Unrestricted writes decline pins outright: its own reason.
        let mut ambient = Caveats {
            exec: Scope::only(["echo".into()]),
            ..Caveats::top()
        };
        let (_, declined) = resolve_basenames_detailed(&mut ambient, Some(bin.as_os_str()));
        assert_eq!(declined[0].reason, UnpinnedReason::Declined);
    }

    fn unpinned(name: &str, writable: &[&str]) -> Unpinned {
        let reason = if writable.is_empty() {
            UnpinnedReason::NotFound
        } else {
            UnpinnedReason::Refused(writable.iter().map(PathBuf::from).collect())
        };
        Unpinned {
            name: name.into(),
            reason,
        }
    }

    /// Names that are simply not installed, and names declined under
    /// unrestricted writes, each get their own clause; neither is dressed up as a
    /// permissions problem, and nothing is said when nothing is unpinned.
    #[test]
    fn the_notice_keeps_missing_and_declined_names_in_their_own_clauses() {
        assert_eq!(startup_notice(&[]), None);
        let missing = startup_notice(&[unpinned("fd", &[]), unpinned("pip", &[])]).unwrap();
        assert_eq!(missing, "Not found on PATH: fd, pip.");
        let declined = startup_notice(&[Unpinned {
            name: "echo".into(),
            reason: UnpinnedReason::Declined,
        }])
        .unwrap();
        assert_eq!(
            declined,
            "Executable grants stay basenames under unrestricted writes: echo."
        );
    }

    /// A found-but-untrusted name is never reported as not found (#3152 review):
    /// it gets its own clause, with no directory and no chmod promise.
    #[test]
    fn an_untrusted_name_is_not_reported_as_not_found() {
        let untrusted = |name: &str| Unpinned {
            name: name.into(),
            reason: UnpinnedReason::Untrusted,
        };
        let notice = startup_notice(&[untrusted("gh"), unpinned("fd", &[])]).unwrap();
        assert!(notice.contains("not trusted for this session"), "{notice}");
        assert!(
            notice.contains("gh") && notice.contains("Not found on PATH: fd."),
            "{notice}"
        );
        assert!(!notice.contains("Not found on PATH: gh"), "{notice}");
        assert!(
            !notice.contains("chmod") && !notice.contains("newt doctor"),
            "{notice}"
        );
        // A wrapper that is untrusted is sent to the build lane like a refused one.
        let proxy = startup_notice(&[untrusted("cargo")]).unwrap();
        assert!(proxy.contains("lifecycle action=build"), "{proxy}");
        assert!(!proxy.contains("not trusted for this session"), "{proxy}");
    }

    /// A refused name gets its directory and the way to the fix.
    #[test]
    fn the_notice_names_each_refused_directory_and_points_at_doctor() {
        let notice = startup_notice(&[
            unpinned("fd", &[]),
            unpinned("npm", &["/opt/homebrew/lib"]),
            unpinned("rg", &["/opt/homebrew/lib", "/opt/homebrew/opt"]),
        ])
        .unwrap();
        assert!(notice.contains("npm") && notice.contains("rg"), "{notice}");
        // The missing name is in its own clause, not in the permissions one.
        assert!(notice.contains("Not found on PATH: fd."), "{notice}");
        assert!(
            !notice.contains("or other-writable: fd"),
            "the permissions clause names refused programs only: {notice}"
        );
        for dir in ["/opt/homebrew/lib", "/opt/homebrew/opt"] {
            assert_eq!(
                notice.matches(dir).count(),
                1,
                "each directory once: {notice}"
            );
        }
        assert!(notice.contains("newt doctor"), "{notice}");
        assert!(!notice.contains("lifecycle"), "{notice}");
    }

    /// The rustup proxies are wrappers that re-exec the selected toolchain, so a
    /// pinned wrapper path does not make them run confined: neither a path nor a
    /// chmod is suggested, builds go through `lifecycle action=build`.
    #[test]
    fn toolchain_proxies_are_sent_to_the_build_lane_not_to_a_path() {
        let notice = startup_notice(&[
            unpinned("cargo", &["/opt/homebrew/opt"]),
            unpinned("rustc", &["/opt/homebrew/opt"]),
        ])
        .unwrap();
        assert!(
            notice.contains("cargo") && notice.contains("rustc"),
            "{notice}"
        );
        assert!(notice.contains("lifecycle action=build"), "{notice}");
        assert!(!notice.contains("/opt/homebrew/opt"), "{notice}");
        assert!(!notice.contains("chmod"), "{notice}");
        assert!(is_toolchain_proxy("cargo") && is_toolchain_proxy("/x/bin/rustup"));
        assert!(!is_toolchain_proxy("npm"));
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
