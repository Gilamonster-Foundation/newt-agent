//! Read Git metadata inside a filesystem read grant, without a `git` process.
//! A subprocess may read anything, so [`metadata_git`](super::metadata_git)
//! needs unrestricted reads; these check every path against the grant instead.
//! Moved down from newt-git's embedded tool so the native preflight can use
//! them too (#3117).
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Why a scoped metadata read failed. The embedded Git tool converts these into
/// its own `GitError` variants of the same names and text.
#[derive(Debug)]
pub enum ScopedGitReadError {
    Engine(grit_lib::error::Error),
    Io(io::Error),
    Unsupported(&'static str),
    Refused(String),
}

impl fmt::Display for ScopedGitReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(error) => write!(f, "git: {error}"),
            Self::Io(error) => write!(f, "io: {error}"),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Refused(why) => f.write_str(why),
        }
    }
}

impl From<io::Error> for ScopedGitReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<grit_lib::error::Error> for ScopedGitReadError {
    fn from(error: grit_lib::error::Error) -> Self {
        Self::Engine(error)
    }
}

pub fn checked_git_read(
    path: &Path,
    scope: &crate::Scope<String>,
) -> Result<PathBuf, ScopedGitReadError> {
    let canonical = path.canonicalize()?;
    if !crate::caveats::permits_path(scope, &canonical.to_string_lossy()) {
        return Err(ScopedGitReadError::Refused(format!(
            "capability denied: fs_read for Git path {}",
            path.display()
        )));
    }
    let metadata = canonical.metadata()?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(ScopedGitReadError::Unsupported(
            "non-regular Git input cannot be read",
        ));
    }
    Ok(canonical)
}

pub fn checked_optional_git_read(
    path: &Path,
    scope: &crate::Scope<String>,
) -> Result<bool, ScopedGitReadError> {
    match path.symlink_metadata() {
        Ok(_) => {
            checked_git_read(path, scope)?;
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Discover only inside authorized ancestors, then authorize both the resolved
/// gitfile target and the shared worktree metadata before any engine read.
pub fn scoped_repository_paths(
    root: &Path,
    scope: &crate::Scope<String>,
) -> Result<(PathBuf, PathBuf, Option<PathBuf>), ScopedGitReadError> {
    let mut current = checked_git_read(root, scope)?;
    let (git_dir, worktree) = loop {
        let dot_git = current.join(".git");
        if checked_optional_git_read(&dot_git, scope)? {
            break (
                checked_git_read(&grit_lib::repo::resolve_dot_git(&dot_git)?, scope)?,
                Some(current),
            );
        }
        if current.join("HEAD").exists() && current.join("objects").is_dir() {
            break (current, None);
        }
        current = checked_git_read(
            current
                .parent()
                .ok_or(ScopedGitReadError::Unsupported("not a Git repository"))?,
            scope,
        )?;
    };
    if !checked_optional_git_read(&git_dir.join("HEAD"), scope)? {
        return Err(ScopedGitReadError::Unsupported("Git directory has no HEAD"));
    }
    let common_dir = if checked_optional_git_read(&git_dir.join("commondir"), scope)? {
        checked_git_read(
            &grit_lib::refs::common_dir(&git_dir)
                .ok_or(ScopedGitReadError::Unsupported("invalid Git commondir"))?,
            scope,
        )?
    } else {
        git_dir.clone()
    };
    Ok((git_dir, common_dir, worktree))
}

/// The branch `refs/remotes/origin/HEAD` names for the repository at `cwd`,
/// read as one file inside `scope`. `None` when there is no such ref, the same
/// best-effort fallback to `main`/`master` as [`super::is_default_branch`].
pub fn remote_default_branch(
    cwd: &Path,
    scope: &crate::Scope<String>,
) -> Result<Option<String>, ScopedGitReadError> {
    let (_, common_dir, _) = scoped_repository_paths(cwd, scope)?;
    let head = common_dir.join("refs/remotes/origin/HEAD");
    if !checked_optional_git_read(&head, scope)? {
        return Ok(None);
    }
    Ok(match grit_lib::refs::read_ref_file(&head)? {
        grit_lib::refs::Ref::Symbolic(target) => target
            .strip_prefix("refs/remotes/origin/")
            .map(str::to_owned),
        grit_lib::refs::Ref::Direct(_) => None,
    })
}
