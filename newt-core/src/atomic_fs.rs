//! Crash-safe, lock-serialized file writes.
//!
//! Two primitives, factored out of the pattern `NoteStore` and
//! `ConversationStore` already use, so authority-bearing registries do not
//! open-code a fourth copy:
//!
//! * [`acquire_lock`] — a best-effort advisory lock via `create_new` on a
//!   sidecar file, with stale-lock takeover so a crashed process cannot wedge
//!   the file forever. Held across a whole read-modify-write, it serializes
//!   concurrent writers so one cannot silently clobber the other's update.
//! * [`atomic_write`] — write a sibling `.tmp`, **fsync it**, then `rename` over
//!   the target. The rename is atomic and same-directory, so a crash mid-write
//!   leaves the previous file intact (never a truncated one) and a reader only
//!   ever sees a complete file. The fsync is the piece `NoteStore::save` omits;
//!   without it a crash after rename but before the data reaches disk can lose
//!   the write.
//!
//! Neither is a cross-machine lock; both target the single-host, multi-process
//! case (two `newt` invocations racing the same `~/.newt` file).

use std::path::{Path, PathBuf};
use std::time::Duration;

const LOCK_RETRIES: u32 = 100;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
const LOCK_STALE: Duration = Duration::from_secs(30);

/// Held for the lifetime of a locked critical section; removes the sidecar lock
/// file on drop (including on panic/early-return via `?`).
#[must_use = "drop the guard to release the lock"]
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Acquire the advisory lock at `lock_path` (a sidecar like `peers.toml.lock`).
/// Blocks up to a bounded number of retries; takes over a lock older than the
/// stale threshold (a crashed writer). Returns a guard that releases on drop.
pub fn acquire_lock(lock_path: &Path) -> anyhow::Result<LockGuard> {
    // Track whether the *last* contended iteration was a permission denial, so
    // the bail below can tell a settling-lock retry apart from a genuinely
    // unwritable directory (see the message branch).
    let mut denied = false;
    for _ in 0..LOCK_RETRIES {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(_) => {
                return Ok(LockGuard {
                    path: lock_path.to_path_buf(),
                })
            }
            // `AlreadyExists` is the POSIX "another holder has it" signal. On
            // Windows a lock whose previous holder just dropped its guard can be
            // in DELETE_PENDING state, and `create_new` then returns
            // `PermissionDenied` (ERROR_ACCESS_DENIED / os error 5) instead of
            // `AlreadyExists` — the deletion is still settling. Treat both as the
            // same transient "contended, retry" condition: the exclusive
            // `create_new` above is still the ONLY thing that ever grants the
            // lock, so retrying here can never hand it to two writers, and a
            // genuine persistent permission fault just exhausts the bounded
            // retries and falls through to the bail below. Without this, a burst
            // of concurrent `newt dock approve` on Windows fatally errors
            // mid-write (seen as a flaky `concurrent_approvals_do_not_lost_update`
            // on the Windows CI lane).
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                denied = e.kind() == std::io::ErrorKind::PermissionDenied;
                let stale = std::fs::metadata(lock_path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > LOCK_STALE);
                // Only skip the backoff if we actually reclaimed a stale lock;
                // if the removal itself fails (e.g. an unwritable dir), fall
                // through to the sleep so we can't busy-spin the whole budget.
                if stale && std::fs::remove_file(lock_path).is_ok() {
                    continue;
                }
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(e) => return Err(e.into()),
        }
    }
    // A persistent `PermissionDenied` is not contention — retrying won't help,
    // so say so rather than misdiagnosing an unwritable lock directory as a
    // racing writer.
    if denied {
        anyhow::bail!(
            "could not acquire {} — permission denied (os error 5 / EACCES). If a prior writer \
             just released it this is transient; a persistent denial means the lock directory \
             is not writable — check its permissions/ownership",
            lock_path.display()
        );
    }
    anyhow::bail!(
        "could not acquire {} — another process appears to be writing it; try again",
        lock_path.display()
    )
}

/// Write `bytes` to `path` atomically: a same-directory `.tmp`, fsynced, then
/// renamed over `path`. A crash never leaves `path` truncated.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?
        .to_string_lossy()
        .into_owned();
    let tmp = path.with_file_name(format!("{file_name}.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?; // durability: the bytes are on disk before the rename
    drop(f);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The conventional sidecar lock path for `path`: `<path>.lock`.
#[must_use]
pub fn lock_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_replaces_and_leaves_no_tmp() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("f.toml");
        atomic_write(&p, b"one").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        atomic_write(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert!(!dir.path().join("f.toml.tmp").exists(), "no stray tmp");
    }

    #[test]
    fn a_lock_excludes_a_second_holder_until_dropped() {
        let dir = TempDir::new().unwrap();
        let lock = lock_path_for(&dir.path().join("f.toml"));
        let g = acquire_lock(&lock).unwrap();
        // A second acquire cannot immediately succeed (retries then would fail,
        // but we only assert the lock file exists while held).
        assert!(lock.exists());
        drop(g);
        assert!(!lock.exists(), "drop releases the lock");
        // Re-acquire now succeeds.
        let _g2 = acquire_lock(&lock).unwrap();
    }
    // Stale-lock takeover is the verbatim NoteStore mechanism (notes.rs), which
    // is covered there; std has no portable set-mtime to exercise it here.
}
