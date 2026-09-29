//! Race-free bounded opens beneath a granted root (agent-bridle#351/#354,
//! ADR 0026 slice 2).
//!
//! ## The invariant
//!
//! **INV-BENEATH: once authority over a directory has been resolved into a
//! [`GrantedRoot`], no later filesystem-namespace mutation — renaming or
//! replacing an ancestor, planting a symlink anywhere on the old pathname,
//! deleting and recreating the root — can redirect an open performed through
//! that handle to an object outside the subtree rooted at the directory object
//! it holds.** The handle *is* the authority; the pathname it was resolved
//! from is provenance (audit text, display, diagnostics) and is never used to
//! re-derive authority.
//!
//! ## Why a pathname must not become authority twice (agent-bridle#354)
//!
//! A leash check (`check_path_*`) canonicalizes and tests membership — correct,
//! but *advisory about the future*. The first cut of this module then re-opened
//! the canonical root **by pathname** with only final-component `O_NOFOLLOW`
//! protection. `O_NOFOLLOW` guards the last component alone: an *ancestor* of
//! the canonical root swapped for a symlink (or renamed and replaced) between
//! authority resolution and that re-open redirects the whole walk, and the
//! subsequent `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` then operates
//! perfectly beneath the WRONG root. The fix is architectural, not a flag:
//!
//! ```text
//! authority resolution ──▶ GrantedRoot::acquire (the LAST pathname use)
//!                              │  OwnedFd + RootIdentity
//!                              ▼
//!            every bounded open is relative to that descriptor
//! ```
//!
//! [`GrantedRoot::acquire`] is the single point where a pathname is converted
//! into a descriptor, and even that conversion refuses every symlink component
//! (`RESOLVE_NO_SYMLINKS` on Linux; a per-component `O_NOFOLLOW | O_DIRECTORY`
//! walk elsewhere), so a swapped ancestor at acquisition time is refused rather
//! than followed. After acquisition, opens go through the held descriptor and
//! the namespace can mutate freely without redirecting them.
//!
//! ## Per-open mechanism
//!
//! - **Linux**: one `openat2(2)` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`
//!   relative to the held root descriptor — the kernel refuses `..`-escapes,
//!   absolute jumps, and every symlink (magic `/proc/self/fd` reopen links
//!   included; `RESOLVE_NO_SYMLINKS` subsumes `RESOLVE_NO_MAGICLINKS`)
//!   atomically. `ENOSYS` (pre-5.6 kernel) fails closed: the repo's Linux
//!   floor has the syscall. `EAGAIN` (openat2's detected-race result) retries
//!   bounded, then fails closed.
//! - **Other Unix (macOS)**: no `openat2`, so an equivalent walk: each
//!   component is opened `openat(dirfd, comp, O_NOFOLLOW | O_DIRECTORY)`
//!   relative to the previous handle, and the final component with the
//!   caller's flags plus `O_NOFOLLOW`. `..`/absolute components are refused up
//!   front, so resolution can never leave the root; any symlink terminates it.
//!
//! Both legs are **Conservative** in the projection vocabulary: an in-root
//! symlink is refused too (honest callers pass *canonical* `rel` paths, which
//! contain none). Callers run in the parent, not a fork child — no
//! async-signal-safety constraint here; the `unsafe` is plain FFI,
//! encapsulated so `agent-bridle-core` stays `forbid(unsafe_code)`.
//!
//! ## What a grant means at a mount boundary (decided, agent-bridle#354)
//!
//! A grant on a root means the **pathname subtree beneath that root in the
//! mount namespace, *including* any filesystems mounted beneath it** — not
//! "the filesystem subtree up to the first mount transition". Rationale: the
//! grantor granted a *place in the namespace* ("you may write beneath
//! `/work`"); a scratch tmpfs or an overlay checkout legitimately mounted
//! inside the grant is inside what was granted, and refusing it would make the
//! projection lie about the grantor's intent. `RESOLVE_NO_XDEV` is therefore
//! deliberately **not** set. The residual this accepts: an attacker who can
//! *plant a new mount* beneath the root (bind-mounting `/etc` into the grant)
//! could redirect a bounded open — but mounting requires `CAP_SYS_ADMIN` in
//! the mount namespace, an authority the confined agent must never hold; an
//! attacker with mount authority has already escaped the model this seam
//! enforces. Recorded as a residual, not silently assumed.
//!
//! ## Refusal classification
//!
//! [`is_resolution_refusal`] answers "did the *resolution itself* refuse?" so
//! callers report an authority denial rather than an I/O error. The two legs
//! spell the same refusal differently — **measured natively on both**, Linux
//! on the Linux test host and the walk leg on macOS 26.5.2 / xnu-12377 arm64:
//!
//! | case                           | Linux (`openat2`) | walk (`O_NOFOLLOW`, Darwin) |
//! |--------------------------------|-------------------|-----------------------------|
//! | intermediate symlink → dir     | `ELOOP`           | `ENOTDIR`                   |
//! | intermediate symlink → file    | `ELOOP`           | `ENOTDIR`                   |
//! | intermediate symlink, dangling | `ELOOP`           | `ENOTDIR`                   |
//! | intermediate, honestly non-dir | `ENOTDIR`         | `ENOTDIR`                   |
//! | intermediate, genuinely absent | `ENOENT`          | `ENOENT`                    |
//! | final-component symlink        | `ELOOP`           | `ELOOP`                     |
//!
//! The walk leg answers `ENOTDIR` for an intermediate symlink because that
//! component is opened `O_DIRECTORY | O_NOFOLLOW` and the not-a-directory
//! check fires before `O_NOFOLLOW`'s `ELOOP`. The open still fails, so the
//! symlink is never followed and the bound holds — but this makes `ENOTDIR`
//! **load-bearing rather than cosmetic**: it is *the* primary refusal spelling
//! for an intermediate symlink on Darwin, so without it in the classifier the
//! walk leg's most common denial would be misreported as an ordinary I/O error
//! on exactly the platform that leg serves.
//!
//! **Epistemic limit — errno proves THAT, not WHY.** On the walk leg `ENOTDIR`
//! is a single spelling covering at least four distinct situations
//! (symlink→dir refused, symlink→file refused, dangling symlink refused, and an
//! honestly non-directory component). Asserting `ENOTDIR` therefore proves the
//! open was **refused**; it does **not** prove a *symlink* was refused. That is
//! strictly stronger than a boolean — it closes the refusal-vs-ordinary-error
//! gap — but a test that wants to prove specifically that a symlink was not
//! followed cannot get that from errno: it must compare the resulting
//! descriptor's **identity** (`fstat` dev/ino, or `F_GETPATH`) against the
//! intended target. Note also that a *dangling* intermediate symlink answers
//! `ENOTDIR`, not `ENOENT` — never model "missing component" with a dangling
//! link in a fixture, or it will fail on Darwin for an entirely correct reason.
//!
//! `EMLINK` stays in the accepted set for other BSDs that spell `O_NOFOLLOW`
//! that way, but it was **never observed on Darwin** — every final-component
//! symlink there answered `ELOOP`. A passing test is not evidence that the
//! `EMLINK` arm is exercised.
//!
//! ## Errno tells you *that*; identity tells you *what*
//!
//! The point above generalizes, and it is the same idea [`RootIdentity`] rests
//! on. An errno or an exit status reports that an operation was **refused or
//! permitted**. It cannot report **which object you got** — and for every
//! property of the form "resolution did/did not land outside the grant", the
//! object is the claim. So:
//!
//! - a **refusal** may be asserted from errno, because a refused open yields
//!   no descriptor at all, and "no object" is the whole claim;
//! - a **success** must be asserted from identity — `fstat` `(dev, ino)`, or
//!   `F_GETPATH` on Darwin — because "it opened" is true of the intended
//!   object and of an attacker's substitute alike.
//!
//! [`RootIdentity`] applies this to an authority record (the grant names an
//! object, not a path); the walk leg's positive-control test applies the same
//! oracle to a test. Both exist because a name is not an identity.

