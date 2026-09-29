//! Private build temporary directories, acquired only after Build admission.
//! Random names and compact partition ordinals are ephemeral locators; the
//! partition map retains each full RawContentId. No durable identity is minted.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn outside_git(path: &Path) -> bool {
    path.ancestors().all(|ancestor| {
        matches!(std::fs::symlink_metadata(ancestor.join(".git")), Err(error) if error.kind() == io::ErrorKind::NotFound)
    })
}

/// A stable proposed path, not an allocated directory. Permission denial has
/// no filesystem effects, and subsequent session grants name the same base.
struct Plan {
    path: PathBuf,
    shared: Arc<LeaseState>,
    // Keep mappings after lease cleanup: remembered session grants must still
    // name the same partition. Full identities are never truncated or hashed again.
    workspaces: parking_lot::Mutex<Vec<content_addressable::RawContentId>>,
}

#[derive(Default)]
struct LeaseState {
    // `cleaning` covers the interval between the last strong reference going
    // away and Base::drop finishing cleanup; a new run waits for that cleanup.
    // A failed cleanup may retain an already-held base. That is the only
    // existing base that a later run may reuse: reopening a matching pathname
    // would give an attacker-owned replacement build authority.
    live: Mutex<Lease>,
    cleaned: Condvar,
}

#[derive(Default)]
struct Lease {
    base: Weak<Base>,
    cleaning: bool,
    retained: Option<RetainedBase>,
    cleanup_failure: Option<String>,
}

/// An owned base which cleanup could not remove. Both identities stay held so
/// a later build can safely reuse this exact directory without treating an
/// arbitrary pre-existing path as ours.
struct RetainedBase {
    directory: Directory,
    parent: Directory,
}

impl Plan {
    fn new(parent: PathBuf) -> Self {
        Self {
            path: parent.join(format!("newt-{}", uuid::Uuid::new_v4().simple())),
            shared: Arc::default(),
            workspaces: parking_lot::Mutex::default(),
        }
    }

    fn partition(&self, identity: &content_addressable::RawContentId) -> PathBuf {
        let mut workspaces = self.workspaces.lock();
        let ordinal = workspaces
            .iter()
            .position(|known| known == identity)
            .unwrap_or_else(|| {
                let ordinal = workspaces.len();
                workspaces.push(*identity);
                ordinal
            });
        self.path.join(format!("w{ordinal:x}"))
    }

    fn acquire(&self) -> io::Result<Arc<Base>> {
        let mut live = self
            .shared
            .live
            .lock()
            .map_err(|_| io::Error::other("build scratch lease poisoned"))?;
        if let Some(error) = &live.cleanup_failure {
            return Err(io::Error::other(error.clone()));
        }
        loop {
            if let Some(base) = live.base.upgrade() {
                drop(live);
                base.directory().check()?;
                return Ok(base);
            }
            if !live.cleaning {
                break;
            }
            live = self
                .shared
                .cleaned
                .wait(live)
                .map_err(|_| io::Error::other("build scratch lease poisoned"))?;
        }
        // A final lease cleanup can record a fail-closed error while this
        // acquisition waits on its Condvar. Check again before considering
        // either retained state or a fresh pathname.
        if let Some(error) = &live.cleanup_failure {
            return Err(io::Error::other(error.clone()));
        }
        if let Some(retained) = live.retained.as_ref() {
            // Do not take this state until both checks pass. A rejected
            // replacement must remain rejected on every later acquisition.
            retained.parent.check()?;
            retained.directory.check()?;
            let RetainedBase { directory, parent } = live
                .retained
                .take()
                .expect("checked retained build scratch must remain present");
            let base = Arc::new(Base {
                directory: Some(directory),
                parent: Some(parent),
                shared: self.shared.clone(),
                operations: Mutex::new(()),
            });
            live.base = Arc::downgrade(&base);
            live.cleaning = true;
            return Ok(base);
        }
        let parent_path = self
            .path
            .parent()
            .ok_or_else(|| refused("build scratch has no parent"))?;
        if !outside_git(parent_path) {
            return Err(refused(
                "managed build scratch must be outside every Git worktree",
            ));
        }
        let parent = Directory::acquire(parent_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = parent.root.open_read(Path::new(""))?.metadata()?.mode();
            if mode & 0o022 != 0 && mode & 0o1000 == 0 {
                return Err(refused(
                    "build scratch parent is writable by others without the sticky bit",
                ));
            }
        }
        let directory = parent.create_owned_child(self.path.file_name().unwrap())?;
        let base = Arc::new(Base {
            directory: Some(directory),
            parent: Some(parent),
            shared: self.shared.clone(),
            operations: Mutex::new(()),
        });
        live.base = Arc::downgrade(&base);
        live.cleaning = true;
        Ok(base)
    }
}

