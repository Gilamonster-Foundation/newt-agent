//! Private input copy for a governed dependency fetch (#2731).
//!
//! The source workspace may change while permission is pending. Revalidation
//! detects that change, but is not the security boundary: Cargo reads the copy,
//! outside the source workspace's write fence, throughout the operation.

use std::path::{Path, PathBuf};

use content_addressable::RawContentId;

pub(super) struct FetchInputs {
    _directory: tempfile::TempDir,
    pub root: PathBuf,
    pub cwd: PathBuf,
    screened: Vec<(PathBuf, Option<RawContentId>)>,
}

impl FetchInputs {
    pub fn capture(root: &Path, cwd: &Path) -> Result<Self, String> {
        let relative = cwd
            .strip_prefix(root)
            .map_err(|_| "fetch directory is outside the workspace")?;
        let directory =
            private_tempdir().map_err(|e| format!("cannot create private fetch inputs: {e}"))?;
        let private = directory.path().canonicalize().map_err(|e| e.to_string())?;
        let source = root.canonicalize().map_err(|e| e.to_string())?;
        if private.starts_with(&source) {
            return Err("private fetch inputs must be outside the mutable workspace; configure an external temporary directory".into());
        }
        let copy = private.join("workspace");
        create_private_dir(&copy).map_err(|e| e.to_string())?;
        copy_workspace(&source, &copy).map_err(|e| format!("cannot pin fetch inputs: {e}"))?;
        let copied_cwd = copy.join(relative);
        let read = |path: &Path| std::fs::read_to_string(path).ok();
        if let Some(reason) = super::fetch_refusal(&copy, &copied_cwd, read) {
            return Err(reason.into());
        }
        let mut screened = Vec::new();
        let mut found_lock = false;
        for dir in copied_cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(&copy))
        {
            for name in [".cargo/config", ".cargo/config.toml", "Cargo.lock"] {
                if name == "Cargo.lock" && found_lock {
                    continue;
                }
                let copied = dir.join(name);
                let bytes = match std::fs::read(&copied) {
                    Ok(bytes) => Some(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(format!("cannot read pinned fetch input: {e}")),
                };
                if name == "Cargo.lock" && bytes.is_some() {
                    found_lock = true;
                }
                screened.push((
                    root.join(copied.strip_prefix(&copy).expect("copy-contained input")),
                    bytes.map(|bytes| RawContentId::from_content(&bytes)),
                ));
            }
        }
        Ok(Self {
            _directory: directory,
            root: copy,
            cwd: copied_cwd,
            screened,
        })
    }

    /// This is an early, explicit refusal for prompt-time edits. Safety after
    /// this check comes from using the private copy, never reopening source.
    pub fn verify_source(&self, read: impl Fn(&Path) -> Option<String>) -> Result<(), String> {
        verify_private_tree(self._directory.path())
            .map_err(|e| format!("fetch inputs are not owner-only: {e}"))?;
        if self.screened.iter().any(|(path, expected)| {
            read(path).map(|bytes| RawContentId::from_content(bytes.as_bytes())) != *expected
        }) {
            return Err("Cargo.lock or repository .cargo/config changed during dependency-fetch approval; retry the build against the new inputs".into());
        }
        Ok(())
    }
}

/// Permissions are supplied to creation itself: a permissive umask cannot
/// open a group-readable/writable window before a later chmod.
fn private_tempdir() -> std::io::Result<tempfile::TempDir> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tempfile::Builder::new()
            .prefix("newt-fetch-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
    }
    #[cfg(not(unix))]
    {
        Err(std::io::Error::other(
            "owner-only fetch copies require Unix permissions",
        ))
    }
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::other(
            "owner-only fetch copies require Unix permissions",
        ))
    }
}

/// Called before approval and again immediately before returning the approved
/// request for execution. Never follow a substituted link while checking modes.
fn verify_private_tree(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.permissions().mode() & 0o077 != 0 || !(metadata.is_dir() || metadata.is_file())
        {
            return Err(std::io::Error::other(
                "copy permits non-owner access or contains a non-regular entry",
            ));
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                verify_private_tree(&entry?.path())?;
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(std::io::Error::other(
            "owner-only fetch copies require Unix permissions",
        ))
    }
}

