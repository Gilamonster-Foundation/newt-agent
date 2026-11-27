//! Real kernel grounding for the same-object Git alias (#3318).
//!
//! A confined build binds the verified system Git over its own same-image
//! alias, so a nested fence that grants only that Git also covers the alias
//! name. Three kernel behaviours carry this, and no mock can observe them: a
//! Landlock file rule follows the object through a file bind; a byte-identical
//! copy at the alias is a different object and stays refused; and the mount
//! topology cannot be changed once a filesystem fence is installed.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

// linux/mount.h and linux/landlock.h values this libc does not name yet.
const OPEN_TREE_CLONE: libc::c_uint = 1;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x4;
const MOVE_MOUNT_T_EMPTY_PATH: libc::c_uint = 0x40;
const MOUNT_ATTR_RDONLY: u64 = 0x1;
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 0x1;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

/// The system Git and the alias it re-executes, by the two packaging layouts
/// the production resolver knows. Identity is left to each caller: on the host
/// the two are distinct objects, inside a private build they are one.
pub fn pair() -> Option<(PathBuf, PathBuf)> {
    ["/usr/bin/git", "/bin/git", "/usr/sbin/git", "/sbin/git"]
        .into_iter()
        .filter_map(|git| std::fs::canonicalize(git).ok())
        .find_map(|primary| {
            let prefix = primary.parent()?.parent()?;
            ["lib/git-core/git", "libexec/git-core/git"]
                .into_iter()
                .filter_map(|alias| std::fs::canonicalize(prefix.join(alias)).ok())
                .find(|alias| *alias != primary)
                .map(|alias| (primary, alias))
        })
}

pub fn object(path: &Path) -> (u64, u64) {
    let metadata = std::fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

fn handle(path: &Path) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(path)
        .unwrap_or_else(|error| panic!("open {}: {error}", path.display()))
}

fn errno(result: libc::c_long) -> String {
    if result < 0 {
        std::io::Error::last_os_error()
            .raw_os_error()
            .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
    } else {
        "ok".to_owned()
    }
}

/// A detached file bind of `source`, attached over `target`.
fn bind(source: &Path, target: &Path, read_only: bool) -> libc::c_long {
    let (source, target) = (handle(source), handle(target));
    // SAFETY: live descriptors and NUL-terminated empty paths; the detached
    // mount descriptor is owned and closed on every path.
    unsafe {
        let mount = libc::syscall(
            libc::SYS_open_tree,
            source.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH as libc::c_uint | OPEN_TREE_CLONE | libc::O_CLOEXEC as libc::c_uint,
        );
        if mount < 0 {
            return mount;
        }
        let mount = OwnedFd::from_raw_fd(mount as i32);
        if read_only {
            let attributes = [MOUNT_ATTR_RDONLY, 0, 0, 0];
            let result = libc::syscall(
                libc::SYS_mount_setattr,
                mount.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                attributes.as_ptr(),
                std::mem::size_of_val(&attributes),
            );
            if result < 0 {
                return result;
            }
        }
        libc::syscall(
            libc::SYS_move_mount,
            mount.as_raw_fd(),
            c"".as_ptr(),
            target.as_raw_fd(),
            c"".as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
        )
    }
}

