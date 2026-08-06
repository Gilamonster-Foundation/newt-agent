//! Object-bound workspace filesystem capability (step-52.1).
//!
//! # Why this exists
//!
//! Filesystem authorization in newt has historically been *pathname*-bound: a
//! predicate decides "is this string inside the workspace?" (`tui_permits_path`,
//! `is_workspace_contained`, `is_safe_worktree_path`) and then a *separate*
//! `std::fs` call opens the path. Two structural flaws follow from that split:
//!
//! 1. **TOCTOU.** The name is checked, then re-resolved at open time; a rename or
//!    symlink swap between the two makes the checked name and the opened object
//!    differ.
//! 2. **Symlink escape.** A lexical check sees `ws/link/secret` as "inside `ws`";
//!    the kernel, following `link -> /etc`, opens `/etc/secret`. `#522` /
//!    `fs-canonical-containment` names this the known residual.
//!
//! [`WorkspaceDir`] removes the split. It owns an `O_DIRECTORY` file descriptor
//! for the workspace root and resolves every relative path *through that fd* with
//! `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`. Authorization is then bound
//! to the **object the kernel actually opened**, atomically: a path that would
//! leave the root — via `..`, an absolute component, or a symlink (under the
//! workspace or not) whose target is outside it — is refused *by the kernel at
//! resolve time*, in the same syscall that opens it. There is no separate name to
//! check and no window to swap.
//!
//! `RESOLVE_BENEATH` rejects any resolution that would ascend above the root fd
//! (so `..`, absolute paths, and absolute/escaping symlinks cannot leave it);
//! `RESOLVE_NO_MAGICLINKS` rejects `/proc`-style magic links. In-tree symlinks
//! that stay beneath the root are still permitted — the fence is *containment*,
//! not a blanket symlink ban.
//!
//! # Scope (step-52.1)
//!
//! This slice lands the capability and proves the containment property (see
//! `tests/fs_cap_object_bound.rs`). Reading ([`open`](WorkspaceDir::open)),
//! writing ([`create`](WorkspaceDir::create)), and directory traversal
//! ([`open_dir`](WorkspaceDir::open_dir)) are object-bound here. Rewiring the
//! existing file tool arms and the write primitives (`newt-core` `tools.rs`,
//! `newt-tools` `patch.rs`) onto it — and the matching flip of the residual
//! `tui_permits_path_symlink_escape_is_the_known_residual` test — is step-52.2 /
//! step-52.3. Mutating-name operations (`unlinkat` / `mkdirat`) need the
//! open-parent-then-operate pattern to stay beneath-safe and land with the
//! write-arm rewire that consumes them.
//!
//! `openat2` is Linux-only, so this module is `#[cfg(target_os = "linux")]`; the
//! cross-platform fallback (and the fail-closed-for-untrusted policy on kernels
//! without `openat2`, invariant #9) is applied where consumers wire it in.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{open, openat2, Mode, OFlags, ResolveFlags};

/// A capability handle to a workspace root directory. Every method resolves its
/// relative path argument *beneath* the held root fd; a path that would escape is
/// an error, never an open of an object outside the root.
///
/// The handle *is* the authority: holding a `WorkspaceDir` grants access to that
/// subtree and no more, and a subtree handle from [`open_dir`](Self::open_dir)
/// can only narrow — never widen — that authority.
#[derive(Debug)]
pub struct WorkspaceDir {
    /// `O_DIRECTORY` fd for the workspace root. The private field is the
    /// capability: a `WorkspaceDir` can exist only for a directory that was
    /// actually opened, and every resolve is anchored to *this* fd.
    root: OwnedFd,
}

impl WorkspaceDir {
    /// Open `path` as the workspace root, returning a capability anchored to it.
    ///
    /// `path` is resolved with the caller's ambient authority — it is the
    /// operator-supplied root, not a model-supplied path. Every *subsequent*
    /// access goes through the returned handle and is contained beneath it.
    pub fn open_root(path: &Path) -> io::Result<Self> {
        let root = open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        Ok(Self { root })
    }

    /// The single choke point: resolve `rel` beneath the root fd and return the
    /// opened fd, or an error if resolution would escape. Every public method
    /// flows through here, so the containment property has one owner.
    fn resolve(&self, rel: &Path, oflags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        // Containment policy for every resolve: stay beneath the root fd (so `..`,
        // absolute paths, and escaping symlinks are refused), and reject magic
        // links. In-tree symlinks that stay beneath still resolve.
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
        openat2(&self.root, rel, oflags | OFlags::CLOEXEC, mode, resolve).map_err(io::Error::from)
    }

    /// Open a file for reading, contained beneath the root.
    pub fn open(&self, rel: &Path) -> io::Result<File> {
        Ok(File::from(self.resolve(
            rel,
            OFlags::RDONLY,
            Mode::empty(),
        )?))
    }

    /// Create (or truncate) a file for writing, contained beneath the root.
    ///
    /// The final component is created *inside* the resolved-beneath path or the
    /// whole call fails — there is no `workspace.join(rel)` that could land the
    /// write elsewhere, and no separate containment check to skip.
    pub fn create(&self, rel: &Path) -> io::Result<File> {
        Ok(File::from(self.resolve(
            rel,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC,
            Mode::from_raw_mode(0o644),
        )?))
    }

    /// Open a subdirectory as its own contained [`WorkspaceDir`]. Traversal stays
    /// beneath the original root; the returned handle cannot reach outside it.
    pub fn open_dir(&self, rel: &Path) -> io::Result<Self> {
        Ok(Self {
            root: self.resolve(rel, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?,
        })
    }

    /// List the entry names of a subdirectory, contained beneath the root. The
    /// directory is resolved object-bound — a symlink-escape directory is refused
    /// by the kernel — and its entries are read straight off the returned fd, so
    /// there is no second path to re-resolve. `.` and `..` are filtered; the
    /// order is filesystem order (the caller sorts). Names only, matching the
    /// `list_dir` tool's output.
    pub fn read_dir(&self, rel: &Path) -> io::Result<Vec<std::ffi::OsString>> {
        use std::os::unix::ffi::OsStringExt;
        let dirfd = self.resolve(rel, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
        let dir = rustix::fs::Dir::read_from(&dirfd).map_err(io::Error::from)?;
        let mut names = Vec::new();
        for entry in dir {
            let entry = entry.map_err(io::Error::from)?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
        }
        Ok(names)
    }
}