fn plan() -> &'static Plan {
    static PLAN: OnceLock<Plan> = OnceLock::new();
    PLAN.get_or_init(|| {
        let temporary = std::env::temp_dir();
        // A typical macOS user temp path is already ~60 bytes; adding private
        // parents can exhaust sockaddr_un before an ordinary TempDir socket
        // filename. Prefer the short canonical shared parent, whose held-root,
        // sticky-bit, outside-Git and owner-only child checks remain unchanged.
        #[cfg(unix)]
        let candidates = [PathBuf::from("/tmp"), temporary.clone()];
        #[cfg(not(unix))]
        let candidates = [temporary.clone()];
        let parent = candidates
            .into_iter()
            .filter_map(|path| path.canonicalize().ok())
            .find(|path| outside_git(path))
            .unwrap_or(temporary);
        Plan::new(parent)
    })
}

pub(super) fn default_base() -> PathBuf {
    plan().path.clone()
}

pub(crate) fn managed_partition(
    base: &Path,
    identity: &content_addressable::RawContentId,
) -> Option<PathBuf> {
    let plan = plan();
    (base == plan.path).then(|| plan.partition(identity))
}

struct Base {
    directory: Option<Directory>,
    parent: Option<Directory>,
    shared: Arc<LeaseState>,
    operations: Mutex<()>,
}

impl Base {
    fn directory(&self) -> &Directory {
        self.directory.as_ref().unwrap()
    }
}

impl Drop for Base {
    fn drop(&mut self) {
        let Ok(mut live) = self.shared.live.lock() else {
            return;
        };
        let directory = self.directory.take();
        let parent = self.parent.take();
        if let (Some(directory), Some(parent)) = (directory, parent) {
            let path = directory.path.clone();
            // Empty-directory removal only; never sweep unrelated or replaced
            // data. If a previous run left content behind, keep the directory
            // handle instead of later reopening a same-named path.
            match directory.is_nonempty() {
                Ok(false) => match parent.remove_owned_child(directory) {
                    Ok(()) => {}
                    Err((Some(directory), error)) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "managed build scratch cleanup could not remove its empty base; retaining its held base for safe reuse"
                        );
                        live.retained = Some(RetainedBase { directory, parent });
                    }
                    Err((None, error)) => {
                        let message = format!(
                                "managed build scratch cleanup could not retain its base after removal failed: {error}"
                            );
                        tracing::warn!(path = %path.display(), "{message}");
                        live.cleanup_failure = Some(message);
                    }
                },
                Ok(true) => {
                    tracing::warn!(
                        path = %directory.path.display(),
                        "managed build scratch cleanup left owned entries; retaining its held base for safe reuse"
                    );
                    live.retained = Some(RetainedBase { directory, parent });
                }
                Err(error) => {
                    tracing::warn!(
                        path = %directory.path.display(),
                        error = %error,
                        "managed build scratch cleanup could not inspect its base; retaining its held base for safe reuse"
                    );
                    live.retained = Some(RetainedBase { directory, parent });
                }
            }
        }
        live.cleaning = false;
        self.shared.cleaned.notify_all();
    }
}

