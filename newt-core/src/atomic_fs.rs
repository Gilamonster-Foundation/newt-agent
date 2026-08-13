//! Crash-safe, process-serialized file writes.
//!
//! The lock owner is recorded in the sidecar so a process killed without
//! running `Drop` does not wedge the destination forever.  Writes are staged
//! beside the destination, synced, atomically replaced, and followed by a
//! parent-directory sync so a successful return is durable across a crash.

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 18:32 EDT | Date: 2026-08-12

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LOCK_RETRIES: u32 = 100;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
const LEGACY_LOCK_STALE: Duration = Duration::from_secs(30);
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A durable replacement failure that records whether the destination name
/// was already changed. Callers which publish multi-file invariants must not
/// roll back prerequisites after [`Self::committed`] becomes true.
#[derive(Debug)]
pub struct DurableReplaceError {
    path: PathBuf,
    committed: bool,
    source: std::io::Error,
}

impl DurableReplaceError {
    fn before_commit(path: &Path, source: std::io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            committed: false,
            source,
        }
    }

    /// Build an error for a failure after the destination name changed.
    ///
    /// This is public so transaction adapters and deterministic failpoints can
    /// preserve the same commit-state contract as [`ResolvedPath`].
    pub fn after_commit(path: &Path, source: std::io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            committed: true,
            source,
        }
    }

    /// Whether replacement changed the destination name before failing.
    #[must_use]
    pub fn committed(&self) -> bool {
        self.committed
    }
}

impl std::fmt::Display for DurableReplaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.committed {
            write!(
                formatter,
                "replaced {}, but could not durably sync its parent directory: {}",
                self.path.display(),
                self.source
            )
        } else {
            write!(
                formatter,
                "could not replace {}: {}",
                self.path.display(),
                self.source
            )
        }
    }
}

impl std::error::Error for DurableReplaceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Kernel-held serialization for stale-lock takeover. The file is deliberately
/// persistent: deleting a native-lock inode while another contender has it
/// open would recreate the same generation race this lease prevents. Closing
/// the file releases the OS lock automatically, including after process death.
struct ReclaimLease {
    _file: std::fs::File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LockOwner {
    pid: i64,
    nonce: String,
}

impl LockOwner {
    fn current() -> Self {
        Self {
            pid: i64::from(std::process::id()),
            nonce: unique_suffix(),
        }
    }

    fn encode(&self) -> String {
        format!("{}:{}\n", self.pid, self.nonce)
    }

    fn decode(body: &str) -> Option<Self> {
        let (pid, nonce) = body.trim().split_once(':')?;
        let pid: i64 = pid.parse().ok()?;
        if pid <= 0 {
            return None;
        }
        if nonce.is_empty() {
            return None;
        }
        Some(Self {
            pid,
            nonce: nonce.to_string(),
        })
    }
}

/// Held for the lifetime of a locked critical section.
///
/// Drop removes the sidecar only when it still contains this guard's owner
/// token.  A late drop can therefore never unlink a replacement owner's lock.
#[must_use = "drop the guard to release the lock"]
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    owner: LockOwner,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let ours = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|body| LockOwner::decode(&body))
            .is_some_and(|owner| owner == self.owner);
        if ours {
            let _ = std::fs::remove_file(&self.path);
            let _ = sync_parent(&self.path);
        }
    }
}

