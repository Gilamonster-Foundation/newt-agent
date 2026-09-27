//! Private build temporary directories, acquired only after Build admission.
//! The random name is an ephemeral locator; workspace partition identity stays
//! the existing RawContentId in confined_exec. No durable identity is minted.

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
}

#[derive(Default)]
struct LeaseState {
    // The flag covers the interval between the last strong reference going
    // away and Base::drop finishing cleanup; a new run waits for that cleanup.
    live: Mutex<(Weak<Base>, bool)>,
    cleaned: Condvar,
}

impl Plan {
    fn new(parent: PathBuf) -> Self {
        Self {
            path: parent.join(format!("newt-build-{}", uuid::Uuid::new_v4())),
            shared: Arc::default(),
        }
    }

    fn acquire(&self) -> io::Result<Arc<Base>> {
        let mut live = self
            .shared
            .live
            .lock()
            .map_err(|_| io::Error::other("build scratch lease poisoned"))?;
        loop {
            if let Some(base) = live.0.upgrade() {
                drop(live);
                base.directory().check()?;
                return Ok(base);
            }
            if !live.1 {
                break;
            }
            live = self
                .shared
                .cleaned
                .wait(live)
                .map_err(|_| io::Error::other("build scratch lease poisoned"))?;
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
        let directory = parent.create_child(self.path.file_name().unwrap(), false)?;
        let base = Arc::new(Base {
            directory: Some(directory),
            parent,
            shared: self.shared.clone(),
            operations: Mutex::new(()),
        });
        *live = (Arc::downgrade(&base), true);
        Ok(base)
    }
}

fn plan() -> &'static Plan {
    static PLAN: OnceLock<Plan> = OnceLock::new();
    PLAN.get_or_init(|| {
        let temporary = std::env::temp_dir();
        let candidates = vec![temporary.clone()];
        #[cfg(unix)]
        let candidates = candidates.into_iter().chain([PathBuf::from("/tmp")]);
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

struct Base {
    directory: Option<Directory>,
    parent: Directory,
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
        // Empty-directory removal only; never sweep unrelated or replaced data.
        if let Some(directory) = self.directory.take() {
            let _ = self.parent.remove_child(directory);
        }
        live.1 = false;
        self.shared.cleaned.notify_all();
    }
}

/// Held filesystem identity, using existing Bridle Unix capabilities and the
/// already-used same-file Windows handle. Windows handles deny delete-sharing
/// for every ancestor, preventing path relocation while creating a child.
struct Directory {
    path: PathBuf,
    #[cfg(unix)]
    root: agent_bridle_fdguard::GrantedRoot,
    #[cfg(windows)]
    ancestors: Vec<same_file::Handle>,
}

impl Directory {
    fn acquire(path: &Path) -> io::Result<Self> {
        if path.canonicalize()? != path {
            return Err(refused("build scratch path is not canonical"));
        }
        #[cfg(unix)]
        let root = agent_bridle_fdguard::GrantedRoot::acquire(path)?;
        #[cfg(windows)]
        let ancestors = {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            let mut paths: Vec<_> = path.ancestors().collect();
            paths.reverse();
            let mut handles = Vec::new();
            for ancestor in paths {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(ancestor)?;
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
        })
    }

    fn check(&self) -> io::Result<()> {
        let current = Self::acquire(&self.path)?;
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

    fn create_child(&self, name: &std::ffi::OsStr, reuse: bool) -> io::Result<Self> {
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
        Self::acquire(&path)
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