/// Reuse descriptor-relative WorkspaceDir reads: source renames or escaping
/// links cannot make the harness copy a file outside its workspace authority.
/// No hardlinks are shared with the source. Limits bound hostile/huge trees.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn copy_workspace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use crate::fs_cap::WorkspaceDir;
    use std::io::{Error, Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    fn copy(
        dir: WorkspaceDir,
        destination: &Path,
        remaining: &mut (usize, usize),
        depth: usize,
    ) -> std::io::Result<()> {
        if depth > 64 {
            return Err(Error::other("fetch snapshot exceeds 64 directory levels"));
        }
        for name in dir.read_dir(Path::new("."))? {
            if name == ".git" || name == "target" {
                continue;
            }
            remaining.0 = remaining
                .0
                .checked_sub(1)
                .ok_or_else(|| Error::other("fetch snapshot exceeds 20000 entries"))?;
            let relative = Path::new(&name);
            match dir.open_regular(relative, true) {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    file.take(remaining.1 as u64 + 1).read_to_end(&mut bytes)?;
                    remaining.1 = remaining
                        .1
                        .checked_sub(bytes.len())
                        .ok_or_else(|| Error::other("fetch snapshot exceeds 128 MiB"))?;
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(destination.join(&name))?
                        .write_all(&bytes)?;
                }
                // Symlinks are never materialized. If Cargo needs one, the
                // isolated fetch fails closed rather than following live input.
                Err(e) if e.raw_os_error() == Some(libc::ELOOP) => continue,
                Err(_) => {
                    let child = dir.open_dir(relative)?;
                    let target = destination.join(&name);
                    create_private_dir(&target)?;
                    copy(child, &target, remaining, depth + 1)?;
                }
            }
        }
        Ok(())
    }
    copy(
        WorkspaceDir::open_root(source)?,
        destination,
        &mut (20_000, 128 * 1024 * 1024),
        0,
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn copy_workspace(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "this platform cannot bind dependency-fetch inputs with descriptor-relative reads",
    ))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    /// #2731 round 3: a permissive process umask must never expose the
    /// outer directory, copied directories or copied files to another user.
    /// Change umask only in a fresh subprocess, never in the parallel runner.
    #[test]
    fn copy_is_owner_only_under_permissive_umask() {
        use std::os::unix::fs::PermissionsExt;
        const CHILD: &str = "NEWT_FETCH_UMASK_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new("/bin/sh")
                .args(["-c", "umask 002; exec \"$@\"", "fetch-umask-test"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", "agentic::tools::dependency_fetch::inputs::tests::copy_is_owner_only_under_permissive_umask", "--nocapture"])
                .env(CHILD, "1")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let source = tempfile::tempdir().unwrap();
        let root = source.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Cargo.lock"),
            "[[package]]\nname='app'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn value() {}\n").unwrap();
        let inputs = FetchInputs::capture(&root, &root).unwrap();
        for path in [
            inputs._directory.path().to_path_buf(),
            inputs.root.clone(),
            inputs.root.join("Cargo.lock"),
            inputs.root.join("src"),
            inputs.root.join("src/lib.rs"),
        ] {
            let mode = std::fs::symlink_metadata(&path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode & 0o077,
                0,
                "copy exposed at {}: mode {mode:o}",
                path.display()
            );
        }
    }

    /// #2731 round 3: permission widening anywhere in the retained copy
    /// must fail the same verification called after approval and before launch.
    #[test]
    fn changed_copy_permissions_refuse_before_use() {
        use std::os::unix::fs::PermissionsExt;
        let source = tempfile::tempdir().unwrap();
        let root = source.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Cargo.lock"),
            "[[package]]\nname='app'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let inputs = FetchInputs::capture(&root, &root).unwrap();
        let read = |p: &Path| std::fs::read_to_string(p).ok();
        inputs.verify_source(read).unwrap();
        for path in [
            inputs._directory.path().to_path_buf(),
            inputs.root.clone(),
            inputs.root.join("src"),
            inputs.root.join("src/lib.rs"),
        ] {
            let original = std::fs::metadata(&path).unwrap().permissions();
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(original.mode() | 0o060),
            )
            .unwrap();
            assert!(inputs
                .verify_source(read)
                .is_err_and(|e| e.contains("not owner-only")));
            std::fs::set_permissions(&path, original).unwrap();
        }
        inputs.verify_source(read).unwrap();
    }

    /// #2731: a private copy, not a second source-name check, binds input bytes
    /// through use. Source edits after verification cannot affect the copy.
    #[test]
    fn source_edits_after_verification_cannot_change_pinned_inputs() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().canonicalize().unwrap();
        let lock = "version = 4\n[[package]]\nname = 'app'\nversion = '0.1.0'\n";
        std::fs::write(root.join("Cargo.lock"), lock).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'app'\nversion = '0.1.0'\n",
        )
        .unwrap();
        let inputs = FetchInputs::capture(&root, &root).unwrap();
        inputs
            .verify_source(|p| std::fs::read_to_string(p).ok())
            .unwrap();
        std::fs::write(root.join("Cargo.lock"), "changed").unwrap();
        std::fs::write(root.join("Cargo.toml"), "changed").unwrap();
        std::fs::create_dir(root.join(".cargo")).unwrap();
        std::fs::write(
            root.join(".cargo/config.toml"),
            "[source.crates-io]\nreplace-with='other'\n",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(inputs.root.join("Cargo.lock")).unwrap(),
            lock
        );
        assert!(!std::fs::read_to_string(inputs.root.join("Cargo.toml"))
            .unwrap()
            .contains("changed"));
        assert!(!inputs.root.join(".cargo/config.toml").exists());
        assert!(inputs
            .verify_source(|p| std::fs::read_to_string(p).ok())
            .is_err());
        let pinned = inputs.root.clone();
        drop(inputs);
        assert!(
            !pinned.exists(),
            "private inputs must be removed when the fetch ends"
        );
    }

    /// #2731: a symlink cannot redirect snapshot reads into external files.
    #[test]
    fn snapshot_does_not_follow_workspace_symlinks() {
        use std::os::unix::fs::symlink;
        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = source.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Cargo.lock"),
            "[[package]]\nname='app'\nversion='0.1.0'\n",
        )
        .unwrap();
        std::fs::write(outside.path().join("secret"), "outside").unwrap();
        symlink(outside.path(), root.join("escape")).unwrap();
        let inputs = FetchInputs::capture(&root, &root).unwrap();
        assert!(!inputs.root.join("escape").exists());
    }
}
