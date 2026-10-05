//! The staging trust walk, shared by enforcement and operator diagnostics.
use super::*;

#[derive(Clone, Copy)]
struct PathMetadata {
    uid: u32,
    mode: u32,
    is_dir: bool,
    is_symlink: bool,
}

fn metadata(path: &Path) -> std::io::Result<PathMetadata> {
    let meta = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    let (uid, mode) = {
        use std::os::unix::fs::MetadataExt;
        (meta.uid(), meta.mode())
    };
    #[cfg(not(unix))]
    let (uid, mode) = (0, 0);
    Ok(PathMetadata {
        uid,
        mode,
        is_dir: meta.is_dir(),
        is_symlink: meta.file_type().is_symlink(),
    })
}

/// Return every writable component on both the original and resolved path.
/// Uses exactly the staging policy, including its platform exceptions.
///
/// # Errors
/// Refuses relative, unresolved, uninspectable, or model-writable paths.
pub fn tool_path_trust_hints(path: &Path, ctx: &TrustContext) -> Result<Vec<TrustHint>, Refusal> {
    #[cfg(unix)]
    let uid = effective_uid();
    #[cfg(not(unix))]
    let uid = 0;
    inspect(path, ctx, uid, |p| std::fs::canonicalize(p), metadata)
}

fn inspect(
    path: &Path,
    ctx: &TrustContext,
    uid: u32,
    resolve: impl FnOnce(&Path) -> std::io::Result<PathBuf>,
    metadata: impl Fn(&Path) -> std::io::Result<PathMetadata>,
) -> Result<Vec<TrustHint>, Refusal> {
    if !path.is_absolute() {
        return Err(format!(
            "'{}' is not an absolute path; refusing to trust it",
            path.display()
        )
        .into());
    }
    let resolved = resolve(path).map_err(|e| {
        format!(
            "'{}' cannot be resolved ({e}); refusing to trust it",
            path.display()
        )
    })?;
    let mut hints = Vec::new();
    for spelling in [path, resolved.as_path()] {
        let mut below_owner = None;
        for component in spelling.ancestors() {
            if ctx.excludes(component) {
                return Err(format!("governed push refused: '{}' is inside a model-writable tree; the file must live outside the session's writable roots", shorten_home(component)).into());
            }
            let meta = metadata(component).map_err(|e| {
                format!(
                    "governed push refused: '{}' cannot be inspected ({e})",
                    shorten_home(component)
                )
            })?;
            if !meta.is_symlink && writable_by_others(&meta, below_owner, component, ctx, path, uid)
            {
                let hint = TrustHint {
                    path: component.to_path_buf(),
                    chmod_arg: match (meta.mode & 0o020 != 0, meta.mode & 0o002 != 0) {
                        (true, true) => "g-w,o-w",
                        (true, false) => "g-w",
                        _ => "o-w",
                    },
                    mode: meta.mode,
                };
                if !hints.contains(&hint) {
                    hints.push(hint);
                }
            }
            below_owner = Some(meta.uid);
        }
    }
    Ok(hints)
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn writable_by_others(
    meta: &PathMetadata,
    below_owner: Option<u32>,
    path: &Path,
    ctx: &TrustContext,
    approved_target: &Path,
    current_uid: u32,
) -> bool {
    let mode = meta.mode;
    if mode & 0o022 == 0 {
        return false;
    }
    // A2 exemption: root-owned sticky directory whose immediate child is owned
    // by root or the current user (e.g. /tmp on Linux).
    if meta.is_dir && is_secure_sticky(meta.uid, mode, below_owner, current_uid) {
        return false;
    }
    // A5 named widening (operator-accepted 2026-09-30): the root-owned,
    // admin-group-writable Apple developer directories, bounded by
    // `is_apple_developer_path` — never a general exemption.
    #[cfg(target_os = "macos")]
    if meta.is_dir
        && meta.uid == 0
        && mode & 0o002 == 0  // NOT other-writable
        && is_apple_developer_path(path, approved_target, ctx.clt_git)
    {
        return false;
    }
    true
}

#[cfg(not(unix))]
fn writable_by_others(
    meta: &PathMetadata,
    _: Option<u32>,
    _: &Path,
    _: &TrustContext,
    _: &Path,
    _: u32,
) -> bool {
    let _ = (meta.mode, meta.is_dir);
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// #2739: report all original and symlink-target ancestors in one pass.
    #[test]
    fn symlinked_tool_reports_every_offending_ancestor() {
        let ctx = TrustContext::bind(&Scope::none()).unwrap();
        let hints = inspect(
            Path::new("/brew/bin/gh"),
            &ctx,
            1000,
            |_| Ok("/brew/Cellar/gh/1/bin/gh".into()),
            |p| {
                Ok(PathMetadata {
                    uid: 0,
                    mode: if matches!(p.to_str().unwrap(), "/brew/bin" | "/brew/Cellar" | "/brew") {
                        0o775
                    } else {
                        0o755
                    },
                    is_dir: p.file_name().is_none_or(|n| n != "gh"),
                    is_symlink: p == Path::new("/brew/bin/gh"),
                })
            },
        )
        .unwrap();
        assert_eq!(
            hints.iter().map(|h| h.path.as_path()).collect::<Vec<_>>(),
            [
                Path::new("/brew/bin"),
                Path::new("/brew"),
                Path::new("/brew/Cellar")
            ]
        );
        assert!(hints.iter().all(|h| h.chmod_arg == "g-w"));
    }

    /// #2739: diagnostics must not invent repairs for a clean path.
    #[test]
    fn clean_path_reports_none() {
        let ctx = TrustContext::bind(&Scope::none()).unwrap();
        assert!(inspect(
            Path::new("/usr/bin/git"),
            &ctx,
            1000,
            |p| Ok(p.to_path_buf()),
            |_| Ok(PathMetadata {
                uid: 0,
                mode: 0o755,
                is_dir: true,
                is_symlink: false
            })
        )
        .unwrap()
        .is_empty());
    }
}
