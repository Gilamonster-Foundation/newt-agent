//! New-ref absence includes Git's entire D/F namespace, not just one name.
use super::*;

/// Git's global packed-ref lock stabilizes packed names until loose publication.
/// Leaf `.lock` + mkdir/rename continue to arbitrate ordinary loose Git writers.
/// Fail promptly on contention rather than waiting with locks in reverse order.
pub(super) struct NewRefNamespace<'a> {
    identity: &'a BoundGitIdentity,
    _lock: std::fs::File,
}

impl<'a> NewRefNamespace<'a> {
    pub(super) fn lock(identity: &'a BoundGitIdentity, branch: &str) -> Result<Self, String> {
        // Refuse known conflicts before ANY directory/lock-file creation.
        check(identity, branch)?;
        let guard = Self {
            identity,
            _lock: identity
                .common_workspace
                .create_new(Path::new("packed-refs.lock"))
                .map_err(|e| format!("refused: packed-refs is locked by another writer ({e})"))?,
        };
        // A pack transaction may have won between the first check and lock.
        check(identity, branch)?;
        Ok(guard)
    }
}

impl Drop for NewRefNamespace<'_> {
    fn drop(&mut self) {
        let _ = self
            .identity
            .common_workspace
            .unlink(Path::new("packed-refs.lock"));
    }
}

pub(super) fn check(identity: &BoundGitIdentity, branch: &str) -> Result<(), String> {
    if read_ref_natively(identity, branch)?.is_some() {
        return Err(format!("refused: refs/heads/{branch} already exists"));
    }
    let name = format!("refs/heads/{branch}");
    // A loose descendant is already caught above: a directory is not a ref.
    // Ancestors may be directories, but must never be files (even invalid refs).
    let mut ancestor = Path::new(&name).parent();
    while let Some(path) = ancestor.filter(|p| *p != Path::new("refs/heads")) {
        match identity.common_root.open_read(path) {
            Ok(file) => {
                if !file.metadata().map_err(|e| e.to_string())?.is_dir() {
                    return Err(format!("refused: {} blocks {name}", path.display()));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("refused: cannot inspect {} ({e})", path.display())),
        }
        ancestor = path.parent();
    }
    let packed = read_packed_refs_natively(identity)?;
    for line in packed.lines().filter(|l| !l.starts_with(['#', '^'])) {
        let (_, existing) = line
            .split_once(' ')
            .ok_or("refused: invalid packed-refs entry")?;
        if existing == name
            || existing
                .strip_prefix(&name)
                .is_some_and(|s| s.starts_with('/'))
            || name
                .strip_prefix(existing)
                .is_some_and(|s| s.starts_with('/'))
        {
            return Err(format!("refused: packed {existing} conflicts with {name}"));
        }
    }
    Ok(())
}
