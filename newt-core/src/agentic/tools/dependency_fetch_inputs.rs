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
        let directory = tempfile::Builder::new()
            .prefix("newt-fetch-")
            .tempdir()
            .map_err(|e| format!("cannot create private fetch inputs: {e}"))?;
        let private = directory.path().canonicalize().map_err(|e| e.to_string())?;
        let source = root.canonicalize().map_err(|e| e.to_string())?;
        if private.starts_with(&source) {
            return Err("private fetch inputs must be outside the mutable workspace; configure an external temporary directory".into());
        }
        let copy = private.join("workspace");
        std::fs::create_dir(&copy).map_err(|e| e.to_string())?;
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
        if self.screened.iter().any(|(path, expected)| {
            read(path).map(|bytes| RawContentId::from_content(bytes.as_bytes())) != *expected
        }) {
            return Err("Cargo.lock or repository .cargo/config changed during dependency-fetch approval; retry the build against the new inputs".into());
        }
        Ok(())
    }
}

/// Reuse descriptor-relative WorkspaceDir reads: source renames or escaping
/// links cannot make the harness copy a file outside its workspace authority.
/// No hardlinks are shared with the source. Limits bound hostile/huge trees.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn copy_workspace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use crate::fs_cap::WorkspaceDir;
    use std::io::{Error, Read};
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
                    std::fs::write(destination.join(&name), bytes)?;
                }
                // Symlinks are never materialized. If Cargo needs one, the
                // isolated fetch fails closed rather than following live input.
                Err(e) if e.raw_os_error() == Some(libc::ELOOP) => continue,
                Err(_) => {
                    let child = dir.open_dir(relative)?;
                    let target = destination.join(&name);
                    std::fs::create_dir(&target)?;
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