/// A nested Landlock domain whose only `Execute` rules name `primary` and the
/// dynamic loader. No rule names the alias path or any directory.
fn fence_granting_only(primary: &Path) {
    #[repr(C, packed)]
    struct PathBeneath {
        allowed_access: u64,
        parent_fd: i32,
    }
    // SAFETY: the kernel copies each live attribute during its syscall. The
    // ruleset descriptor is owned; rule descriptors close after each rule.
    unsafe {
        let handled = LANDLOCK_ACCESS_FS_EXECUTE;
        let ruleset = libc::syscall(libc::SYS_landlock_create_ruleset, &handled, 8usize, 0);
        assert!(ruleset >= 0, "{}", std::io::Error::last_os_error());
        let ruleset = OwnedFd::from_raw_fd(ruleset as i32);
        let loaders = ["/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1"]
            .into_iter()
            .filter_map(|loader| std::fs::canonicalize(loader).ok());
        for path in std::iter::once(primary.to_path_buf()).chain(loaders) {
            let file = handle(&path);
            let rule = PathBeneath {
                allowed_access: LANDLOCK_ACCESS_FS_EXECUTE,
                parent_fd: file.as_raw_fd(),
            };
            assert_eq!(
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset.as_raw_fd(),
                    LANDLOCK_RULE_PATH_BENEATH,
                    &rule,
                    0,
                ),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
        }
        assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
        assert_eq!(
            libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
    }
}

/// Every way this process could change what the alias path names. Each result
/// is printed for the parent to judge; `attribute` is the one call Landlock
/// does not mediate, so it is refused only without namespace capabilities.
fn topology_attempts(stage: &str, primary: &Path, alias: &Path) -> [String; 4] {
    let target = std::ffi::CString::new(alias.to_str().unwrap()).unwrap();
    let source = std::ffi::CString::new(primary.to_str().unwrap()).unwrap();
    // SAFETY: live C strings and null optional operands. Any call that did
    // succeed would affect only this process's private mount namespace.
    let (unmount, mount, attribute) = unsafe {
        let unmount = libc::umount2(target.as_ptr(), libc::MNT_DETACH) as libc::c_long;
        let unmount = errno(unmount);
        let mount = errno(libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        ) as libc::c_long);
        let attributes = [0, MOUNT_ATTR_RDONLY, 0, 0];
        let attribute = errno(libc::syscall(
            libc::SYS_mount_setattr,
            handle(alias).as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            attributes.as_ptr(),
            std::mem::size_of_val(&attributes),
        ));
        (unmount, mount, attribute)
    };
    let moved = errno(bind(primary, alias, false));
    println!("GIT_ALIAS_TOPOLOGY {stage}: mount={mount} move_mount={moved} umount={unmount} attribute={attribute}");
    [mount, moved, unmount, attribute]
}

fn same_object(primary: &Path, alias: &Path) -> bool {
    object(primary) == object(alias)
}

/// `--git-alias-ground=<none|copy|bind>`: this process is its own build. It
/// enters a caller-mapped user and mount namespace as the production hook
/// does, optionally binds, fences, then replaces itself through the ALIAS name.
pub fn ground_child(mode: &str) -> ! {
    let (primary, alias) = pair().expect("the parent found a Git alias pair");
    // SAFETY: single-threaded at the top of main. Only this process enters the
    // namespaces; propagation is private before the first mount.
    unsafe {
        let (uid, gid) = (libc::geteuid(), libc::getegid());
        assert_eq!(
            libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n")).unwrap();
        std::fs::write("/proc/self/setgroups", "deny").unwrap();
        std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n")).unwrap();
        assert_eq!(
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ),
            0
        );
    }
    let source = match mode {
        "none" => None,
        "bind" => Some(primary.clone()),
        "copy" => Some(PathBuf::from(std::env::args().nth(2).unwrap())),
        other => panic!("unknown grounding mode {other}"),
    };
    if let Some(source) = source {
        let bound = bind(&source, &alias, true);
        assert_eq!(bound, 0, "file bind: {}", std::io::Error::last_os_error());
    }
    println!(
        "GIT_ALIAS_OWNER_SEEN_IN_NAMESPACE: {}",
        std::fs::metadata(&primary).unwrap().uid()
    );
    println!("GIT_ALIAS_IS_PRIMARY: {}", same_object(&primary, &alias));
    fence_granting_only(&primary);
    topology_attempts("fenced", &primary, &alias);
    println!(
        "GIT_ALIAS_IS_PRIMARY_AFTER_ATTEMPTS: {}",
        same_object(&primary, &alias)
    );
    let error = std::process::Command::new(&alias).arg("--version").exec();
    println!("GIT_ALIAS_EXEC_REFUSED: {:?}", error.raw_os_error());
    std::process::exit(0);
}

