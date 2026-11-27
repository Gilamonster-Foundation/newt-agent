//! Child-side private loopback, scratch and PTYs, with the existing floor on error.
//! Allocation happens before fork. Private-mount rules bind descriptors opened
//! after mounting, before either fence. No host or parent namespace is changed.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
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

pub const PRIVATE: &str = "Build isolation: private loopback only; private tmpfs /tmp (2 GiB) and /dev/shm (256 MiB); private devpts (64 PTYs); external networking unavailable.\n";
pub const FALLBACK: &str = "Build isolation: deny-all fallback; loopback, private scratch and private PTYs unavailable. Local tests may fail because of this fence.\n";

/// Install the prepared filesystem fence after new user, network and mount
/// namespaces, with private tmpfs/devpts grants bound after mounting. Setup errors
/// return RETRY_DENY_ALL before exec; the caller sets the returned flag and
/// spawns a fresh child using both fallback mechanisms. Fence installation
/// errors refuse exec entirely.
/// `deny_inherited_fds` must also be installed by the confined spawn funnel.
pub fn confine(
    command: &mut Command,
    filesystem: OwnedFd,
    fallback_rules: OwnedFd,
    deny: seccompiler::BpfProgram,
    loopback: seccompiler::BpfProgram,
    scratch_access: u64,
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
    // root, never the host mount covered by it. The generated kernel ABI type
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