use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

/// The stable identity of the directory object a [`GrantedRoot`] holds —
/// `(st_dev, st_ino)` observed **from the held descriptor**, never from a
/// pathname.
///
/// This is the CID-grade anchor for authority-bearing records (grants,
/// admitted fences, audit events): a pathname re-resolving to a different
/// object cannot silently change it, because it names the object, not the
/// name. Two identities are equal iff they name the same filesystem object.
///
/// Caveats, stated so they cannot be over-claimed:
/// - The identity names the *object*, not its contents; it is not a content
///   hash. Feed [`RootIdentity::to_bytes`] into a fence CID as the *namespace
///   anchor* component, alongside whatever content addressing the record
///   already carries.
/// - `(dev, ino)` is pinned only while some descriptor holds the object open.
///   A [`GrantedRoot`] holds one, so comparisons against a *live* handle are
///   sound; comparing two identities when neither object is pinned admits
///   inode reuse and is only advisory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootIdentity {
    /// Device number (`st_dev`) of the root directory object.
    pub device: u64,
    /// Inode number (`st_ino`) of the root directory object.
    pub inode: u64,
}

impl RootIdentity {
    /// Canonical byte encoding for content addressing: `device` then `inode`,
    /// each little-endian `u64` (16 bytes total).
    pub fn to_bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&self.device.to_le_bytes());
        out[8..].copy_from_slice(&self.inode.to_le_bytes());
        out
    }
}

impl fmt::Display for RootIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dev{}:ino{}", self.device, self.inode)
    }
}

/// An authoritative handle on a granted root directory: an `OwnedFd` plus the
/// [`RootIdentity`] observed from it at acquisition.
///
/// This is the seam's authority object. Acquire it **once, at authority
/// resolution time** (when the grant is admitted / the leash check passes) via
/// [`GrantedRoot::acquire`], hold it for the lifetime of the resolved
/// authority, and perform every bounded open through it. Dropping it closes
/// the descriptor and ends the authority. The pathname it was acquired from is
/// retained only as [`provenance`](Self::provenance) — audit text — and is
/// never used to re-derive authority.
#[derive(Debug)]
pub struct GrantedRoot {
    dir: OwnedFd,
    identity: RootIdentity,
    provenance: PathBuf,
}

impl GrantedRoot {
    /// Resolve `root` — an **absolute, canonical** directory path — into an
    /// authoritative descriptor. This is the one place in the seam where a
    /// pathname becomes authority, so it happens under the strictest available
    /// resolution: on Linux a single `openat2(RESOLVE_NO_SYMLINKS)`, elsewhere
    /// a per-component `O_NOFOLLOW | O_DIRECTORY` walk from `/`. Any symlink
    /// component — including an ancestor swapped for one after the caller's
    /// canonicalization — is refused ([`is_resolution_refusal`]), never
    /// followed.
    ///
    /// A relative `root` is refused up front: relative paths resolve through
    /// the current working directory, which is ambient authority this seam
    /// exists to exclude.
    ///
    /// What acquisition can and cannot promise: a swapped-in *symlink* on the
    /// path is refused; an ancestor renamed and replaced by a **real
    /// directory** between the caller's resolution and this call is
    /// indistinguishable at the syscall level (pathnames are not capabilities
    /// — that is the point of this type). Acquire as close to authority
    /// resolution as possible, record the returned [`identity`](Self::identity)
    /// in the authority-bearing record, and hold the handle from then on:
    /// after acquisition the window is closed for good.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "GrantedRoot::acquire requires an absolute canonical root, got {root:?} \
                     (a relative root resolves through the CWD — ambient authority)"
                ),
            ));
        }
        let fd = acquire_root_fd(root)?;
        Self::from_owned_fd(fd, root)
    }

    /// Adopt an **already-authoritative** directory descriptor — one delegated
    /// by a parent, received over a socket, or produced by an earlier bounded
    /// open — without ever consulting a pathname. Verifies the descriptor
    /// refers to a directory (`ENOTDIR` otherwise) and records its identity.
    /// `provenance` is audit text only.
    pub fn from_owned_fd(fd: OwnedFd, provenance: impl Into<PathBuf>) -> io::Result<Self> {
        let file = File::from(fd);
        let md = file.metadata()?;
        if !md.file_type().is_dir() {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        Ok(GrantedRoot {
            identity: RootIdentity {
                device: md.dev(),
                inode: md.ino(),
            },
            dir: OwnedFd::from(file),
            provenance: provenance.into(),
        })
    }

    /// Open `rel` for reading, with resolution bounded beneath this root.
    /// An empty `rel` opens the root directory itself.
    pub fn open_read(&self, rel: &Path) -> io::Result<File> {
        self.open_beneath(rel, OpenKind::Read)
    }

    /// Open `rel` for writing (create if absent; `append` appends, otherwise
    /// truncates — the `>` / `>>` shapes), bounded beneath this root.
    pub fn open_write(&self, rel: &Path, append: bool) -> io::Result<File> {
        self.open_beneath(rel, OpenKind::Write { append })
    }

    /// The identity of the directory object this handle holds — the value an
    /// authority-bearing record (grant, admitted fence, audit event) should
    /// carry instead of the pathname. See [`RootIdentity`].
    pub fn identity(&self) -> RootIdentity {
        self.identity
    }

    /// The pathname this authority was resolved from — **provenance only**
    /// (display, audit, diagnostics). After a namespace mutation it may no
    /// longer name the held object; it is never used to re-derive authority.
    pub fn provenance(&self) -> &Path {
        &self.provenance
    }

    /// Borrow the underlying root descriptor (e.g. to pass across an
    /// enforcement seam that speaks raw descriptors).
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.dir.as_fd()
    }

    /// Duplicate the handle (same directory object, same identity).
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(GrantedRoot {
            dir: self.dir.try_clone()?,
            identity: self.identity,
            provenance: self.provenance.clone(),
        })
    }

    fn open_beneath(&self, rel: &Path, kind: OpenKind) -> io::Result<File> {
        // Fail closed on any component that could steer resolution by itself;
        // the kernel-side flags below then only have to stop *filesystem*
        // tricks (symlinks planted after the caller's check).
        let mut components: Vec<&OsStr> = Vec::new();
        for comp in rel.components() {
            match comp {
                Component::Normal(name) => components.push(name),
                Component::CurDir => {}
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("bounded open refuses non-normal path component {other:?}"),
                    ));
                }
            }
        }
        if components.is_empty() && matches!(kind, OpenKind::Write { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bounded write open of the root directory itself",
            ));
        }
        open_beneath_impl(self.dir.as_fd(), &components, kind)
    }
}

