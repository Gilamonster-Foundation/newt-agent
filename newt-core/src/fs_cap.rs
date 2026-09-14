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
//! Linux uses `openat2`; macOS uses a descriptor-relative `openat` walk with
//! `O_NOFOLLOW` for every component. macOS conservatively rejects even in-tree
//! symlinks, with ONE opt-in: `open_regular(.., nofollow = false)` follows a
//! final-component link to a relative target re-walked beneath the link's own
//! directory under the same rules (no `..`, no absolute target) — the explicit
//! final-link policy mutation verification needs, and still stricter than
//! Linux's `RESOLVE_BENEATH`. Operator-supplied root aliases (such as /var)
//! remain supported. Other platforms retain their existing consumer fallback.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Component, Path};

use rustix::fs::{mkdirat, open, unlinkat, AtFlags, Mode, OFlags};
#[cfg(target_os = "linux")]
use rustix::fs::{openat2, ResolveFlags};

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
    #[cfg(target_os = "linux")]
    fn resolve(&self, rel: &Path, oflags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        // Containment policy for every resolve: stay beneath the root fd (so `..`,
        // absolute paths, and escaping symlinks are refused), and reject magic
        // links. In-tree symlinks that stay beneath still resolve.
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
        openat2(&self.root, rel, oflags | OFlags::CLOEXEC, mode, resolve).map_err(io::Error::from)
    }

    /// macOS needs O_DIRECTORY and caller-selected nonblocking/create flags
    /// that Bridle's current read/write-only GrantedRoot API cannot express.
    /// Walk normal components through held descriptors and never follow links —
    /// not even a contained final one; `macos_rejects_symlinks_without_
    /// truncating_their_targets` pins that. The one opt-in is
    /// [`open_regular`](Self::open_regular) with `nofollow = false`, which goes
    /// through [`resolve_following_final`](Self::resolve_following_final).
    #[cfg(target_os = "macos")]
    fn resolve(&self, rel: &Path, oflags: OFlags, mode: Mode) -> io::Result<OwnedFd> {
        let (directory, leaf) = Self::walk_beneath(&self.root, rel)?;
        let leaf = leaf.unwrap_or_else(|| ".".into());
        rustix::fs::openat(
            &directory,
            Path::new(&leaf),
            oflags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            mode,
        )
        .map_err(io::Error::from)
    }

    /// [`resolve`](Self::resolve), except that a FINAL-component symlink is
    /// followed — the "explicit final link policy" `open_regular` documents,
    /// which `openat2`'s `RESOLVE_BENEATH` gives Linux for free. The link is
    /// read and its target re-walked from the link's own directory under the
    /// same rules, one hop at a time, so an absolute or `..` target is refused
    /// exactly like a path component would be; intermediate links stay refused.
    /// Bounded, because a link cycle would otherwise spin.
    #[cfg(target_os = "macos")]
    fn resolve_following_final(
        &self,
        rel: &Path,
        oflags: OFlags,
        mode: Mode,
    ) -> io::Result<OwnedFd> {
        use std::os::unix::ffi::OsStrExt as _;
        let mut base = self.root.try_clone()?;
        let mut rel = rel.to_path_buf();
        // ponytail: 8 hops is far more than any real in-tree link chain; a cycle
        // exhausts it and reports ELOOP, which is what the kernel says too.
        for _hop in 0..8 {
            let (directory, leaf) = Self::walk_beneath(&base, &rel)?;
            let leaf = leaf.unwrap_or_else(|| ".".into());
            match rustix::fs::openat(
                &directory,
                Path::new(&leaf),
                oflags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                mode,
            ) {
                Err(rustix::io::Errno::LOOP) => {
                    let target = rustix::fs::readlinkat(&directory, Path::new(&leaf), Vec::new())
                        .map_err(io::Error::from)?;
                    rel = Path::new(std::ffi::OsStr::from_bytes(target.to_bytes())).to_path_buf();
                    base = directory;
                }
                other => return other.map_err(io::Error::from),
            }
        }
        Err(io::Error::from_raw_os_error(libc::ELOOP))
    }

    /// The resolve `open_regular` uses: Linux lets `openat2` honour the
    /// caller's `NOFOLLOW` (or its absence) under `RESOLVE_BENEATH`; macOS
    /// routes the follow case through the explicit final-link walk.
    #[cfg(target_os = "macos")]
    fn resolve_regular(&self, rel: &Path, flags: OFlags, nofollow: bool) -> io::Result<OwnedFd> {
        if nofollow {
            self.resolve(rel, flags, Mode::empty())
        } else {
            self.resolve_following_final(rel, flags, Mode::empty())
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn resolve_regular(&self, rel: &Path, flags: OFlags, _nofollow: bool) -> io::Result<OwnedFd> {
        self.resolve(rel, flags, Mode::empty())
    }

    /// One descriptor-relative walk of `rel`'s parent components beneath `base`,
    /// every step `O_NOFOLLOW`; returns the directory that holds the final
    /// component and that component's name (`None` for `rel` = `.`). `..`, an
    /// absolute path, or a prefix is refused up front — there is nothing beneath
    /// `base` they could name.
    #[cfg(target_os = "macos")]
    fn walk_beneath(
        base: &OwnedFd,
        rel: &Path,
    ) -> io::Result<(OwnedFd, Option<std::ffi::OsString>)> {
        let mut directory = base.try_clone()?;
        let mut names = Vec::new();
        for component in rel.components() {
            match component {
                Component::Normal(name) => names.push(name),
                Component::CurDir => {}
                _ => return Err(io::Error::from_raw_os_error(libc::EXDEV)),
            }
        }
        let Some((leaf, parents)) = names.split_last() else {
            return Ok((directory, None));
        };
        for name in parents {
            directory = rustix::fs::openat(
                &directory,
                Path::new(name),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
        }
        Ok((directory, Some(leaf.to_os_string())))
    }

    /// Open a file for reading, contained beneath the root.
    pub fn open(&self, rel: &Path) -> io::Result<File> {
        Ok(File::from(self.resolve(
            rel,
            OFlags::RDONLY,
            Mode::empty(),
        )?))
    }

    /// Open a regular file without blocking on a FIFO. Diagnostic snapshots
    /// pass `nofollow` to distinguish a final symlink from its target; mutation
    /// verification may follow a contained link, matching the write policy.
    pub fn open_regular(&self, rel: &Path, nofollow: bool) -> io::Result<File> {
        let flags = OFlags::RDONLY
            | OFlags::NONBLOCK
            | if nofollow {
                OFlags::NOFOLLOW
            } else {
                OFlags::empty()
            };
        let file = File::from(self.resolve_regular(rel, flags, nofollow)?);
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not a regular file",
            ));
        }
        Ok(file)
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

    /// Create `rel` and any missing parent directories, each contained beneath
    /// the root. The walk opens (or creates) one component at a time on a fd
    /// resolved *beneath* the previous one, so a symlink or `..` in any component
    /// is refused by the kernel — there is no `mkdir -p` over an un-resolved path.
    /// An existing directory is fine; a non-directory in the path errors.
    pub fn create_dir_all(&self, rel: &Path) -> io::Result<()> {
        // `try_clone` so the walk owns its cursor without consuming `self.root`.
        let mut cur: OwnedFd = self.root.try_clone()?;
        let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        for comp in rel.components() {
            let name = match comp {
                Component::Normal(n) => n,
                Component::CurDir => continue,
                // A beneath-safe relative path has no `..`, root, or prefix.
                _ => return Err(io::Error::from_raw_os_error(libc::EXDEV)),
            };
            let step = Path::new(name);
            let child = match (Self {
                root: cur.try_clone()?,
            })
            .resolve(step, dir_flags, Mode::empty())
            {
                Ok(fd) => fd,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    mkdirat(&cur, step, Mode::from_raw_mode(0o755)).map_err(io::Error::from)?;
                    (Self {
                        root: cur.try_clone()?,
                    })
                    .resolve(step, dir_flags, Mode::empty())?
                }
                Err(e) => return Err(e),
            };
            cur = child;
        }
        Ok(())
    }

    /// Resolve the directory that holds `rel`'s final component (object-bound —
    /// a symlink / `..` in the parent path is refused) and return it with that
    /// final component name. A bare filename resolves against the root itself.
    /// The shared owner of the name-granularity mutation ops ([`unlink`], [`rename`]).
    ///
    /// [`unlink`]: Self::unlink
    /// [`rename`]: Self::rename
    fn parent_dir_and_name(&self, rel: &Path) -> io::Result<(OwnedFd, std::ffi::OsString)> {
        let name = rel
            .file_name()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?
            .to_os_string();
        let parent = rel.parent().unwrap_or(Path::new(""));
        let dir = if parent.as_os_str().is_empty() {
            self.root.try_clone()?
        } else {
            self.resolve(parent, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?
        };
        Ok((dir, name))
    }

    /// Remove a file entry, contained beneath the root. The parent is resolved
    /// object-bound, then the final component is removed with `unlinkat` relative
    /// to the parent fd — so the removal cannot be redirected outside the root,
    /// and a symlink at the final component is removed *as the link* (its target
    /// is not followed).
    pub fn unlink(&self, rel: &Path) -> io::Result<()> {
        let (parent_dir, name) = self.parent_dir_and_name(rel)?;
        unlinkat(&parent_dir, Path::new(&name), AtFlags::empty()).map_err(io::Error::from)
    }

    /// Rename `from` → `to`, both contained beneath the root. Each path's parent
    /// is resolved object-bound, then `renameat` moves the final component between
    /// the parent fds — so neither endpoint can be redirected outside the root by
    /// a symlink / `..` in its path. Used for atomic temp-then-rename writes.
    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from_dir, from_name) = self.parent_dir_and_name(from)?;
        let (to_dir, to_name) = self.parent_dir_and_name(to)?;
        rustix::fs::renameat(
            &from_dir,
            Path::new(&from_name),
            &to_dir,
            Path::new(&to_name),
        )
        .map_err(io::Error::from)
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
