//! Child-side private loopback, scratch, PTYs and identity, with the existing floor on error.
//! Allocation happens before fork. Private-mount rules bind descriptors opened
//! after mounting, before either fence. No host or parent namespace is changed.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Private pre-exec signal: namespace setup failed before any program ran.
pub const RETRY_DENY_ALL: i32 = libc::ENONET;

/// Directory values needed before private mounts hide their host counterparts.
/// Loader controls and worker-authentication variables are never in this list.
pub const TEMPORARY_ENV: [&str; 5] = ["HOME", "TMPDIR", "TMP", "TEMP", "XDG_RUNTIME_DIR"];

pub const PRIVATE: &str = "Build isolation: private loopback only; private tmpfs /tmp (2 GiB) and /dev/shm (256 MiB); private devpts (64 PTYs); private account files; external networking unavailable.\n";
pub const FALLBACK: &str = "Build isolation: deny-all fallback; loopback, private scratch and private PTYs unavailable. Local tests may fail because of this fence.\n";

/// One regular file a private build must reach under a second name: `source`
/// is attached read-only over `target` in the child's own mount namespace,
/// before either fence. The caller verified both in the host namespace and
/// keeps them open here. The child inherits a mount, never a claim: nothing
/// about the pair is written to the environment, a file or a message.
pub struct SameObjectBind {
    source: (CString, OwnedFd),
    target: (CString, OwnedFd),
}

impl SameObjectBind {
    /// Each path is the absolute, canonical spelling of the held file beside it.
    pub fn new(
        source_path: &std::path::Path,
        source: OwnedFd,
        target_path: &std::path::Path,
        target: OwnedFd,
    ) -> io::Result<Self> {
        let spell = |path: &std::path::Path, held| match CString::new(path.as_os_str().as_bytes()) {
            Ok(spelling) if path.is_absolute() => Ok((spelling, held)),
            _ => Err(io::Error::from_raw_os_error(libc::EINVAL)),
        };
        Ok(Self {
            source: spell(source_path, source)?,
            target: spell(target_path, target)?,
        })
    }
}