/// Held filesystem identity, using existing Bridle Unix capabilities and the
/// already-used same-file Windows handle. Windows handles deny delete-sharing
/// for every ancestor except the known owned base, whose own held handle
/// retains that protection while descendants share its DELETE access.
struct Directory {
    path: PathBuf,
    #[cfg(unix)]
    root: agent_bridle_fdguard::GrantedRoot,
    #[cfg(windows)]
    ancestors: Vec<same_file::Handle>,
    /// Path whose held handle owns DELETE access without DELETE sharing. A
    /// descendant must share that existing access when it traverses this
    /// specific ancestor, while retaining no-delete sharing everywhere else.
    #[cfg(windows)]
    delete_owner: Option<PathBuf>,
}

impl Directory {
    fn acquire(path: &Path) -> io::Result<Self> {
        Self::open(path, false, None)
    }

    fn inspect(path: &Path, delete_owner: Option<&Path>) -> io::Result<Self> {
        // A held owned base requests DELETE access without sharing delete. An
        // inspection needs to share that existing access in order to compare
        // the path with the held identity, but it still asks only for read
        // access itself. The held base continues to prevent a third party
        // from obtaining DELETE access.
        Self::open(path, false, delete_owner)
    }

    fn open(path: &Path, owns_deletion: bool, delete_owner: Option<&Path>) -> io::Result<Self> {
        #[cfg(not(windows))]
        let _ = (owns_deletion, delete_owner);
        if path.canonicalize()? != path {
            return Err(refused("build scratch path is not canonical"));
        }
        #[cfg(unix)]
        let root = agent_bridle_fdguard::GrantedRoot::acquire(path)?;
        #[cfg(windows)]
        let ancestors = {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            use windows_sys::Win32::Foundation::GENERIC_READ;
            use windows_sys::Win32::Storage::FileSystem::{
                DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            let mut paths: Vec<_> = path.ancestors().collect();
            paths.reverse();
            let mut handles = Vec::new();
            for ancestor in paths {
                let mut options = std::fs::OpenOptions::new();
                let share_mode = FILE_SHARE_READ
                    | FILE_SHARE_WRITE
                    | if delete_owner == Some(ancestor) {
                        FILE_SHARE_DELETE
                    } else {
                        0
                    };
                options
                    .share_mode(share_mode)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
                if ancestor == path && owns_deletion {
                    // The final handle both prevents a replacement and carries
                    // DELETE access, so empty-directory removal never reopens
                    // this child by pathname.
                    options.access_mode(GENERIC_READ | DELETE);
                } else {
                    options.read(true);
                }
                let file = options.open(ancestor)?;
                let metadata = file.metadata()?;
                if !metadata.is_dir()
                    || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                {
                    return Err(refused(
                        "build scratch ancestor is not an ordinary directory",
                    ));
                }
                handles.push(same_file::Handle::from_file(file)?);
            }
            handles
        };
        #[cfg(not(any(unix, windows)))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed build scratch requires a held directory identity backend",
        ));
        #[cfg(any(unix, windows))]
        Ok(Self {
            path: path.to_owned(),
            #[cfg(unix)]
            root,
            #[cfg(windows)]
            ancestors,
            #[cfg(windows)]
            delete_owner: if owns_deletion {
                Some(path.to_owned())
            } else {
                delete_owner.map(Path::to_owned)
            },
        })
    }

    fn check(&self) -> io::Result<()> {
        // An inspection borrows read access only. A held final directory
        // deliberately denies DELETE sharing, so a second delete-capable
        // handle would fail before the identity comparison.
        #[cfg(windows)]
        let delete_owner = self.delete_owner.as_deref();
        #[cfg(not(windows))]
        let delete_owner = None;
        let current = Self::inspect(&self.path, delete_owner)?;
        #[cfg(unix)]
        let unchanged = self.root.identity() == current.root.identity();
        #[cfg(windows)]
        let unchanged = self.ancestors.last() == current.ancestors.last();
        #[cfg(not(any(unix, windows)))]
        let unchanged = false;
        if !unchanged {
            return Err(refused("build scratch directory was replaced"));
        }
        Ok(())
    }

    /// Inspect only to avoid attempting an unsafe deletion of a nonempty base.
    /// A nonempty or unreadable result retains the held identity; the empty
    /// case still goes through `remove_child` and its identity checks.
    fn is_nonempty(&self) -> io::Result<bool> {
        self.check()?;
        std::fs::read_dir(&self.path)?
            .next()
            .transpose()
            .map(|entry| entry.is_some())
    }

    fn create_child(&self, name: &std::ffi::OsStr, reuse: bool) -> io::Result<Self> {
        self.create_child_with_ownership(name, reuse, false)
    }

    fn create_owned_child(&self, name: &std::ffi::OsStr) -> io::Result<Self> {
        self.create_child_with_ownership(name, false, true)
    }

    fn create_child_with_ownership(
        &self,
        name: &std::ffi::OsStr,
        reuse: bool,
        owns_deletion: bool,
    ) -> io::Result<Self> {
        #[cfg(not(windows))]
        let _ = owns_deletion;
        self.check()?;
        let path = self.path.join(name);
        #[cfg(unix)]
        let created = rustix::fs::mkdirat(
            self.root.as_fd(),
            name,
            rustix::fs::Mode::from_raw_mode(0o700),
        )
        .map_err(io::Error::from);
        #[cfg(not(unix))]
        let created = std::fs::create_dir(&path);
        match created {
            Err(error) if reuse && error.kind() == io::ErrorKind::AlreadyExists => {}
            other => other?,
        }
        #[cfg(unix)]
        {
            let fd = rustix::fs::openat(
                self.root.as_fd(),
                name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let child = Self {
                root: agent_bridle_fdguard::GrantedRoot::from_owned_fd(fd, &path)?,
                path,
            };
            child.check()?;
            Ok(child)
        }
        #[cfg(not(unix))]
        {
            #[cfg(windows)]
            let delete_owner = self.delete_owner.as_deref();
            #[cfg(not(windows))]
            let delete_owner = None;
            Self::open(&path, owns_deletion, delete_owner)
        }
    }

    #[cfg(windows)]
    fn delete_empty_by_identity(&self) -> io::Result<()> {
        use std::mem::size_of;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
        };

        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        let handle = self
            .ancestors
            .last()
            .ok_or_else(|| refused("build scratch child has no identity handle"))?;
        // The final handle was opened with DELETE access and without
        // FILE_SHARE_DELETE. This marks exactly that held directory for
        // deletion; no pathname can be substituted between validation and
        // removal.
        let result = unsafe {
            SetFileInformationByHandle(
                handle.as_file().as_raw_handle() as _,
                FileDispositionInfo,
                &disposition as *const FILE_DISPOSITION_INFO as *const _,
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Removes the exclusive base child. On failure, returns its held identity
    /// so a later lease never reopens a same-named pathname.
    fn remove_owned_child(&self, child: Self) -> Result<(), (Option<Self>, io::Error)> {
        if let Err(error) = self.check() {
            return Err((Some(child), error));
        }
        if let Err(error) = child.check() {
            return Err((Some(child), error));
        }
        #[cfg(test)]
        if take_forced_remove_child_failure() {
            return Err((
                Some(child),
                io::Error::other("forced managed build scratch removal failure"),
            ));
        }
        #[cfg(unix)]
        return match rustix::fs::unlinkat(
            self.root.as_fd(),
            child.path.file_name().unwrap(),
            rustix::fs::AtFlags::REMOVEDIR,
        ) {
            Ok(()) => Ok(()),
            Err(error) => Err((Some(child), io::Error::from(error))),
        };
        #[cfg(windows)]
        return child
            .delete_empty_by_identity()
            .map_err(|error| (Some(child), error));
        #[cfg(not(any(unix, windows)))]
        return match std::fs::remove_dir(&child.path) {
            Ok(()) => Ok(()),
            Err(error) => Err((Some(child), error)),
        };
    }

    fn remove_child(&self, child: Self) -> io::Result<()> {
        self.check()?;
        child.check()?;
        #[cfg(windows)]
        let mut child = child;
        #[cfg(windows)]
        child.ancestors.pop();
        #[cfg(unix)]
        return rustix::fs::unlinkat(
            self.root.as_fd(),
            child.path.file_name().unwrap(),
            rustix::fs::AtFlags::REMOVEDIR,
        )
        .map_err(io::Error::from);
        #[cfg(not(unix))]
        std::fs::remove_dir(&child.path)
    }
}

#[cfg(test)]
std::thread_local! {
    static FORCE_NEXT_REMOVE_CHILD_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn force_next_remove_child_failure() {
    FORCE_NEXT_REMOVE_CHILD_FAILURE.with(|forced| forced.set(true));
}

#[cfg(test)]
fn take_forced_remove_child_failure() -> bool {
    FORCE_NEXT_REMOVE_CHILD_FAILURE.with(|forced| forced.replace(false))
}

/// Keeps all owned parents alive until the existing execution lease ends.
pub(crate) struct ManagedRun {
    run: Option<Directory>,
    partition: Option<Directory>,
    base: Arc<Base>,
}

impl ManagedRun {
    fn create(plan: &Plan, path: &Path) -> io::Result<Self> {
        let base = plan.acquire()?;
        let operations = base
            .operations
            .lock()
            .map_err(|_| io::Error::other("build scratch allocation poisoned"))?;
        let partition = base.directory().create_child(
            path.parent()
                .and_then(Path::file_name)
                .ok_or_else(|| refused("build scratch partition missing"))?,
            true,
        )?;
        let run = partition.create_child(
            path.file_name()
                .ok_or_else(|| refused("build scratch run missing"))?,
            false,
        )?;
        drop(operations);
        Ok(Self {
            run: Some(run),
            partition: Some(partition),
            base,
        })
    }
}

impl Drop for ManagedRun {
    fn drop(&mut self) {
        let Ok(_operations) = self.base.operations.lock() else {
            return;
        };
        if let Some(run) = self.run.take() {
            if run.check().is_ok() {
                #[cfg(windows)]
                let mut run = run;
                #[cfg(windows)]
                run.ancestors.pop();
                let _ = std::fs::remove_dir_all(&run.path);
            }
        }
        if let Some(partition) = self.partition.take() {
            let _ = self.base.directory().remove_child(partition);
        }
    }
}

pub(crate) fn acquire_run(path: &Path) -> io::Result<Option<ManagedRun>> {
    if path.parent().and_then(Path::parent) == Some(plan().path.as_path()) {
        ManagedRun::create(plan(), path).map(Some)
    } else {
        Ok(None)
    }
}

pub(crate) fn validate_grant(
    path: &Path,
    writes: &crate::caveats::Scope<String>,
) -> io::Result<()> {
    if path.parent().and_then(Path::parent) != Some(plan().path.as_path()) {
        return Ok(());
    }
    // Existing grant aliases (notably macOS /tmp) cannot hide authority over
    // the private parent. Prospective, absent roots still get lexical checks;
    // the executor resolves their final filesystem authority at admission.
    let overlaps = crate::caveats::permits_path(writes, &plan().path.to_string_lossy())
        || match writes {
            crate::caveats::Scope::All => true,
            crate::caveats::Scope::Only(roots) => roots.iter().any(|root| {
                Path::new(root)
                    .canonicalize()
                    .is_ok_and(|root| plan().path.starts_with(root))
            }),
        };
    if overlaps {
        return Err(refused(
            "build write authority must not cover the managed scratch parent",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Plan) {
        let temporary = tempfile::tempdir().unwrap();
        let plan = Plan::new(temporary.path().canonicalize().unwrap());
        (temporary, plan)
    }

    #[test]
    fn managed_partitions_preserve_full_identity_without_filesystem_effects() {
        let (_temporary, plan) = fixture();
        let first_id = content_addressable::RawContentId::from_content(b"first workspace");
        let second_id = content_addressable::RawContentId::from_content(b"second workspace");
        let first = plan.partition(&first_id);
        let second = plan.partition(&second_id);
        assert_ne!(first, second);
        assert_eq!(first, plan.partition(&first_id));
        let grant = crate::caveats::Scope::only([first.to_string_lossy().into_owned()]);
        assert!(crate::caveats::permits_path(
            &grant,
            &first.join("run/file").to_string_lossy()
        ));
        assert!(!crate::caveats::permits_path(
            &grant,
            &second.to_string_lossy()
        ));
        assert!(!crate::caveats::permits_path(
            &grant,
            &plan.path.to_string_lossy()
        ));
        assert!(!plan.path.exists(), "planning must precede allocation");
        let run = first.join("run");
        drop(ManagedRun::create(&plan, &run).unwrap());
        assert!(!plan.path.exists());
        assert_eq!(
            first,
            plan.partition(&first_id),
            "remember the granted root"
        );
        assert_eq!(second, plan.partition(&second_id));
        assert_eq!(plan.workspaces.lock().as_slice(), &[first_id, second_id]);
    }

    #[test]
    fn managed_scratch_is_planned_without_effects_and_owned_until_last_run() {
        let (_temporary, plan) = fixture();
        assert!(!plan.path.exists());
        let first = plan.path.join("workspace/first");
        let second = plan.path.join("workspace/second");
        let first_lease = ManagedRun::create(&plan, &first).unwrap();
        let second_lease = ManagedRun::create(&plan, &second).unwrap();
        std::fs::write(first.join("temporary"), "first").unwrap();
        std::fs::write(second.join("temporary"), "second").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&plan.path, first.parent().unwrap(), &first] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
        drop(first_lease);
        assert!(!first.exists());
        assert!(second.join("temporary").is_file());
        drop(second_lease);
        assert!(!plan.path.exists());
        // A remembered Build grant still names the same proposed partition.
        drop(ManagedRun::create(&plan, &first).unwrap());
        assert!(!plan.path.exists());
    }

    #[test]
    fn managed_scratch_rejects_a_git_ancestor_and_preexisting_base() {
        let (temporary, plan) = fixture();
        std::fs::create_dir(temporary.path().join(".git")).unwrap();
        assert!(plan.acquire().is_err());
        assert!(!plan.path.exists());
        std::fs::remove_dir(temporary.path().join(".git")).unwrap();
        std::fs::create_dir(&plan.path).unwrap();
        std::fs::write(plan.path.join("keep"), "unowned").unwrap();
        assert!(plan.acquire().is_err());
        assert_eq!(
            std::fs::read_to_string(plan.path.join("keep")).unwrap(),
            "unowned"
        );
    }

    #[test]
    fn managed_scratch_reuses_a_held_base_after_incomplete_cleanup() {
        let (_temporary, plan) = fixture();
        let orphan_path = plan.path.join("orphan");
        let base = plan.acquire().unwrap();
        // Model a run which could not remove one of its own directories. The
        // base must remain attributable to this lease, not merely to its path.
        let orphan = base
            .directory()
            .create_child(std::ffi::OsStr::new("orphan"), false)
            .unwrap();
        drop(base);
        assert!(orphan_path.is_dir());

        let next = plan.path.join("workspace/next");
        drop(ManagedRun::create(&plan, &next).unwrap());
        assert!(
            orphan_path.is_dir(),
            "recovery must not sweep the leftover owned directory"
        );

        // Once the known leftover is gone, ordinary empty-base cleanup still
        // removes the base rather than retaining it for the whole process.
        let base = plan.acquire().unwrap();
        assert!(base.directory().remove_child(orphan).is_ok());
        drop(base);
        assert!(!plan.path.exists());
    }

    #[test]
    fn managed_scratch_recovers_from_empty_base_cleanup_failure() {
        let (_temporary, plan) = fixture();
        let base = plan.acquire().unwrap();
        force_next_remove_child_failure();
        drop(base);
        assert!(plan.path.is_dir());

        let next = plan.path.join("workspace/next");
        drop(ManagedRun::create(&plan, &next).unwrap());
        assert!(
            !plan.path.exists(),
            "a retained empty base should be removed by its next successful cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retained_managed_scratch_rejects_a_replaced_base() {
        let (_temporary, plan) = fixture();
        let base = plan.acquire().unwrap();
        drop(
            base.directory()
                .create_child(std::ffi::OsStr::new("orphan"), false)
                .unwrap(),
        );
        drop(base);

        // A naïve `create_child(..., true)` recovery would now adopt this
        // replacement. The retained descriptor must continue to reject it.
        let old = plan.path.with_extension("old");
        std::fs::rename(&plan.path, &old).unwrap();
        std::fs::create_dir(&plan.path).unwrap();
        std::fs::write(plan.path.join("keep"), "replacement").unwrap();
        assert!(plan.acquire().is_err());
        assert!(
            plan.acquire().is_err(),
            "failed checks must not discard the held base"
        );
        assert_eq!(
            std::fs::read_to_string(plan.path.join("keep")).unwrap(),
            "replacement"
        );
    }

    #[test]
    fn managed_scratch_concurrent_last_release_and_acquisition_are_serialized() {
        let (_temporary, plan) = fixture();
        let plan = Arc::new(plan);
        std::thread::scope(|scope| {
            for thread in 0..4 {
                let plan = plan.clone();
                scope.spawn(move || {
                    for iteration in 0..20 {
                        let path = plan.path.join(format!("workspace/{thread}-{iteration}"));
                        let lease = ManagedRun::create(&plan, &path).unwrap();
                        std::fs::write(path.join("owned"), "temporary").unwrap();
                        drop(lease);
                    }
                });
            }
        });
        assert!(!plan.path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn managed_scratch_rejects_partition_symlink_and_replaced_root() {
        let (_temporary, plan) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let base = plan.acquire().unwrap();
        std::os::unix::fs::symlink(outside.path(), plan.path.join("workspace")).unwrap();
        assert!(ManagedRun::create(&plan, &plan.path.join("workspace/run")).is_err());
        assert!(!outside.path().join("run").exists());
        std::fs::remove_file(plan.path.join("workspace")).unwrap();
        let old = plan.path.with_extension("moved");
        std::fs::rename(&plan.path, &old).unwrap();
        std::fs::create_dir(&plan.path).unwrap();
        std::fs::write(plan.path.join("keep"), "replacement").unwrap();
        assert!(plan.acquire().is_err());
        drop(base);
        assert_eq!(
            std::fs::read_to_string(plan.path.join("keep")).unwrap(),
            "replacement"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_scratch_rejects_a_symlink_parent_before_creation() {
        let (temporary, _) = fixture();
        let destination = tempfile::tempdir().unwrap();
        let alias = temporary.path().canonicalize().unwrap().join("alias");
        std::os::unix::fs::symlink(destination.path(), &alias).unwrap();
        let plan = Plan::new(alias);
        assert!(plan.acquire().is_err());
        assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    }

    #[test]
    fn managed_scratch_parent_is_never_part_of_the_build_write_grant() {
        use crate::caveats::Scope;
        let path = default_base().join("workspace/run");
        assert!(validate_grant(&path, &Scope::All).is_err());
        assert!(validate_grant(
            &path,
            &Scope::only([default_base()
                .parent()
                .unwrap()
                .to_string_lossy()
                .into_owned()])
        )
        .is_err());
        assert!(validate_grant(
            &path,
            &Scope::only([path.parent().unwrap().to_string_lossy().into_owned()])
        )
        .is_ok());
        #[cfg(unix)]
        {
            let temporary = tempfile::tempdir().unwrap();
            let alias = temporary.path().join("parent-alias");
            std::os::unix::fs::symlink(default_base().parent().unwrap(), &alias).unwrap();
            assert!(
                validate_grant(&path, &Scope::only([alias.to_string_lossy().into_owned()]))
                    .is_err()
            );
        }
    }
}