/// Parent of [`ground_child`]. Grounds, before any production wiring is
/// trusted, the three kernel facts in the module comment; the mocked resolver
/// tests in the sandbox's `git_exec_path_tests` rely on the first two.
pub fn ground(root: &Path) {
    let name = "git_alias_file_bind_carries_a_nested_exec_rule";
    let Some((primary, alias)) = pair().filter(|(primary, alias)| !same_object(primary, alias))
    else {
        eprintln!("test {name} ... skipped (no separately installed Git alias on this host)");
        return;
    };
    assert_eq!(
        std::fs::read(&primary).unwrap(),
        std::fs::read(&alias).unwrap(),
        "the alias must be the same image for this grounding to mean anything"
    );
    let before = (object(&primary), object(&alias));
    let copy = root.join("git-alias-copy");
    std::fs::copy(&primary, &copy).unwrap();
    let refused = format!("GIT_ALIAS_EXEC_REFUSED: Some({})", libc::EACCES);
    // Nothing is mounted at the alias in `none`, so there the kernel answers
    // the unmount with EINVAL before any fence is consulted.
    let immutable = |unmount| {
        format!(
            "GIT_ALIAS_TOPOLOGY fenced: mount={0} move_mount={0} umount={unmount} ",
            libc::EPERM
        )
    };
    for (mode, is_primary, unmount, outcome) in [
        ("none", false, libc::EINVAL, refused.as_str()),
        ("copy", false, libc::EPERM, refused.as_str()),
        ("bind", true, libc::EPERM, "git version "),
    ] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg(format!("--git-alias-ground={mode}"))
            .arg(&copy)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("--- {name} [{mode}]\n{stdout}{stderr}");
        assert!(output.status.success(), "{mode}: {stderr}");
        for expected in [
            format!("GIT_ALIAS_IS_PRIMARY: {is_primary}"),
            format!("GIT_ALIAS_IS_PRIMARY_AFTER_ATTEMPTS: {is_primary}"),
            immutable(unmount),
            outcome.to_owned(),
        ] {
            assert!(stdout.contains(&expected), "{mode}: missing {expected:?}");
        }
    }
    assert_eq!(before, (object(&primary), object(&alias)), "host changed");
    eprintln!("test {name} ... ok");
}

/// `--build-git-alias-probe`: runs INSIDE a confined build on the production
/// routes, after exec. Grounds the prepared binding end to end: the alias is
/// the granted primary object, a nested fence naming only the primary runs it
/// through the alias name, and no later namespace can change what that name
/// reaches. In the deny-all fallback there is no binding and none is claimed.
pub fn route_child() {
    let Some((primary, alias)) = pair() else {
        println!("BUILD_GIT_ALIAS_IS_THE_GRANTED_PRIMARY (no Git alias installed)");
        return;
    };
    if std::net::TcpListener::bind(("127.0.0.1", 0)).is_err() {
        println!("BUILD_GIT_ALIAS_UNBOUND_FALLBACK");
        return;
    }
    assert!(
        same_object(&primary, &alias),
        "a private build must see {} as the object at {}",
        alias.display(),
        primary.display()
    );
    fence_granting_only(&primary);
    let refused = libc::EPERM.to_string();
    assert_eq!(
        topology_attempts("build", &primary, &alias),
        [(); 4].map(|()| refused.clone())
    );
    let output = std::process::Command::new(&alias)
        .arg("--version")
        .output()
        .unwrap();
    assert!(
        output.status.success() && output.stdout.starts_with(b"git version "),
        "{output:?}"
    );
    // SAFETY: this probe is single-threaded; only it enters the new namespaces.
    // A descendant's own namespaces inherit the fence and locked mounts.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } == 0 {
        let nested = topology_attempts("nested namespace", &primary, &alias);
        assert!(nested.iter().all(|result| result != "ok"), "{nested:?}");
    } else {
        println!(
            "GIT_ALIAS_TOPOLOGY nested namespace: unavailable ({})",
            std::io::Error::last_os_error()
        );
    }
    assert!(same_object(&primary, &alias));
    println!("BUILD_GIT_ALIAS_IS_THE_GRANTED_PRIMARY");
}
