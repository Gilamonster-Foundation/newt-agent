//! Repair authority is bound before consent; pathnames are then display only.
use super::*;
use agent_bridle_fdguard::{GrantedRoot, RootIdentity};
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjectKind {
    Directory,
    File,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObjectIdentity {
    object: RootIdentity,
    kind: ObjectKind,
    owner: u32,
    group: u32,
}

#[derive(Clone, Copy)]
pub(super) struct Snapshot {
    identity: ObjectIdentity,
    mode: u32,
}

impl From<Metadata> for Snapshot {
    fn from(meta: Metadata) -> Self {
        Self {
            identity: ObjectIdentity {
                object: RootIdentity {
                    device: meta.dev(),
                    inode: meta.ino(),
                },
                kind: if meta.is_dir() {
                    ObjectKind::Directory
                } else if meta.is_file() {
                    ObjectKind::File
                } else {
                    ObjectKind::Other
                },
                owner: meta.uid(),
                group: meta.gid(),
            },
            mode: meta.mode(),
        }
    }
}

pub(super) trait FileSystem {
    type Parent;
    type Object;
    fn named_metadata(&self, path: &Path) -> io::Result<Snapshot>;
    fn hold_parent(&self, path: &Path) -> io::Result<Self::Parent>;
    fn open(&self, parent: &Self::Parent, leaf: &Path) -> io::Result<Self::Object>;
    fn metadata(&self, object: &Self::Object) -> io::Result<Snapshot>;
    fn set_mode(&self, object: &Self::Object, mode: u32) -> io::Result<()>;
}

pub(super) struct Host;
impl FileSystem for Host {
    type Parent = GrantedRoot;
    type Object = File;
    fn named_metadata(&self, path: &Path) -> io::Result<Snapshot> {
        std::fs::symlink_metadata(path).map(Into::into)
    }
    fn hold_parent(&self, path: &Path) -> io::Result<GrantedRoot> {
        GrantedRoot::acquire(&std::fs::canonicalize(path)?)
    }
    fn open(&self, parent: &GrantedRoot, leaf: &Path) -> io::Result<File> {
        parent.open_read(leaf)
    }
    fn metadata(&self, object: &File) -> io::Result<Snapshot> {
        object.metadata().map(Into::into)
    }
    fn set_mode(&self, object: &File, mode: u32) -> io::Result<()> {
        object.set_permissions(std::fs::Permissions::from_mode(mode))
    }
}

pub(super) struct BoundRepair<F: FileSystem = Host> {
    parent: F::Parent,
    leaf: PathBuf,
    // Keep the diagnosed inode alive until repair/drop; dev+ino alone can be reused.
    _pinned: F::Object,
    identity: ObjectIdentity,
}

fn changed() -> io::Error {
    io::Error::other("diagnosed object changed; permission repair refused; re-run `newt doctor`")
}

impl<F: FileSystem> BoundRepair<F> {
    pub(super) fn capture(hint: &TrustHint, fs: &F) -> io::Result<Self> {
        let named = fs.named_metadata(&hint.path)?;
        if named.mode != hint.mode
            || !matches!(
                named.identity.kind,
                ObjectKind::Directory | ObjectKind::File
            )
        {
            return Err(changed());
        }
        let parent = fs.hold_parent(hint.path.parent().unwrap_or(&hint.path))?;
        let leaf = hint.path.file_name().map(PathBuf::from).unwrap_or_default();
        let pinned = fs.open(&parent, &leaf)?;
        let held = fs.metadata(&pinned)?;
        if held.identity != named.identity || held.mode != named.mode {
            return Err(changed());
        }
        Ok(Self {
            parent,
            leaf,
            _pinned: pinned,
            identity: held.identity,
        })
    }

    pub(super) fn apply(&self, hint: &TrustHint, fs: &F) -> io::Result<()> {
        let file = fs.open(&self.parent, &self.leaf)?;
        let current = fs.metadata(&file)?;
        if current.identity != self.identity {
            return Err(changed());
        }
        let remove = match hint.chmod_arg {
            "g-w" => 0o020,
            "o-w" => 0o002,
            _ => 0o022,
        };
        fs.set_mode(&file, current.mode & !remove)
    }
}

#[cfg(test)]
mod tests;