/// Acquire the advisory lock at `lock_path`.
///
/// A parseable lock owned by a dead process is reclaimed immediately.  Older
/// lock files written by Newt versions which did not record an owner retain the
/// conservative age-based recovery path.
pub fn acquire_lock(lock_path: &Path) -> anyhow::Result<LockGuard> {
    if let Some(parent) = lock_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let owner = LockOwner::current();
    let encoded = owner.encode();
    let mut denied = false;
    for _ in 0..LOCK_RETRIES {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut file) => {
                if let Err(error) = (|| -> std::io::Result<()> {
                    file.write_all(encoded.as_bytes())?;
                    file.sync_all()
                })() {
                    drop(file);
                    let _ = std::fs::remove_file(lock_path);
                    return Err(error.into());
                }
                return Ok(LockGuard {
                    path: lock_path.to_path_buf(),
                    owner,
                });
            }
            // Windows can report a just-deleted lock as DELETE_PENDING via
            // ERROR_ACCESS_DENIED.  Retry it like ordinary contention.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                denied = error.kind() == std::io::ErrorKind::PermissionDenied;
                if reclaim_lock_once(lock_path)? {
                    continue;
                }
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(error.into()),
        }
    }
    if denied {
        anyhow::bail!(
            "could not acquire {} — permission denied; check the lock directory permissions",
            lock_path.display()
        );
    }
    anyhow::bail!(
        "could not acquire {} — another live process appears to be writing it; try again",
        lock_path.display()
    )
}