/// True iff `err` is the *resolution itself* refusing — an escape or planted
/// symlink stopped by `RESOLVE_BENEATH` / `RESOLVE_NO_SYMLINKS` / `O_NOFOLLOW`
/// — as opposed to an ordinary open failure (`ENOENT`, `EACCES`, …). Callers
/// use this to report an authority denial rather than an I/O error.
///
/// Spellings: `ELOOP` (`openat2`'s symlink refusal, and the walk leg's answer
/// for a *final*-component symlink on both Linux and Darwin), `EXDEV`
/// (`RESOLVE_BENEATH` escape), `EMLINK` (the `O_NOFOLLOW` spelling on some
/// BSDs — retained for them, but **never observed on Darwin**), and `ENOTDIR`.
///
/// `ENOTDIR` is not a courtesy inclusion: measured on macOS 26.5.2 /
/// xnu-12377 arm64, it is **the** spelling the walk leg returns for an
/// *intermediate* symlink (to a directory, to a file, or dangling), because
/// those components are opened `O_DIRECTORY | O_NOFOLLOW` and the
/// not-a-directory check fires first. Omitting it would misreport the walk
/// leg's most common denial as an ordinary I/O error on the very platform
/// that leg exists for.
///
/// **This predicate answers "was the resolution refused?", never "why?".** On
/// the walk leg `ENOTDIR` covers a refused symlink *and* an honestly
/// non-directory component; the ambiguity is resolved toward the security
/// reading (evidence must never under-report a refusal). A caller — or a test
/// — that needs to establish that a *symlink specifically* was not followed
/// must compare descriptor identity (`fstat` dev/ino), not errno. See the
/// module docs.
pub fn is_resolution_refusal(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ELOOP) | Some(libc::EXDEV) | Some(libc::EMLINK) | Some(libc::ENOTDIR)
    )
}

#[derive(Clone, Copy)]
enum OpenKind {
    Read,
    Write { append: bool },
}

impl OpenKind {
    fn flags(self) -> libc::c_int {
        match self {
            OpenKind::Read => libc::O_RDONLY,
            OpenKind::Write { append } => {
                libc::O_WRONLY
                    | libc::O_CREAT
                    | if append {
                        libc::O_APPEND
                    } else {
                        libc::O_TRUNC
                    }
            }
        }
    }
}

fn cstr(bytes: &OsStr) -> io::Result<CString> {
    CString::new(bytes.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

// ── Linux: openat2 for both acquisition and per-open enforcement ─────────────

/// The stable openat2(2) ABI (`struct open_how`): three u64s. Defined locally
/// because libc's `open_how` is #[non_exhaustive] and cannot be constructed by
/// literal.
#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Raw `openat2` with the retry policy shared by acquisition and bounded
/// opens: `EINTR` retries; `EAGAIN` (openat2's documented detected-race
/// result) retries a bounded number of times, then surfaces (fail closed,
/// never a fallback to an unbounded open).
#[cfg(target_os = "linux")]
fn openat2_fd(dirfd: libc::c_int, path: &std::ffi::CStr, how: &OpenHow) -> io::Result<OwnedFd> {
    let mut attempts = 0;
    loop {
        // SAFETY: raw `openat2` syscall with a valid dirfd, NUL-terminated
        // path, and a properly sized `open_how`; the returned descriptor is
        // immediately owned.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dirfd,
                path.as_ptr(),
                std::ptr::addr_of!(*how),
                std::mem::size_of::<OpenHow>(),
            )
        };
        if rc >= 0 {
            // SAFETY: `rc` is a freshly opened, owned descriptor.
            return Ok(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(rc as i32) });
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) if attempts < 8 => {
                attempts += 1;
                continue;
            }
            _ => return Err(err),
        }
    }
}

/// Acquire the root descriptor from an absolute pathname with every symlink
/// component refused: `openat2(RESOLVE_NO_SYMLINKS)` resolves the WHOLE path
/// — ancestors included — in one kernel-checked pass. (An absolute pathname
/// makes openat2 ignore `dirfd`; `RESOLVE_BENEATH` is deliberately absent
/// here, since this is the one authorized absolute resolution.)
#[cfg(target_os = "linux")]
fn acquire_root_fd(root: &Path) -> io::Result<OwnedFd> {
    let c = cstr(root.as_os_str())?;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: libc::RESOLVE_NO_SYMLINKS,
    };
    openat2_fd(libc::AT_FDCWD, &c, &how)
}