/// Install the prepared filesystem fence after new user, network and mount
/// namespaces, with private tmpfs/devpts grants bound after mounting. Setup errors
/// return RETRY_DENY_ALL before exec; the caller sets the returned flag and
/// spawns a fresh child using both fallback mechanisms. Fence installation
/// errors refuse exec entirely. `binds` are attached last, after the private
/// mounts and account files; one that cannot be attached exactly is a setup
/// error like any other, and the fallback child has no namespace, so it sees
/// no binding at all.
/// `deny_inherited_fds` must also be installed by the confined spawn funnel.
pub fn confine(
    command: &mut Command,
    filesystem: OwnedFd,
    fallback_rules: OwnedFd,
    deny: seccompiler::BpfProgram,
    loopback: seccompiler::BpfProgram,
    scratch_access: u64,
    binds: Vec<SameObjectBind>,
) -> io::Result<Arc<AtomicBool>> {
    let mut empty_dirs = Vec::new();
    for (name, value) in command.get_envs() {
        if !name
            .to_str()
            .is_some_and(|name| TEMPORARY_ENV.contains(&name))
        {
            continue;
        }
        let Some(path) = value.map(std::path::Path::new) else {
            continue;
        };
        let Some(root) = temporary_root(path) else {
            continue;
        };
        if path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        for parent in path.ancestors().take_while(|p| *p != root) {
            empty_dirs.push(
                CString::new(parent.as_os_str().as_bytes())
                    .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?,
            );
        }
    }
    // Lexical ordering puts each parent before its children. These directories
    // are newly created and empty; no host scratch contents are copied or bound.
    empty_dirs.sort();
    empty_dirs.dedup();
    let fallback = Arc::new(AtomicBool::new(false));
    let child_fallback = Arc::clone(&fallback);
    // Map to the same numeric identity so build tools do not suddenly run as
    // uid 0. Kernel credentials continue to name the original host identity.
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    // NSS may allocate, load modules or contact a service: resolve only here,
    // before fork. A failed lookup selects the same fresh-child fallback.
    let identity = account_files(uid, gid);
    let uid_map = format!("{uid} {uid} 1\n").into_bytes();
    let gid_map = format!("{gid} {gid} 1\n").into_bytes();
    // SAFETY: the hook uses only raw syscalls, stack storage and preallocated
    // rules/maps. Owned rule fds outlive the hook and remain CLOEXEC. No locks,
    // allocation, environment mutation or Rust stdio occur after fork.
    unsafe {
        command.pre_exec(move || {
            // The common fd guard closes >=3 at exec. Stdio is delegated and
            // survives, so refuse host terminals and network sockets there too.
            // AF_UNIX is needed for the authenticated worker-control channel.
            for fd in 0..3 {
                let mut terminal = std::mem::MaybeUninit::<libc::termios>::uninit();
                if libc::tcgetattr(fd, terminal.as_mut_ptr()) == 0 {
                    return Err(io::Error::from_raw_os_error(libc::EACCES));
                }
                let error = io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(libc::ENOTTY | libc::EBADF)) {
                    return Err(error);
                }
                let mut domain = 0;
                let mut size = std::mem::size_of_val(&domain) as libc::socklen_t;
                let result = libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_DOMAIN,
                    (&mut domain as *mut i32).cast(),
                    &mut size,
                );
                if result == 0 && domain != libc::AF_UNIX {
                    return Err(io::Error::from_raw_os_error(libc::EACCES));
                }
                if result < 0 {
                    let error = io::Error::last_os_error();
                    if !matches!(error.raw_os_error(), Some(libc::ENOTSOCK | libc::EBADF)) {
                        return Err(error);
                    }
                }
            }
            let private = !child_fallback.load(Ordering::Relaxed);
            if private
                && namespace(&uid_map, &gid_map)
                    .and_then(|()| {
                        private_filesystems(filesystem.as_raw_fd(), scratch_access, &empty_dirs)
                    })
                    .and_then(|()| {
                        let files = identity
                            .as_ref()
                            .map_err(|_| io::Error::from_raw_os_error(libc::EACCES))?;
                        private_identity(filesystem.as_raw_fd(), scratch_access, files)
                    })
                    .and_then(|()| binds.iter().try_for_each(bind_same_object))
                    .is_err()
            {
                // A partial user namespace cannot be undone. Exit before exec;
                // the parent retries with a fresh child under deny-all.
                return Err(io::Error::from_raw_os_error(RETRY_DENY_ALL));
            }
            let filter = if private { &loopback } else { &deny };
            seccompiler::apply_filter(filter)
                .map_err(|_| io::Error::from_raw_os_error(libc::EACCES))?;
            restrict(if private {
                filesystem.as_raw_fd()
            } else {
                fallback_rules.as_raw_fd()
            })?;
            let message = if private { PRIVATE } else { FALLBACK };
            libc::write(2, message.as_ptr().cast(), message.len());
            Ok(())
        });
    }
    Ok(fallback)
}

fn temporary_root(path: &std::path::Path) -> Option<&'static std::path::Path> {
    [
        std::path::Path::new("/tmp"),
        std::path::Path::new("/dev/shm"),
    ]
    .into_iter()
    .find(|root| path.starts_with(root))
}