/// Attempt one stale-generation takeover while holding a kernel advisory lock
/// on a persistent recovery sidecar. Every reclaimer must hold this lease
/// across both the liveness proof and unlink, so a second reclaimer cannot act
/// on an observation from the previous generation.
fn reclaim_lock_once(lock_path: &Path) -> anyhow::Result<bool> {
    let Some(_lease) = try_reclaim_lease(lock_path)? else {
        return Ok(false);
    };
    #[cfg(test)]
    pause_reclaimer_for_test()?;
    if !reclaimable_lock(lock_path) {
        return Ok(false);
    }
    match std::fs::remove_file(lock_path) {
        Ok(()) => {
            sync_parent(lock_path)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
fn pause_reclaimer_for_test() -> anyhow::Result<()> {
    const READY_ENV: &str = "NEWT_ATOMIC_RECLAIM_READY";
    const RELEASE_ENV: &str = "NEWT_ATOMIC_RECLAIM_RELEASE";

    let Some(ready) = std::env::var_os(READY_ENV).map(PathBuf::from) else {
        return Ok(());
    };
    let release = std::env::var_os(RELEASE_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{RELEASE_ENV} is required with {READY_ENV}"))?;
    std::fs::write(&ready, b"reclaim lease held")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !release.exists() {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for {}", release.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn try_reclaim_lease(lock_path: &Path) -> anyhow::Result<Option<ReclaimLease>> {
    let path = reclaim_lease_path_for(lock_path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    if try_native_exclusive_lock(&file)? {
        Ok(Some(ReclaimLease { _file: file }))
    } else {
        Ok(None)
    }
}

fn reclaim_lease_path_for(lock_path: &Path) -> PathBuf {
    let name = lock_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    lock_path.with_file_name(format!("{name}.reclaim"))
}

#[cfg(unix)]
fn try_native_exclusive_lock(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    loop {
        // SAFETY: `file` owns a valid descriptor for the duration of this call;
        // LOCK_NB makes contention observable without blocking the retry loop.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        return Err(error);
    }
}

#[cfg(windows)]
fn try_native_exclusive_lock(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::LockFile;

    // SAFETY: `file` owns a valid Windows file handle for the call's duration.
    // LockFile is non-blocking and the held handle releases the byte-range lock
    // on Drop/process death.
    let handle = file.as_raw_handle() as HANDLE;
    let result = unsafe { LockFile(handle, 0, 0, 1, 0) };
    if result != 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        return Ok(false);
    }
    Err(error)
}

fn reclaimable_lock(lock_path: &Path) -> bool {
    match std::fs::read_to_string(lock_path)
        .ok()
        .and_then(|body| LockOwner::decode(&body))
    {
        Some(owner) => !crate::store::pid_is_alive(owner.pid),
        None => std::fs::metadata(lock_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > LEGACY_LOCK_STALE),
    }
}

/// A destination resolved exactly once. Lock selection, staging, and commit
/// all use this bound path so a symlink retarget cannot move a transaction to a
/// different file after its lock was acquired.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPath {
    path: PathBuf,
}

impl ResolvedPath {
    /// Resolve `path` once, failing closed for dangling symlinks.
    pub fn resolve(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            path: resolve_stable_path(path)?,
        })
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        lock_path_for(&self.path)
    }

    pub fn stage(&self, bytes: &[u8]) -> anyhow::Result<PathBuf> {
        stage_file_at(&self.path, bytes, None, false)
    }

    pub fn stage_private(&self, bytes: &[u8]) -> anyhow::Result<PathBuf> {
        stage_file_at(&self.path, bytes, None, true)
    }

    pub fn stage_with_permissions(
        &self,
        bytes: &[u8],
        permissions: Option<&std::fs::Permissions>,
        private_if_new: bool,
    ) -> anyhow::Result<PathBuf> {
        stage_file_at(&self.path, bytes, permissions, private_if_new)
    }

    /// Remove sibling staging files left by writers whose owner process is no
    /// longer alive. Call only while holding this destination's canonical lock.
    /// Live, malformed, or PID-reused stages are preserved fail-closed.
    pub fn cleanup_abandoned_stages(&self) -> anyhow::Result<usize> {
        let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        else {
            return Ok(0);
        };
        let file_name = self
            .path
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("{} has no file name", self.path.display()))?
            .to_string_lossy();
        let prefix = format!(".{file_name}.");
        let mut removed = 0;
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(owner_and_nonce) = name
                .strip_prefix(&prefix)
                .and_then(|name| name.strip_suffix(".tmp"))
            else {
                continue;
            };
            let Some(pid) = owner_and_nonce
                .split_once('-')
                .and_then(|(pid, _)| pid.parse::<i64>().ok())
                .filter(|pid| *pid > 0)
            else {
                continue;
            };
            if crate::store::pid_is_alive(pid) {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if removed > 0 {
            sync_parent(&self.path)?;
        }
        Ok(removed)
    }

    pub fn durable_replace(&self, staged: &Path) -> Result<(), DurableReplaceError> {
        self.durable_replace_with_sync(staged, sync_parent_io)
    }

    /// Replace using an injected parent-sync operation while preserving the
    /// before/after-commit error contract. Transaction adapters use this only
    /// for deterministic durability failpoints; ordinary callers use
    /// [`Self::durable_replace`].
    pub fn durable_replace_with_sync(
        &self,
        staged: &Path,
        sync: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> Result<(), DurableReplaceError> {
        replace_file(staged, &self.path)
            .map_err(|source| DurableReplaceError::before_commit(&self.path, source))?;
        sync(&self.path).map_err(|source| DurableReplaceError::after_commit(&self.path, source))
    }

    pub fn durable_create(&self, staged: &Path) -> anyhow::Result<()> {
        create_file_no_replace(staged, &self.path)?;
        sync_parent(&self.path)
    }

    pub fn atomic_write(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let staged = self.stage(bytes)?;
        if let Err(error) = self.durable_replace(&staged) {
            let _ = std::fs::remove_file(&staged);
            return Err(error.into());
        }
        Ok(())
    }

    pub fn atomic_write_private(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let staged = self.stage_private(bytes)?;
        if let Err(error) = self.durable_replace(&staged) {
            let _ = std::fs::remove_file(&staged);
            return Err(error.into());
        }
        Ok(())
    }
}

/// Return a stable absolute identity for a destination.
///
/// Existing symlinks resolve to their target.  For a new file the existing
/// parent is canonicalized and the final component is appended.
pub fn stable_path(path: &Path) -> anyhow::Result<PathBuf> {
    Ok(ResolvedPath::resolve(path)?.path)
}

fn resolve_stable_path(path: &Path) -> anyhow::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Ok(std::fs::canonicalize(path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = match parent {
        Some(parent) => {
            std::fs::create_dir_all(parent)?;
            std::fs::canonicalize(parent)?
        }
        None => std::env::current_dir()?,
    };
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?;
    Ok(parent.join(name))
}

/// The canonical sidecar lock identity for `path`.
pub fn stable_lock_path_for(path: &Path) -> anyhow::Result<PathBuf> {
    Ok(ResolvedPath::resolve(path)?.lock_path())
}

/// Write `bytes` to a unique sibling staging file and sync its contents.
/// Existing destination permissions are copied to the stage.
pub fn stage_file(path: &Path, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    ResolvedPath::resolve(path)?.stage(bytes)
}

/// Write a secret to a unique sibling staging file with mode 0600 on Unix.
pub fn stage_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    ResolvedPath::resolve(path)?.stage_private(bytes)
}

/// Stage a file while preserving explicit permissions.  `private_if_new`
/// creates a previously absent destination as 0600 on Unix.
pub fn stage_file_with_permissions(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
    private_if_new: bool,
) -> anyhow::Result<PathBuf> {
    ResolvedPath::resolve(path)?.stage_with_permissions(bytes, permissions, private_if_new)
}

fn stage_file_at(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
    private_if_new: bool,
) -> anyhow::Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no file name", path.display()))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", unique_suffix()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if private_if_new { 0o600 } else { 0o666 });
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&tmp)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions.clone())?;
        } else if private_if_new {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
        } else if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(tmp)
}

