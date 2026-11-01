//! Object-bound read grants. File grants retain only the file descriptor;
//! acquiring its parent for a no-follow open never grants the parent tree.
use agent_bridle_fdguard::{GrantedRoot, RootIdentity};
use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug)]
pub(super) enum HeldRoot {
    Directory(GrantedRoot),
    File { file: File, identity: RootIdentity },
}

impl HeldRoot {
    pub fn acquire(path: &Path) -> io::Result<Self> {
        match GrantedRoot::acquire(path) {
            Ok(directory) => Ok(Self::Directory(directory)),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                let parent = GrantedRoot::acquire(path.parent().ok_or(error)?)?;
                let name = path
                    .file_name()
                    .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
                let file = crate::fs_cap::WorkspaceDir::from_granted_root(&parent)?
                    .open_regular(Path::new(name), true)?;
                let metadata = file.metadata()?;
                Ok(Self::File {
                    file,
                    identity: RootIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    },
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn directory(&self) -> Option<&GrantedRoot> {
        match self {
            Self::Directory(root) => Some(root),
            Self::File { .. } => None,
        }
    }

    pub fn identity(&self) -> RootIdentity {
        match self {
            Self::Directory(root) => root.identity(),
            Self::File { identity, .. } => *identity,
        }
    }

    #[cfg(target_os = "linux")]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Directory(root) => root.as_fd(),
            Self::File { file, .. } => file.as_fd(),
        }
    }

    pub fn open_read(&self, relative: &Path) -> io::Result<File> {
        match self {
            Self::Directory(root) => root.open_read(relative),
            Self::File { file, .. }
                if relative.as_os_str().is_empty() || relative == Path::new(".") =>
            {
                file.try_clone()
            }
            Self::File { .. } => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
        }
    }
}

/// Duplicated file-root descriptors share a cursor. Every planning consumer
/// reads positionally, including .git pointer files, so neither repeated nor
/// concurrent reads advance another reader's cursor.
pub(super) fn read_from_start(file: &File) -> io::Result<String> {
    use std::os::unix::fs::FileExt;
    let mut content = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = match file.read_at(&mut buffer, content.len() as u64) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        content.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8(content).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    /// Supporting file grants must not turn symlinks or special files into roots.
    #[test]
    fn acquisition_refuses_symlinks_and_special_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let file = root.join("config");
        std::fs::write(&file, "fixture").unwrap();
        assert!(matches!(
            HeldRoot::acquire(&file).unwrap(),
            HeldRoot::File { .. }
        ));
        let link = root.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(HeldRoot::acquire(&link).is_err());
        let socket_path = root.join("socket");
        let _socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        assert!(HeldRoot::acquire(&socket_path).is_err());
    }
}