fn account_files(uid: libc::uid_t, gid: libc::gid_t) -> io::Result<[Vec<u8>; 2]> {
    // Bound NSS scratch storage; an oversized entry is a setup failure, never
    // a reason to import the host databases or truncate an account field.
    let mut buffer = vec![0; 65536];
    let mut user: libc::passwd = unsafe { std::mem::zeroed() };
    let mut group: libc::group = unsafe { std::mem::zeroed() };
    let mut user_ptr = std::ptr::null_mut();
    let mut group_ptr = std::ptr::null_mut();
    // SAFETY: reentrant NSS writes into these live records and scratch storage.
    // The owned line is completed before the buffer is reused for the group.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut user,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut user_ptr,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    if user_ptr.is_null()
        || user.pw_uid != uid
        || user.pw_gid != gid
        || [user.pw_name, user.pw_dir, user.pw_shell]
            .iter()
            .any(|p| p.is_null())
    {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    let passwd = unsafe {
        account_line(&[
            CStr::from_ptr(user.pw_name).to_bytes(),
            b"x",
            uid.to_string().as_bytes(),
            gid.to_string().as_bytes(),
            b"",
            CStr::from_ptr(user.pw_dir).to_bytes(),
            CStr::from_ptr(user.pw_shell).to_bytes(),
        ])?
    };
    let rc = unsafe {
        libc::getgrgid_r(
            gid,
            &mut group,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut group_ptr,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    if group_ptr.is_null() || group.gr_gid != gid || group.gr_name.is_null() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    let group = unsafe {
        account_line(&[
            CStr::from_ptr(group.gr_name).to_bytes(),
            b"x",
            gid.to_string().as_bytes(),
            b"",
        ])?
    };
    Ok([passwd, group])
}

fn account_line(fields: &[&[u8]]) -> io::Result<Vec<u8>> {
    if fields.first().is_none_or(|name| name.is_empty())
        || fields.iter().any(|field| {
            field
                .iter()
                .any(|byte| matches!(byte, b':' | b'\n' | b'\r' | 0))
        })
    {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut line = fields.join(&b':');
    line.push(b'\n');
    Ok(line)
}

fn private_identity(ruleset: i32, access: u64, files: &[Vec<u8>; 2]) -> io::Result<()> {
    for ((source, target), bytes) in [
        (c"/tmp/.newt-build-passwd", c"/etc/passwd"),
        (c"/tmp/.newt-build-group", c"/etc/group"),
    ]
    .into_iter()
    .zip(files)
    {
        // The source exists only in the freshly mounted private tmpfs. No build
        // code runs until both readonly mounts and their object grants succeed.
        let fd = unsafe {
            libc::open(
                source.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        check(fd)?;
        let result = (|| {
            let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            if written != bytes.len() as isize {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            check(unsafe { libc::fchmod(fd, 0o444) })?;
            check(unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            })?;
            check(unsafe {
                libc::mount(
                    std::ptr::null(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND
                        | libc::MS_REMOUNT
                        | libc::MS_RDONLY
                        | libc::MS_NOSUID
                        | libc::MS_NODEV
                        | libc::MS_NOEXEC,
                    std::ptr::null(),
                )
            })?;
            // Remove the writable-mount alias before granting the readonly one.
            check(unsafe { libc::unlink(source.as_ptr()) })?;
            let read = access & u64::from(linux_raw_sys::landlock::LANDLOCK_ACCESS_FS_READ_FILE);
            if read != 0 {
                grant_private_mount(ruleset, read, target)?;
            }
            Ok(())
        })();
        unsafe { libc::close(fd) };
        result?;
    }
    Ok(())
}

fn private_filesystems(ruleset: i32, access: u64, empty_dirs: &[CString]) -> io::Result<()> {
    // SAFETY: only the new mount namespace is affected; propagation is made
    // private recursively BEFORE the first mount, so no event reaches the host.
    check(unsafe {
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    })?;
    for (path, options) in [
        // Four concurrent debug-executable copies exceeded 1 GiB in acceptance.
        (c"/tmp", c"size=2g,nr_inodes=65536,mode=0700"),
        (c"/dev/shm", c"size=256m,nr_inodes=16384,mode=0700"),
    ] {
        check(unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                path.as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                options.as_ptr().cast(),
            )
        })?;
        grant_private_mount(ruleset, access, path)?;
    }
    // A fresh instance has its own slave indices and allocation limit. It must
    // permit device opens (no MS_NODEV), but neither set-id nor executable files.
    check(unsafe {
        libc::mount(
            c"devpts".as_ptr(),
            c"/dev/pts".as_ptr(),
            c"devpts".as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            c"newinstance,ptmxmode=0666,mode=0600,max=64"
                .as_ptr()
                .cast(),
        )
    })?;
    check(unsafe {
        libc::mount(
            c"/dev/pts/ptmx".as_ptr(),
            c"/dev/ptmx".as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    })?;
    use linux_raw_sys::landlock::{
        LANDLOCK_ACCESS_FS_IOCTL_DEV, LANDLOCK_ACCESS_FS_READ_DIR, LANDLOCK_ACCESS_FS_READ_FILE,
        LANDLOCK_ACCESS_FS_WRITE_FILE,
    };
    // ABI 3/4 do not gate ioctls. If the prepared ruleset handles IOCTL_DEV
    // (ABI >=5), allow it only on the new instance and its ptmx bind alias.
    let terminal = access
        & u64::from(
            LANDLOCK_ACCESS_FS_READ_FILE
                | LANDLOCK_ACCESS_FS_WRITE_FILE
                | LANDLOCK_ACCESS_FS_IOCTL_DEV,
        );
    grant_private_mount(
        ruleset,
        terminal | (access & u64::from(LANDLOCK_ACCESS_FS_READ_DIR)),
        c"/dev/pts",
    )?;
    // Bind the source inode: /dev/ptmx can itself be a symlink on the host.
    grant_private_mount(ruleset, terminal, c"/dev/pts/ptmx")?;
    for path in empty_dirs {
        let rc = unsafe { libc::mkdir(path.as_ptr(), 0o700) };
        if rc < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
            check(rc)?;
        }
    }
    Ok(())
}

fn grant_private_mount(ruleset: i32, access: u64, path: &CStr) -> io::Result<()> {
    // SAFETY: the mount has succeeded in this child. The fd binds the private
    // object, never the host mount covered by it. The generated kernel ABI type
    // avoids a second handwritten Landlock layout. Close the fd on every path.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    check(fd)?;
    let rule = linux_raw_sys::landlock::landlock_path_beneath_attr {
        allowed_access: access,
        parent_fd: fd,
    };
    let result = check(unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset,
            linux_raw_sys::landlock::landlock_rule_type::LANDLOCK_RULE_PATH_BENEATH,
            &rule,
            0,
        )
    } as i32);
    unsafe { libc::close(fd) };
    result
}

fn restrict(fd: i32) -> io::Result<()> {
    // SAFETY: fd is a live, prepared Landlock ruleset; flags are zero.
    check(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0) } as i32)
}

fn check(result: i32) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_map(path: &std::ffi::CStr, bytes: &[u8]) -> io::Result<()> {
    // SAFETY: a static NUL-terminated path; the returned fd is local and closed
    // on every path. A short write is an error, never a successful mapping.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    check(fd)?;
    let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    let result = if written == bytes.len() as isize {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::EACCES))
    };
    unsafe { libc::close(fd) };
    result
}