/// Atomically replace `path` with a synced sibling stage, then sync the parent
/// directory so the name update itself is durable.
pub fn durable_replace(staged: &Path, path: &Path) -> anyhow::Result<()> {
    Ok(ResolvedPath::resolve(path)?.durable_replace(staged)?)
}

/// Publish a synced sibling stage only if `path` does not already exist.
/// Used for generated drop-ins whose operator-owned name must never be
/// clobbered by a racing setup process.
pub fn durable_create(staged: &Path, path: &Path) -> anyhow::Result<()> {
    ResolvedPath::resolve(path)?.durable_create(staged)
}

#[cfg(unix)]
fn create_file_no_replace(staged: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::hard_link(staged, destination)
}

#[cfg(windows)]
fn create_file_no_replace(staged: &Path, destination: &Path) -> std::io::Result<()> {
    move_file_ex(staged, destination, false)
}

#[cfg(not(windows))]
fn replace_file(staged: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(staged, destination)
}

#[cfg(windows)]
fn replace_file(staged: &Path, destination: &Path) -> std::io::Result<()> {
    move_file_ex(staged, destination, true)
}

#[cfg(windows)]
fn move_file_ex(staged: &Path, destination: &Path, replace: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers valid for
    // the duration of the call. MOVEFILE_REPLACE_EXISTING provides the Windows
    // replace semantics `rename` cannot portably promise; WRITE_THROUGH is the
    // Windows equivalent of making the directory-entry update durable.
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Write `bytes` to `path` atomically and durably.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    ResolvedPath::resolve(path)?.atomic_write(bytes)
}

/// Write private `bytes` to `path` atomically and durably (0600 on Unix).
pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    ResolvedPath::resolve(path)?.atomic_write_private(bytes)
}

/// Sync the directory entry containing `path`.
pub fn sync_parent(path: &Path) -> anyhow::Result<()> {
    Ok(sync_parent_io(path)?)
}

fn sync_parent_io(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        std::fs::File::open(parent)?.sync_all()?;
    }
    // Windows cannot portably FlushFileBuffers on a directory handle. The
    // durable replacement path above uses MOVEFILE_WRITE_THROUGH instead.
    #[cfg(windows)]
    let _ = parent;
    Ok(())
}

/// The conventional sidecar lock path for `path`: `<path>.lock`.
#[must_use]
pub fn lock_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.lock"))
}