#[cfg(target_os = "linux")]
fn open_beneath_impl(
    root: BorrowedFd<'_>,
    components: &[&OsStr],
    kind: OpenKind,
) -> io::Result<File> {
    // Join the validated components into one relative path; `openat2` resolves
    // it in a single kernel-checked pass. Empty → the root itself (".").
    let mut rel_bytes: Vec<u8> = Vec::new();
    for (i, comp) in components.iter().enumerate() {
        if i > 0 {
            rel_bytes.push(b'/');
        }
        rel_bytes.extend_from_slice(comp.as_bytes());
    }
    if rel_bytes.is_empty() {
        rel_bytes.push(b'.');
    }
    let rel_c = CString::new(rel_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;

    let how = OpenHow {
        flags: (kind.flags() | libc::O_CLOEXEC) as u64,
        mode: match kind {
            OpenKind::Read => 0,
            OpenKind::Write { .. } => 0o666,
        },
        resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS,
    };
    openat2_fd(root.as_raw_fd(), &rel_c, &how).map(File::from)
}

// ── Other Unix (macOS): O_NOFOLLOW component walks ───────────────────────────

/// `openat` one directory component `O_NOFOLLOW | O_DIRECTORY` relative to
/// `dirfd`; any symlink terminates resolution.
#[cfg(all(unix, not(target_os = "linux")))]
fn openat_dir_nofollow(dirfd: libc::c_int, comp: &std::ffi::CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    loop {
        // SAFETY: `openat` FFI relative to a live directory descriptor; the
        // returned descriptor is immediately owned.
        let fd = unsafe { libc::openat(dirfd, comp.as_ptr(), flags) };
        if fd >= 0 {
            // SAFETY: freshly opened, owned descriptor.
            return Ok(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) });
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Acquire the root descriptor from an absolute pathname with every symlink
/// component refused: a per-component `O_NOFOLLOW | O_DIRECTORY` walk from
/// `/`, the walk equivalent of Linux's `RESOLVE_NO_SYMLINKS` acquisition.
#[cfg(all(unix, not(target_os = "linux")))]
fn acquire_root_fd(root: &Path) -> io::Result<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;

    // `/` itself cannot be a symlink; no O_NOFOLLOW needed for the anchor.
    let mut dir = OwnedFd::from(
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open("/")?,
    );
    for comp in root.components() {
        match comp {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::Normal(name) => {
                dir = openat_dir_nofollow(dir.as_raw_fd(), &cstr(name)?)?;
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("root acquisition refuses non-normal path component {other:?}"),
                ));
            }
        }
    }
    Ok(dir)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_beneath_impl(
    root: BorrowedFd<'_>,
    components: &[&OsStr],
    kind: OpenKind,
) -> io::Result<File> {
    let Some((last, intermediate)) = components.split_last() else {
        // Empty `rel`, read (write of the root was refused up front): reopen
        // the root itself via "." — descriptor-relative, no pathname.
        return openat_dir_nofollow(root.as_raw_fd(), &cstr(OsStr::new("."))?).map(File::from);
    };

    // ══════════════════════════════════════════════════════════════════════
    //  DO NOT COLLAPSE THIS LOOP INTO A SINGLE MULTI-COMPONENT `openat`.
    //
    //  `O_NOFOLLOW` constrains ONLY the final component. Measured natively on
    //  both Linux and macOS 26.5.2/xnu-12377:
    //
    //      openat(root, "symdir/leaf", O_RDONLY|O_DIRECTORY|O_NOFOLLOW)
    //          SUCCEEDS, and the fd it returns is the object BENEATH the
    //          symlink — i.e. resolution walked straight through it.
    //
    //  One component at a time is therefore the entire enforcement mechanism
    //  on this leg, not a stylistic choice: `"a/b/c"` in one call would leave
    //  `a` and `b` unguarded and silently reopen the confinement hole this
    //  module exists to close — while every refusal test kept passing, because
    //  they exercise single-component refusals. That vacuity is exactly why
    //  `o_nofollow_alone_opens_through_an_intermediate_symlink` exists: it is
    //  a positive control that FAILS if this walk is ever flattened.
    // ══════════════════════════════════════════════════════════════════════
    let mut owned: Option<OwnedFd> = None;
    for comp in intermediate {
        let cur = owned.as_ref().map_or(root.as_raw_fd(), AsRawFd::as_raw_fd);
        // Exactly one component per call — see the banner above.
        owned = Some(openat_dir_nofollow(cur, &cstr(comp)?)?);
    }

    let cur = owned.as_ref().map_or(root.as_raw_fd(), AsRawFd::as_raw_fd);
    let c = cstr(last)?;
    let flags = kind.flags() | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    loop {
        // SAFETY: `openat` FFI relative to a live directory descriptor; the
        // returned descriptor is immediately owned by `File`.
        let fd = unsafe { libc::openat(cur, c.as_ptr(), flags, 0o666 as libc::c_uint) };
        if fd >= 0 {
            // SAFETY: freshly opened, owned descriptor.
            return Ok(unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd) });
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn tmp_root(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fdguard-beneath-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn read_to_string(mut f: File) -> String {
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    }

    // ── Expected refusal spellings, per enforcement leg ──────────────────────
    //
    // The errno is part of what is under test (#354 item 6), so these tests
    // assert the SPECIFIC value, not merely `is_resolution_refusal`. A boolean
    // assertion cannot tell "refused as a symlink" from "refused for some other
    // reason the classifier also accepts", and the two legs genuinely differ —
    // measured, not assumed:
    //
    // | case                          | Linux (openat2) | walk (`O_NOFOLLOW`) |
    // |-------------------------------|-----------------|---------------------|
    // | intermediate-component symlink| `ELOOP`         | `ENOTDIR`           |
    // | final-component symlink       | `ELOOP`         | `ELOOP` (BSD `EMLINK`)|
    // | non-directory intermediate    | `ENOTDIR`       | `ENOTDIR`           |
    //
    // The walk leg reports `ENOTDIR` for an intermediate symlink because the
    // component is opened `O_DIRECTORY | O_NOFOLLOW`: the not-a-directory check
    // fires before `O_NOFOLLOW`'s `ELOOP`. The open still FAILS, so the symlink
    // is never followed and the bound holds — but it makes #354 item 6
    // load-bearing rather than cosmetic: without `ENOTDIR` in
    // `is_resolution_refusal`, the walk leg's PRIMARY refusal would be
    // misreported as an ordinary I/O error on exactly the platform that leg
    // serves (macOS).

    /// Refusal spellings accepted for a symlink at an INTERMEDIATE component.
    #[cfg(target_os = "linux")]
    const INTERMEDIATE_SYMLINK: &[i32] = &[libc::ELOOP];
    /// The walk leg's spelling is platform-dependent (`ENOTDIR` on the
    /// `O_DIRECTORY | O_NOFOLLOW` open; some BSDs answer `ELOOP`/`EMLINK`
    /// first), so the set is wider here — but still excludes `EXDEV` and every
    /// non-refusal errno.
    #[cfg(not(target_os = "linux"))]
    const INTERMEDIATE_SYMLINK: &[i32] = &[libc::ENOTDIR, libc::ELOOP, libc::EMLINK];

    /// Refusal spellings accepted for a symlink as the FINAL component: no
    /// `O_DIRECTORY` is involved, so `O_NOFOLLOW` answers directly.
    const FINAL_SYMLINK: &[i32] = &[libc::ELOOP, libc::EMLINK];

    /// Spelling for a RAW `openat(.., O_DIRECTORY | O_NOFOLLOW)` on a lone
    /// symlink component. Distinct from [`INTERMEDIATE_SYMLINK`], which
    /// describes what the *seam* returns: on Linux the seam uses `openat2` and
    /// answers `ELOOP`, but this raw syscall is the walk-leg shape and answers
    /// `ENOTDIR` on **both** platforms (measured: the Linux test host, macOS 26.5.2 /
    /// xnu-12377 arm64). Conflating the two is a real trap — it cost this test
    /// a red run.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    const RAW_NOFOLLOW_DIR_SYMLINK: &[i32] = &[libc::ENOTDIR];
    /// Untested BSDs may answer with the `O_NOFOLLOW` spellings instead.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const RAW_NOFOLLOW_DIR_SYMLINK: &[i32] = &[libc::ENOTDIR, libc::ELOOP, libc::EMLINK];

    /// `(dev, ino)` of a path, for identity comparisons. Errno cannot say WHY
    /// a resolution refused (see the module docs), so where a test needs to
    /// prove *which object* an open landed on, it compares identity instead.
    fn ident_of(p: &Path) -> (u64, u64) {
        let md = p.metadata().expect("stat fixture");
        (md.dev(), md.ino())
    }

    /// `(dev, ino)` of an open descriptor.
    fn ident_of_fd(f: &File) -> (u64, u64) {
        let md = f.metadata().expect("fstat");
        (md.dev(), md.ino())
    }

    /// Assert a refusal is BOTH classified as an authority denial AND spelled
    /// the way this platform's leg is expected to spell it. Always prints the
    /// observed errno, so a `--nocapture` run on any platform reports the
    /// concrete spelling rather than leaving a green boolean to be trusted.
    ///
    /// Note the limit this cannot exceed: on the walk leg `ENOTDIR` is one
    /// spelling for several distinct refusals, so a pass here proves the open
    /// was REFUSED, not that a *symlink* was refused. Tests needing the latter
    /// compare identity via [`ident_of`] / [`ident_of_fd`].
    #[track_caller]
    fn assert_refused_with(err: &io::Error, expected: &[i32], what: &str) {
        let got = err.raw_os_error();
        eprintln!("[refusal] {what}: errno={got:?} ({err})");
        assert!(
            is_resolution_refusal(err),
            "{what}: must classify as a resolution refusal, got {err:?}"
        );
        assert!(
            got.is_some_and(|n| expected.contains(&n)),
            "{what}: expected one of {expected:?} (the spelling this leg is \
             specified to produce), got {got:?} — a refusal for an unintended \
             reason is not evidence for the intended one"
        );
    }

    /// In-scope opens work: create-truncate, append, then read back — all
    /// through one acquired handle.
    #[test]
    fn in_scope_write_append_read_roundtrip() {
        let root = tmp_root("roundtrip");
        std::fs::create_dir(root.join("sub")).unwrap();
        let rel = Path::new("sub/out.txt");

        let granted = GrantedRoot::acquire(&root).expect("acquire");
        let mut f = granted.open_write(rel, false).expect("create");
        f.write_all(b"one").unwrap();
        drop(f);
        let mut f = granted.open_write(rel, true).expect("append");
        f.write_all(b"two").unwrap();
        drop(f);

        // Content proves the bytes; identity proves it is the INTENDED object
        // rather than a same-content substitute (errno/success cannot).
        let opened = granted.open_read(rel).expect("read");
        assert_eq!(ident_of_fd(&opened), ident_of(&root.join("sub/out.txt")));
        assert_eq!(read_to_string(opened), "onetwo");
        assert_eq!(granted.provenance(), root.as_path());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The check→open TOCTOU regression (agent-bridle#351): a path component
    /// swapped for a symlink pointing OUTSIDE the root after the caller's
    /// check must be refused at open time — a plain `open` would follow it —
    /// and the refusal must classify as a resolution refusal (#354 item 6:
    /// enforcement outcome and evidence classification agree).
    #[test]
    fn symlink_component_escape_is_refused() {
        let root = tmp_root("swap");
        let outside = tmp_root("swap-outside");
        std::fs::write(outside.join("victim.txt"), b"outside").unwrap();

        let granted = GrantedRoot::acquire(&root).expect("acquire");
        // The "swap": what the check saw as a real directory is now a symlink.
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();

        // Positive control: the unbounded open follows the planted symlink —
        // the exact behavior the mediated open exists to remove.
        assert!(
            std::fs::File::open(root.join("sub/victim.txt")).is_ok(),
            "positive control: a plain open follows the planted symlink"
        );

        let err = granted
            .open_read(Path::new("sub/victim.txt"))
            .expect_err("bounded open must refuse the symlinked component");
        assert_refused_with(
            &err,
            INTERMEDIATE_SYMLINK,
            "escape via intermediate symlink (read)",
        );

        let err = granted
            .open_write(Path::new("sub/victim.txt"), false)
            .expect_err("bounded write must refuse the symlinked component");
        assert_refused_with(
            &err,
            INTERMEDIATE_SYMLINK,
            "escape via intermediate symlink (write)",
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A symlink as the FINAL component is refused too (Conservative: even one
    /// that resolves in-root — honest callers pass canonical paths).
    #[test]
    fn symlink_final_component_is_refused() {
        let root = tmp_root("final");
        std::fs::write(root.join("real.txt"), b"real").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        let granted = GrantedRoot::acquire(&root).expect("acquire");
        let err = granted
            .open_read(Path::new("link.txt"))
            .expect_err("final symlink must be refused");
        assert_refused_with(&err, FINAL_SYMLINK, "final-component symlink");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An in-root symlink at an INTERMEDIATE component is refused too — the
    /// Conservative posture is about symlinks per se, not only escapes.
    #[test]
    fn symlink_intermediate_in_root_is_refused() {
        let root = tmp_root("mid");
        std::fs::create_dir(root.join("realdir")).unwrap();
        std::fs::write(root.join("realdir/f.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("realdir"), root.join("alias")).unwrap();

        let granted = GrantedRoot::acquire(&root).expect("acquire");
        let err = granted
            .open_read(Path::new("alias/f.txt"))
            .expect_err("in-root intermediate symlink must be refused");
        assert_refused_with(&err, INTERMEDIATE_SYMLINK, "in-root intermediate symlink");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `..` and absolute components never reach the kernel: refused up front.
    #[test]
    fn dotdot_and_absolute_rel_are_refused() {
        let root = tmp_root("dotdot");
        let granted = GrantedRoot::acquire(&root).expect("acquire");
        assert_eq!(
            granted
                .open_read(Path::new("../etc/hosts"))
                .expect_err("..-escape must be refused")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            granted
                .open_read(Path::new("/etc/hosts"))
                .expect_err("absolute rel must be refused")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty `rel` reads the root directory handle itself; a write open of
    /// the root itself is refused up front on every platform.
    #[test]
    fn empty_rel_opens_the_root_for_read_and_refuses_write() {
        let root = tmp_root("self");
        let granted = GrantedRoot::acquire(&root).expect("acquire");
        granted.open_read(Path::new("")).expect("open root");
        assert_eq!(
            granted
                .open_write(Path::new(""), false)
                .expect_err("write open of the root itself must be refused")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── #354 hostile-mutation suite: the pathname must not become authority
    //    again after acquisition ─────────────────────────────────────────────

    /// THE #354 regression: an ANCESTOR of the canonical root swapped for a
    /// symlink after authority resolution. The old acquisition (`open(path,
    /// O_NOFOLLOW)`) protects only the final component — the positive control
    /// proves that exact open follows the swapped ancestor and lands on the
    /// wrong root — so `GrantedRoot::acquire` must refuse the symlink
    /// component instead of walking through it. This test FAILS against the
    /// pathname-reopen design and PASSES with symlink-free acquisition.
    #[test]
    fn ancestor_symlink_swap_at_acquisition_is_refused() {
        use std::os::unix::fs::OpenOptionsExt;

        let base = tmp_root("anc-swap");
        let real = base.join("real");
        std::fs::create_dir_all(real.join("root")).unwrap();
        std::fs::write(real.join("root/victim.txt"), b"wrong-root").unwrap();
        // The swap: the ancestor `parent` the caller canonicalized through is
        // now a symlink to somewhere else entirely.
        std::os::unix::fs::symlink(&real, base.join("parent")).unwrap();
        let swapped_root = base.join("parent").join("root");

        // Positive control: the OLD acquisition — a pathname re-open with only
        // final-component O_NOFOLLOW — follows the swapped ancestor and
        // happily opens the WRONG root.
        assert!(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                .open(&swapped_root)
                .is_ok(),
            "positive control: final-component O_NOFOLLOW follows a swapped ancestor"
        );

        let err = GrantedRoot::acquire(&swapped_root)
            .expect_err("acquisition through a symlinked ancestor must be refused");
        assert_refused_with(
            &err,
            INTERMEDIATE_SYMLINK,
            "ancestor symlink at acquisition",
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Ancestor RENAME after acquisition: authority tracks the directory
    /// object, not the name. Opens through the handle keep reaching the real
    /// (moved) tree; an imposter placed at the old pathname is unreachable
    /// through the handle, while pathname re-resolution (the old failure mode)
    /// is redirected to the imposter.
    #[test]
    fn ancestor_rename_after_acquisition_does_not_redirect() {
        let base = tmp_root("anc-rename");
        std::fs::create_dir_all(base.join("parent/root")).unwrap();
        std::fs::write(base.join("parent/root/data.txt"), b"real").unwrap();
        let root_path = base.join("parent/root");

        let granted = GrantedRoot::acquire(&root_path).expect("acquire");

        // Namespace mutation: rename the ancestor, then rebuild an imposter
        // tree at the ORIGINAL pathname.
        std::fs::rename(base.join("parent"), base.join("parent-moved")).unwrap();
        std::fs::create_dir_all(base.join("parent/root")).unwrap();
        std::fs::write(base.join("parent/root/data.txt"), b"imposter").unwrap();

        // Positive control: pathname re-resolution now yields the imposter —
        // exactly what reconstructing authority from the path would open.
        assert_eq!(
            std::fs::read_to_string(root_path.join("data.txt")).unwrap(),
            "imposter",
            "positive control: the pathname now names the imposter"
        );

        // The handle still opens the REAL tree (moved with its ancestor).
        assert_eq!(
            read_to_string(granted.open_read(Path::new("data.txt")).expect("read")),
            "real",
            "authority must track the directory object, not the pathname"
        );

        // A fresh pathname acquisition observes a DIFFERENT identity: the
        // redirection cannot hide from the identity anchor.
        let re = GrantedRoot::acquire(&root_path).expect("acquire imposter");
        assert_ne!(
            re.identity(),
            granted.identity(),
            "the imposter root must not share the resolved authority's identity"
        );
        assert_ne!(re.identity().to_bytes(), granted.identity().to_bytes());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Root DELETION + replacement after acquisition: the handle keeps naming
    /// the (now unlinked) original object — opens fail closed with ENOENT and
    /// can never be redirected into the imposter, while pathname
    /// re-resolution reaches the imposter.
    #[test]
    fn root_replacement_after_acquisition_fails_closed() {
        let base = tmp_root("root-replace");
        let root_path = base.join("root");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(root_path.join("f.txt"), b"real").unwrap();

        let granted = GrantedRoot::acquire(&root_path).expect("acquire");

        // Delete the granted root entirely, then recreate an imposter at the
        // same pathname.
        std::fs::remove_dir_all(&root_path).unwrap();
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(root_path.join("f.txt"), b"imposter").unwrap();

        // Positive control: the pathname reaches the imposter.
        assert_eq!(
            std::fs::read_to_string(root_path.join("f.txt")).unwrap(),
            "imposter"
        );

        // The handle must NOT be redirected: the original object is gone, so
        // reads and creates beneath it fail closed (never the imposter).
        let err = granted
            .open_read(Path::new("f.txt"))
            .expect_err("the deleted root's handle must not see the imposter");
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
        let err = granted
            .open_write(Path::new("g.txt"), false)
            .expect_err("creating beneath a deleted root must fail closed");
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));

        // And the imposter's identity differs from the resolved authority's.
        let re = GrantedRoot::acquire(&root_path).expect("acquire imposter");
        assert_ne!(re.identity(), granted.identity());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A relative root is ambient authority (CWD-dependent) and is refused.
    #[test]
    fn relative_root_is_refused() {
        assert_eq!(
            GrantedRoot::acquire(Path::new("some/relative/dir"))
                .expect_err("relative root must be refused")
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    /// Adopting a non-directory descriptor is refused (ENOTDIR): a file fd is
    /// not a root authority.
    #[test]
    fn from_owned_fd_refuses_a_non_directory() {
        let root = tmp_root("adopt");
        std::fs::write(root.join("f.txt"), b"x").unwrap();
        let fd = OwnedFd::from(std::fs::File::open(root.join("f.txt")).unwrap());
        let err = GrantedRoot::from_owned_fd(fd, root.join("f.txt"))
            .expect_err("a file descriptor must be refused as a root");
        assert_eq!(err.raw_os_error(), Some(libc::ENOTDIR));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Adopting an already-authoritative directory descriptor works and never
    /// consults the pathname (the provenance is deliberately wrong here).
    #[test]
    fn from_owned_fd_adopts_a_directory_without_pathname_authority() {
        let root = tmp_root("adopt-dir");
        std::fs::write(root.join("f.txt"), b"via-fd").unwrap();
        let fd = OwnedFd::from(std::fs::File::open(&root).unwrap());
        let granted = GrantedRoot::from_owned_fd(fd, "/definitely/not/the/path").expect("adopt");
        assert_eq!(
            read_to_string(granted.open_read(Path::new("f.txt")).expect("read")),
            "via-fd"
        );
        assert_eq!(granted.provenance(), Path::new("/definitely/not/the/path"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **Positive control for the walk leg's per-component discipline.**
    ///
    /// `O_NOFOLLOW` constrains only the FINAL component, so a single
    /// multi-component `openat` walks straight through an intermediate
    /// symlink. This test measures that directly — the open SUCCEEDS and the
    /// descriptor it returns is the object *beneath* the symlink (proven by
    /// identity, since errno cannot speak here and success has no errno at
    /// all) — while the same component opened on its own is refused.
    ///
    /// Why it exists: it is the load-bearing justification for opening one
    /// component at a time in `open_beneath_impl`'s walk leg, and it fails
    /// loudly if anyone ever "simplifies" that loop into a single call. Every
    /// *refusal* test would stay green through such a change, because they
    /// exercise single-component refusals — the same vacuous-oracle shape this
    /// PR already had to fix once. Verified natively on Linux and on macOS
    /// 26.5.2 / xnu-12377 arm64.
    #[test]
    fn o_nofollow_alone_opens_through_an_intermediate_symlink() {
        let root = tmp_root("nofollow-control");
        std::fs::create_dir_all(root.join("real/leaf")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("symdir")).unwrap();

        let granted = GrantedRoot::acquire(&root).expect("acquire");
        let rootfd = granted.as_fd().as_raw_fd();
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

        // THE MEASUREMENT: two components in one call defeats `O_NOFOLLOW`.
        //
        // DO NOT "TIDY" THE NEXT THREE LINES. The shape under test is a
        // RELATIVE, MULTI-COMPONENT path resolved from a directory fd. Two
        // rewrites a reviewer may suggest in good faith both destroy it while
        // staying green:
        //   * an ABSOLUTE path — changes the resolution root, so the property
        //     being measured (relative resolution beneath a dirfd) is gone;
        //   * a COMPONENT-AT-A-TIME sequence — that IS the walk, and it will
        //     correctly refuse, turning the positive control into a second
        //     refusal test that proves nothing about why the walk is needed.
        let multi = cstr(OsStr::new("symdir/leaf")).unwrap();
        // SAFETY: `openat` FFI on a live root descriptor; the fd is owned
        // immediately. Test-only: this is the UNSAFE shape, exercised on
        // purpose to prove it is unsafe.
        let fd = unsafe { libc::openat(rootfd, multi.as_ptr(), flags) };
        assert!(
            fd >= 0,
            "positive control: a multi-component openat with O_NOFOLLOW must \
             traverse the intermediate symlink (errno {:?}) — if this now \
             refuses, the platform changed and the walk's rationale needs \
             re-measuring, not deleting",
            io::Error::last_os_error().raw_os_error()
        );
        // SAFETY: freshly opened, owned descriptor.
        let via_symlink = unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd) };

        // IDENTITY, not the success bit. "It opened" would stay true if the
        // fixture ever grew a real `symdir` directory, or if `symdir/leaf`
        // came to resolve somewhere else — the control would be green while
        // the property it guards had evaporated (the vacuous-oracle shape).
        // Opening BOTH paths and comparing (dev, ino) cannot be satisfied by
        // an accidental success: it asserts the two names reached the SAME
        // object, which is precisely what "traversed the symlink" means.
        let intended = cstr(OsStr::new("real/leaf")).unwrap();
        // SAFETY: `openat` FFI on a live root descriptor; fd owned immediately.
        let fd2 = unsafe { libc::openat(rootfd, intended.as_ptr(), flags) };
        assert!(fd2 >= 0, "the intended target must open");
        // SAFETY: freshly opened, owned descriptor.
        let direct = unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd2) };

        assert_eq!(
            ident_of_fd(&via_symlink),
            ident_of_fd(&direct),
            "`symdir/leaf` and `real/leaf` must be the SAME object (dev, ino) \
             — that is what 'followed the symlink' means, and neither errno \
             nor a success bit can establish it"
        );
        // Belt and braces: and that object is the fixture's real leaf.
        assert_eq!(ident_of_fd(&direct), ident_of(&root.join("real/leaf")));

        // …whereas the walk's actual discipline — one component per call —
        // refuses that same symlink outright.
        let single = cstr(OsStr::new("symdir")).unwrap();
        // SAFETY: `openat` FFI on a live root descriptor.
        let refused = unsafe { libc::openat(rootfd, single.as_ptr(), flags) };
        assert!(refused < 0, "a lone symlink component must be refused");
        assert_refused_with(
            &io::Error::last_os_error(),
            RAW_NOFOLLOW_DIR_SYMLINK,
            "lone symlink component under raw O_DIRECTORY|O_NOFOLLOW",
        );

        // And the seam itself refuses the whole path, on either leg.
        let err = granted
            .open_read(Path::new("symdir/leaf"))
            .expect_err("the bounded open must refuse what raw openat allows");
        assert_refused_with(&err, INTERMEDIATE_SYMLINK, "bounded open of symdir/leaf");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #354 item 6: `ENOTDIR` — the kernel refusing to treat a non-directory
    /// as a path component — classifies as a resolution refusal, so
    /// enforcement outcome and evidence classification agree.
    #[test]
    fn enotdir_classifies_as_a_resolution_refusal() {
        let root = tmp_root("enotdir");
        std::fs::write(root.join("afile"), b"not a dir").unwrap();
        let granted = GrantedRoot::acquire(&root).expect("acquire");
        let err = granted
            .open_read(Path::new("afile/sub.txt"))
            .expect_err("a file used as an intermediate component must fail");
        assert_refused_with(&err, &[libc::ENOTDIR], "non-directory intermediate");
        // Ordinary open failures stay ordinary — the classifier must not
        // swallow everything, or "is a refusal" would carry no information.
        let missing = granted
            .open_read(Path::new("no-such-file.txt"))
            .expect_err("missing file");
        assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
        assert!(!is_resolution_refusal(&missing), "ENOENT is not a refusal");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// #354 item 7, executable: a grant means the **pathname subtree including
    /// nested mounts**, so a bounded open MAY traverse a mount transition
    /// beneath the root — and the same open WOULD be refused if
    /// `RESOLVE_NO_XDEV` were set. Both halves are asserted so the decision is
    /// a tested theorem, not a comment.
    ///
    /// **Linux-only and ABSENT (not skipped) elsewhere**, because it needs a
    /// mount transition reachable without privilege and the `RESOLVE_NO_XDEV`
    /// flag, both Linux-specific. A `cfg`'d-out test never appears in the
    /// `ignored` count, so record it where the platform coverage is read, not
    /// only here: this decision is UNVERIFIED on the walk leg.
    ///
    /// It never self-skips. A silent early return reports as `ok`, which is
    /// indistinguishable from a real pass ("SKIP is not PASS" — see
    /// `formal/assurance/assumptions.md`), so an absent mount transition is a
    /// hard failure instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_grant_includes_nested_mounts_and_no_xdev_would_refuse_them() {
        // Any parent whose child sits on a different `st_dev` is a mount
        // transition. `/dev` → `/dev/shm` is preferred because it is writable,
        // so the traversal can be proven by reading content THROUGH it; the
        // rest are read-only fallbacks that still prove the transition.
        let writable = Path::new("/dev/shm");
        let transition = [
            (Path::new("/dev"), "shm"),
            (Path::new("/"), "proc"),
            (Path::new("/"), "sys"),
            (Path::new("/"), "run"),
            (Path::new("/"), "dev"),
        ]
        .into_iter()
        .find(
            |(parent, child)| match (parent.metadata(), parent.join(child).metadata()) {
                (Ok(p), Ok(c)) => p.dev() != c.dev(),
                _ => false,
            },
        );
        let Some((parent, child)) = transition else {
            panic!(
                "no unprivileged mount transition found on this host — the mount \
                 decision cannot be verified, and a silent skip would report as a pass"
            );
        };
        eprintln!("[mount] using transition {}/{child}", parent.display());

        let granted = GrantedRoot::acquire(parent).expect("acquire the mount parent");

        // Decision (a): traversal into the nested mount is ALLOWED.
        let rel = if parent == Path::new("/dev") && child == "shm" {
            // Prove content really flows across the transition.
            let name = format!("fdguard-xdev-{}", std::process::id());
            std::fs::write(writable.join(&name), b"across-the-mount")
                .expect("write into the nested mount");
            let rel = PathBuf::from(child).join(&name);
            assert_eq!(
                read_to_string(granted.open_read(&rel).expect("read across the mount")),
                "across-the-mount",
                "a grant covers the pathname subtree INCLUDING nested mounts"
            );
            let _ = std::fs::remove_file(writable.join(&name));
            rel
        } else {
            // Read-only fallback: opening the mount point itself already
            // crosses the transition.
            let rel = PathBuf::from(child);
            granted
                .open_read(&rel)
                .expect("opening the nested mount point must be allowed");
            rel
        };

        // …and RESOLVE_NO_XDEV would have refused exactly this open, which is
        // why it is deliberately not set (see the module docs).
        let c = CString::new(rel.as_os_str().as_bytes()).unwrap();
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_XDEV,
        };
        let err = openat2_fd(granted.as_fd().as_raw_fd(), &c, &how)
            .expect_err("RESOLVE_NO_XDEV must refuse a crossing of the mount transition");
        assert_eq!(err.raw_os_error(), Some(libc::EXDEV));
    }

    /// Identity and clone semantics: a clone shares the object identity; the
    /// identity survives (is unchanged by) namespace mutation of the pathname.
    #[test]
    fn identity_is_stable_and_shared_by_clones() {
        let base = tmp_root("identity");
        let root_path = base.join("root");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::write(root_path.join("f.txt"), b"real").unwrap();

        let granted = GrantedRoot::acquire(&root_path).expect("acquire");
        let clone = granted.try_clone().expect("clone");
        assert_eq!(granted.identity(), clone.identity());

        // Mutate the namespace: the held identity does not change.
        let before = granted.identity();
        std::fs::rename(&root_path, base.join("root-moved")).unwrap();
        assert_eq!(granted.identity(), before);
        assert_eq!(
            read_to_string(clone.open_read(Path::new("f.txt")).expect("read")),
            "real",
            "a clone holds the same authority object"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