fn namespace(uid: &[u8], gid: &[u8]) -> io::Result<()> {
    // Inspect the child's actual cwd after Command's chdir, including relative
    // paths and symlinks. Hiding a workspace would strand the build; fall back
    // rather than import host temporary content into a private mount.
    let mut cwd = [0; libc::PATH_MAX as usize];
    check(unsafe { libc::syscall(libc::SYS_getcwd, cwd.as_mut_ptr(), cwd.len()) } as i32)?;
    let cwd = unsafe { CStr::from_ptr(cwd.as_ptr()) };
    if temporary_root(std::path::Path::new(std::ffi::OsStr::from_bytes(
        cwd.to_bytes(),
    )))
    .is_some()
    {
        return Err(io::Error::from_raw_os_error(libc::ENOTSUP));
    }
    // SAFETY: only this fork child's namespaces change. Combined unshare must
    // succeed before any mapping or interface operation; no host interface can
    // ever be touched. A later error stays isolated and selects deny-all.
    check(unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWNS) })?;
    write_map(c"/proc/self/uid_map", uid)?;
    write_map(c"/proc/self/setgroups", b"deny")?;
    write_map(c"/proc/self/gid_map", gid)?;
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    check(fd)?;
    // SAFETY: zeroed ifreq has valid integer/union storage. Both ioctls use the
    // interface name and flags members; the socket belongs to the NEW netns.
    let mut interface: libc::ifreq = unsafe { std::mem::zeroed() };
    interface.ifr_name[0] = b'l' as libc::c_char;
    interface.ifr_name[1] = b'o' as libc::c_char;
    let result = (|| {
        check(unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS as libc::Ioctl, &mut interface) })?;
        unsafe { interface.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
        check(unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as libc::Ioctl, &interface) })
    })();
    unsafe { libc::close(fd) };
    result
}