/// Collision-resistant process-local suffix suitable for staged/versioned
/// file names.  It contains no credentials or host-specific data.
#[must_use]
pub fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_replaces_and_leaves_no_tmp() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f.toml");
        atomic_write(&path, b"one").unwrap();
        atomic_write(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    #[ignore = "real filesystem cleanup acceptance; run in mcp-import-real workflow"]
    #[serial_test::serial(real_fs)]
    fn abandoned_stage_cleanup_removes_only_dead_owner_for_destination() {
        let dir = TempDir::new().unwrap();
        let destination = ResolvedPath::resolve(&dir.path().join("config.toml")).unwrap();
        let dead = dir.path().join(".config.toml.4294967295-dead-0.tmp");
        let live = dir
            .path()
            .join(format!(".config.toml.{}-live-0.tmp", std::process::id()));
        let unrelated = dir.path().join(".mcp.toml.4294967295-dead-0.tmp");
        for path in [&dead, &live, &unrelated] {
            std::fs::write(path, b"stage").unwrap();
        }

        let _guard = acquire_lock(&destination.lock_path()).unwrap();
        assert_eq!(destination.cleanup_abandoned_stages().unwrap(), 1);
        assert!(!dead.exists());
        assert!(live.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn a_lock_excludes_a_second_holder_until_dropped() {
        let dir = TempDir::new().unwrap();
        let lock = lock_path_for(&dir.path().join("f.toml"));
        let guard = acquire_lock(&lock).unwrap();
        assert!(lock.exists());
        drop(guard);
        assert!(!lock.exists());
        let _again = acquire_lock(&lock).unwrap();
    }

    /// Real-filesystem grounding for immediate abandoned-owner recovery.
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn dead_owner_is_reclaimed_without_waiting_for_age_threshold() {
        let dir = TempDir::new().unwrap();
        let lock = lock_path_for(&dir.path().join("f.toml"));
        // u32::MAX cannot be a live Unix pid and OpenProcess rejects it on
        // Windows; keep the fixture positive so it never acquires process-group
        // semantics (`kill(0/-1, 0)`).
        std::fs::write(&lock, "4294967295:abandoned\n").unwrap();
        let _guard = acquire_lock(&lock).unwrap();
        let owner = LockOwner::decode(&std::fs::read_to_string(&lock).unwrap()).unwrap();
        assert_eq!(owner.pid, i64::from(std::process::id()));
    }

    /// Real-filesystem grounding for canonical lock identity selection.
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn aliases_share_one_stable_lock_identity() {
        let dir = TempDir::new().unwrap();
        let destination = dir.path().join("config.toml");
        std::fs::write(&destination, "x").unwrap();
        let relative = dir.path().join(".").join("config.toml");
        assert_eq!(
            stable_lock_path_for(&destination).unwrap(),
            stable_lock_path_for(&relative).unwrap()
        );
    }

    #[cfg(unix)]
    /// Real-filesystem grounding for private replacement permissions.
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn private_replacement_tightens_existing_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("token.age");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write_private(&path, b"secret").unwrap();

        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    /// Real-filesystem grounding for fail-closed dangling-symlink handling.
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn dangling_symlink_is_rejected_instead_of_replaced() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let target = dir.path().join("missing-target");
        let link = dir.path().join("config.toml");
        symlink(&target, &link).unwrap();

        let error = atomic_write(&link, b"body").unwrap_err();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!target.exists());
        assert!(
            error.to_string().contains("No such file") || error.to_string().contains("not find")
        );
    }

    #[cfg(unix)]
    /// Real-filesystem grounding for the transaction's bound destination.
    #[ignore = "real-resource: weekly/release tier; retargets a filesystem symlink"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn resolved_destination_cannot_be_retargeted_mid_transaction() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let first = dir.path().join("first.toml");
        let second = dir.path().join("second.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&first, b"first-old").unwrap();
        std::fs::write(&second, b"second-old").unwrap();
        symlink(&first, &link).unwrap();

        let destination = ResolvedPath::resolve(&link).unwrap();
        let _guard = acquire_lock(&destination.lock_path()).unwrap();
        let staged = destination.stage(b"first-new").unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();

        destination.durable_replace(&staged).unwrap();

        assert_eq!(std::fs::read(&first).unwrap(), b"first-new");
        assert_eq!(std::fs::read(&second).unwrap(), b"second-old");
        assert_eq!(
            std::fs::canonicalize(&link).unwrap(),
            std::fs::canonicalize(second).unwrap()
        );
    }

    /// Real-filesystem grounding for platform replacement semantics.
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn replacement_overwrites_an_existing_destination() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"new");
    }

    /// Real-filesystem grounding for the commit-state error contract used by
    /// setup transactions: the replacement is visible when parent sync fails.
    #[ignore = "real-resource: weekly/release tier; touches the filesystem"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn sync_failure_after_replacement_reports_committed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let destination = ResolvedPath::resolve(&path).unwrap();
        let staged = destination.stage(b"new").unwrap();

        let error = destination
            .durable_replace_with_sync(&staged, |_| {
                Err(std::io::Error::other("injected parent sync failure"))
            })
            .unwrap_err();

        assert!(error.committed());
        assert_eq!(std::fs::read(path).unwrap(), b"new");
        assert!(error.to_string().contains("could not durably sync"));
    }

    /// Real-process grounding for the owner-aware stale-lock contract. The
    /// child is killed (so `LockGuard::drop` cannot run); the parent must then
    /// reclaim its lock immediately on both Unix and Windows.
    #[ignore = "real-resource: weekly/release tier; spawns and kills a process"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn killed_process_lock_is_reclaimed() {
        const CHILD_ENV: &str = "NEWT_ATOMIC_LOCK_CHILD";
        const LOCK_ENV: &str = "NEWT_ATOMIC_LOCK_PATH";
        const READY_ENV: &str = "NEWT_ATOMIC_LOCK_READY";

        if std::env::var_os(CHILD_ENV).is_some() {
            let lock = PathBuf::from(std::env::var_os(LOCK_ENV).expect("child lock path"));
            let ready = PathBuf::from(std::env::var_os(READY_ENV).expect("child ready path"));
            let _guard = acquire_lock(&lock).expect("child acquires lock");
            std::fs::write(ready, b"ready").expect("child publishes readiness");
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }

        let dir = TempDir::new().unwrap();
        let lock = lock_path_for(&dir.path().join("config.toml"));
        let ready = dir.path().join("ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("atomic_fs::tests::killed_process_lock_is_reclaimed")
            .arg("--ignored")
            .arg("--nocapture")
            .env(CHILD_ENV, "1")
            .env(LOCK_ENV, &lock)
            .env(READY_ENV, &ready)
            .spawn()
            .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !ready.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lock-holder child did not become ready");
        }
        child.kill().unwrap();
        let exit_status = child.wait().unwrap();

        let started = std::time::Instant::now();
        let _guard = acquire_lock(&lock).expect("dead owner's lock is reclaimable");
        assert!(
            started.elapsed() < LEGACY_LOCK_STALE,
            "owner-aware recovery must not wait for the legacy age threshold"
        );
        // Keep `Child` (and therefore its Windows process handle) alive through
        // reclamation. An exited Windows process remains openable while this
        // handle exists, so this specifically guards against treating a
        // successful `OpenProcess` as proof of liveness.
        assert_eq!(child.try_wait().unwrap(), Some(exit_status));
    }

    /// Real-process grounding for serialized stale-lock takeover. The first
    /// reclaimer is paused while it holds the recovery lease, guaranteeing the
    /// second contender races the same abandoned generation. Both contenders
    /// must subsequently enter the protected section without overlapping.
    #[ignore = "real-resource: weekly/release tier; spawns and kills processes"]
    #[serial_test::serial(real_fs)]
    #[test]
    fn two_reclaimers_cannot_delete_a_new_lock_generation() {
        const ROLE_ENV: &str = "NEWT_ATOMIC_RACE_ROLE";
        const LOCK_ENV: &str = "NEWT_ATOMIC_RACE_LOCK";
        const READY_ENV: &str = "NEWT_ATOMIC_RACE_READY";
        const STARTED_ENV: &str = "NEWT_ATOMIC_RACE_STARTED";
        const CRITICAL_ENV: &str = "NEWT_ATOMIC_RACE_CRITICAL";

        if let Some(role) = std::env::var_os(ROLE_ENV) {
            let lock = PathBuf::from(std::env::var_os(LOCK_ENV).expect("child lock path"));
            let ready = PathBuf::from(std::env::var_os(READY_ENV).expect("child ready path"));
            if role == "holder" {
                let _guard = acquire_lock(&lock).expect("holder acquires lock");
                std::fs::write(ready, b"holder ready").expect("holder publishes readiness");
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }

            let started =
                PathBuf::from(std::env::var_os(STARTED_ENV).expect("reclaimer started path"));
            let critical = PathBuf::from(
                std::env::var_os(CRITICAL_ENV).expect("reclaimer critical-section path"),
            );
            std::fs::write(started, role.as_encoded_bytes())
                .expect("reclaimer publishes readiness");
            let guard = acquire_lock(&lock).expect("reclaimer acquires lock");
            let marker = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&critical)
                .expect("reclaimers never overlap their protected sections");
            std::thread::sleep(Duration::from_millis(250));
            drop(marker);
            std::fs::remove_file(&critical).expect("remove critical-section marker");
            drop(guard);
            std::fs::write(ready, role.as_encoded_bytes()).expect("reclaimer publishes completion");
            return;
        }

        fn wait_for(path: &Path, description: &str) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !path.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(path.exists(), "timed out waiting for {description}");
        }

        fn wait_for_child(child: &mut std::process::Child, description: &str) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().expect("poll child") {
                    assert!(status.success(), "{description} failed with {status}");
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("timed out waiting for {description}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let dir = TempDir::new().unwrap();
        let lock = lock_path_for(&dir.path().join("config.toml"));
        let holder_ready = dir.path().join("holder-ready");
        let first_started = dir.path().join("first-started");
        let second_started = dir.path().join("second-started");
        let first_done = dir.path().join("first-done");
        let second_done = dir.path().join("second-done");
        let reclaimer_ready = dir.path().join("reclaimer-lease-held");
        let reclaimer_release = dir.path().join("reclaimer-release");
        let critical = dir.path().join("critical");
        let executable = std::env::current_exe().unwrap();
        let test_name = "atomic_fs::tests::two_reclaimers_cannot_delete_a_new_lock_generation";

        let mut holder = std::process::Command::new(&executable)
            .arg("--exact")
            .arg(test_name)
            .arg("--ignored")
            .arg("--nocapture")
            .env(ROLE_ENV, "holder")
            .env(LOCK_ENV, &lock)
            .env(READY_ENV, &holder_ready)
            .spawn()
            .unwrap();
        wait_for(&holder_ready, "holder readiness");
        holder.kill().unwrap();
        holder.wait().unwrap();

        let mut first = std::process::Command::new(&executable)
            .arg("--exact")
            .arg(test_name)
            .arg("--ignored")
            .arg("--nocapture")
            .env(ROLE_ENV, "first")
            .env(LOCK_ENV, &lock)
            .env(READY_ENV, &first_done)
            .env(STARTED_ENV, &first_started)
            .env(CRITICAL_ENV, &critical)
            .env("NEWT_ATOMIC_RECLAIM_READY", &reclaimer_ready)
            .env("NEWT_ATOMIC_RECLAIM_RELEASE", &reclaimer_release)
            .spawn()
            .unwrap();
        wait_for(&first_started, "first reclaimer start");
        wait_for(&reclaimer_ready, "first reclaimer lease");

        let mut second = std::process::Command::new(&executable)
            .arg("--exact")
            .arg(test_name)
            .arg("--ignored")
            .arg("--nocapture")
            .env(ROLE_ENV, "second")
            .env(LOCK_ENV, &lock)
            .env(READY_ENV, &second_done)
            .env(STARTED_ENV, &second_started)
            .env(CRITICAL_ENV, &critical)
            .spawn()
            .unwrap();
        wait_for(&second_started, "second reclaimer start");
        std::fs::write(&reclaimer_release, b"release").unwrap();

        wait_for_child(&mut first, "first reclaimer");
        wait_for_child(&mut second, "second reclaimer");
        assert!(first_done.exists());
        assert!(second_done.exists());
        assert!(!critical.exists());
    }
}