/// Runs with the new namespace's capabilities, before exec drops them and
/// before Landlock freezes the mount topology. Descriptors inherited from the
/// parent sit on the host's mounts and can be neither cloned nor covered here,
/// so each spelling is reopened in this namespace and accepted only as the
/// held object. The mount is made detached, restricted, then attached by
/// descriptor. Every descriptor is owned and closes on every path.
fn bind_same_object(bind: &SameObjectBind) -> io::Result<()> {
    use linux_raw_sys::general as sys;
    let source = reopen(&bind.source.0, &bind.source.1)?;
    let target = reopen(&bind.target.0, &bind.target.1)?;
    // SAFETY: raw syscalls on live owned descriptors, static empty paths and a
    // stack attribute of the stated size. No allocation after fork.
    unsafe {
        let mount = libc::syscall(
            libc::SYS_open_tree,
            source.as_raw_fd(),
            c"".as_ptr(),
            sys::AT_EMPTY_PATH | sys::OPEN_TREE_CLONE | sys::OPEN_TREE_CLOEXEC,
        ) as i32;
        check(mount)?;
        let mount = OwnedFd::from_raw_fd(mount);
        let attributes = sys::mount_attr {
            attr_set: (sys::MOUNT_ATTR_RDONLY | sys::MOUNT_ATTR_NOSUID | sys::MOUNT_ATTR_NODEV)
                as u64,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        check(libc::syscall(
            libc::SYS_mount_setattr,
            mount.as_raw_fd(),
            c"".as_ptr(),
            sys::AT_EMPTY_PATH,
            &attributes,
            std::mem::size_of_val(&attributes),
        ) as i32)?;
        check(libc::syscall(
            libc::SYS_move_mount,
            mount.as_raw_fd(),
            c"".as_ptr(),
            target.as_raw_fd(),
            c"".as_ptr(),
            sys::MOVE_MOUNT_F_EMPTY_PATH | sys::MOVE_MOUNT_T_EMPTY_PATH,
        ) as i32)?;
    }
    // The second name must now reach the held source, or no program runs.
    reopen(&bind.target.0, &bind.source.1).map(drop)
}

/// Open `path` in the current mount namespace with every symlink refused, and
/// return it only if it is the regular file `held` already names. A held
/// descriptor pins its inode, so equal device and inode means the same object.
fn reopen(path: &CStr, held: &OwnedFd) -> io::Result<OwnedFd> {
    let how = crate::beneath::OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: libc::RESOLVE_NO_SYMLINKS,
    };
    let found = crate::beneath::openat2_fd(libc::AT_FDCWD, path, &how)?;
    let identity = |fd: &OwnedFd| {
        // SAFETY: zeroed stat is valid storage for fstat on a live descriptor.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) })?;
        Ok::<_, io::Error>((stat.st_dev, stat.st_ino, stat.st_mode & libc::S_IFMT))
    };
    let seen = identity(&found)?;
    if seen == identity(held)? && seen.2 == libc::S_IFREG {
        Ok(found)
    } else {
        Err(io::Error::from_raw_os_error(libc::ESTALE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_line_preserves_fields_without_adding_records() {
        assert_eq!(
            account_line(&[b"builder", b"x", b"1000", b"1000", b"", b"/build", b"/bin/sh"])
                .unwrap(),
            b"builder:x:1000:1000::/build:/bin/sh\n"
        );
        assert_eq!(
            account_line(&[b"builders", b"x", b"1000", b""]).unwrap(),
            b"builders:x:1000:\n"
        );
        assert_eq!(account_line(&[b"\xff", b""]).unwrap(), b"\xff:\n");
    }

    #[test]
    fn account_line_rejects_empty_names_and_injected_fields() {
        assert!(account_line(&[]).is_err());
        assert!(account_line(&[b"", b"x"]).is_err());
        for field in [b"other:0".as_slice(), b"\nother", b"\rother", b"\0other"] {
            assert!(account_line(&[b"builder", field]).is_err());
        }
    }

    /// Real files: the hook's only defence against a spelling that was
    /// redirected after the caller's check is that `reopen` compares objects.
    /// Grounded end to end by newt-core's `brush_build_pipeline` alias probes.
    #[test]
    fn a_spelling_is_accepted_only_as_the_held_regular_file() {
        let directory = std::env::temp_dir().join(format!("fdguard-3318-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let spell = |name: &str| {
            let path = directory.join(name);
            (CString::new(path.as_os_str().as_bytes()).unwrap(), path)
        };
        let (held_spelling, held_path) = spell("held");
        let (other_spelling, other_path) = spell("other");
        let (link_spelling, link_path) = spell("link");
        std::fs::write(&held_path, b"same bytes").unwrap();
        std::fs::write(&other_path, b"same bytes").unwrap();
        std::os::unix::fs::symlink(&held_path, &link_path).unwrap();
        let held = OwnedFd::from(std::fs::File::open(&held_path).unwrap());
        let folder = OwnedFd::from(std::fs::File::open(&directory).unwrap());
        let directory_spelling = CString::new(directory.as_os_str().as_bytes()).unwrap();

        let refused = |result: io::Result<OwnedFd>| result.unwrap_err().raw_os_error();
        assert!(reopen(&held_spelling, &held).is_ok());
        assert_eq!(refused(reopen(&other_spelling, &held)), Some(libc::ESTALE));
        assert_eq!(refused(reopen(&link_spelling, &held)), Some(libc::ELOOP));
        assert_eq!(
            refused(reopen(&directory_spelling, &folder)),
            Some(libc::ESTALE),
            "only a regular file is ever bound"
        );
        let relative = SameObjectBind::new(
            std::path::Path::new("held"),
            held.try_clone().unwrap(),
            &other_path,
            held,
        );
        assert_eq!(
            relative.err().and_then(|error| error.raw_os_error()),
            Some(libc::EINVAL)
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
