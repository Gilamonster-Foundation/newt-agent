//! Descriptor-guard behavior retained from the former copy's differential suite.
//! Literal expectations cover refusal classification, identity encoding and relative roots;
//! real-resource grounds compare bounded opens with ordinary filesystem observations and
//! guarded children with an unguarded control. There is only one implementation under test.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use agent_bridle_fdguard as guard;

/// Every errno from 0 to 4095, plus errors without an OS code, pins the exact refusal set.
#[test]
fn refusal_classification_agrees_on_every_errno() {
    let refused: BTreeSet<_> = (0..=4095)
        .filter(|&errno| guard::is_resolution_refusal(&io::Error::from_raw_os_error(errno)))
        .collect();
    assert_eq!(
        refused,
        BTreeSet::from([libc::ELOOP, libc::EXDEV, libc::EMLINK, libc::ENOTDIR])
    );
    for error in [
        io::Error::from(io::ErrorKind::NotFound),
        io::Error::new(io::ErrorKind::InvalidInput, "no os code"),
    ] {
        assert!(!guard::is_resolution_refusal(&error), "{error}");
    }
}

/// Device and inode occupy separate little-endian words, including boundary values.
#[test]
fn identity_encoding_agrees() {
    assert_eq!(
        guard::RootIdentity {
            device: 1,
            inode: 2
        }
        .to_bytes(),
        [1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]
    );
    let values: [u64; 4] = [0, 1, 0x0102_0304_0506_0708, u64::MAX];
    for device in values {
        for inode in values {
            let identity = guard::RootIdentity { device, inode };
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&device.to_le_bytes());
            bytes[8..].copy_from_slice(&inode.to_le_bytes());
            assert_eq!(identity.to_bytes(), bytes);
            assert_eq!(identity.to_string(), format!("dev{device}:ino{inode}"));
            assert_eq!(
                format!("{identity:?}"),
                format!("RootIdentity {{ device: {device}, inode: {inode} }}")
            );
        }
    }
}

/// Relative roots are refused before any system call.
#[test]
fn a_relative_root_is_refused_alike() {
    let error = guard::GrantedRoot::acquire(Path::new("relative/root")).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "GrantedRoot::acquire requires an absolute canonical root, got \"relative/root\" (a relative root resolves through the CWD — ambient authority)");
}

/// The two grounds below both place descriptors; one at a time.
static FD_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    FD_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Independent metadata and contents of an opened object; failures cannot count as success.
fn read(mut file: File) -> (u64, u64, Option<Vec<u8>>) {
    let meta = file.metadata().expect("fstat the opened object");
    let bytes = if meta.is_file() {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read the opened file");
        Some(bytes)
    } else {
        None
    };
    (meta.dev(), meta.ino(), bytes)
}

fn stat(file: &File) -> (u64, u64) {
    let meta = file.metadata().expect("fstat");
    (meta.dev(), meta.ino())
}

/// A fixture tree under the temp directory, canonical (macOS keeps it behind the `/var`
/// symlink), removed when dropped.
struct Tree {
    base: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let name = format!("fdguard-differential-{}", std::process::id());
        let base = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create the fixture base");
        let base = base.canonicalize().expect("canonicalize the fixture base");
        let root = base.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::create_dir_all(base.join("real/inner")).unwrap();
        std::fs::write(root.join("file.txt"), b"file").unwrap();
        std::fs::write(root.join("sub/nested.txt"), b"nested").unwrap();
        std::fs::write(root.join("afile"), b"not a directory").unwrap();
        std::fs::write(base.join("outside/victim.txt"), b"outside").unwrap();
        std::os::unix::fs::symlink(root.join("file.txt"), root.join("link_to_file")).unwrap();
        std::os::unix::fs::symlink(root.join("sub"), root.join("link_dir")).unwrap();
        std::os::unix::fs::symlink(base.join("outside"), root.join("escape")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("swapped")).unwrap();
        Self { base }
    }

    fn root(&self) -> PathBuf {
        self.base.join("root")
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Grounds the literal refusal and identity tests against real filesystem objects. Every
/// former differential row remains: successful opens match independent metadata/content,
/// while each refused row pins an ordinary, resolution, or up-front refusal.
#[test]
#[ignore = "real filesystem: run with --ignored --test-threads=1"]
fn acquire_and_bounded_opens_agree_on_a_real_tree() {
    let _lock = lock();
    let tree = Tree::new();
    let root = tree.root();
    let held = guard::GrantedRoot::acquire(&root).unwrap();
    let metadata = std::fs::metadata(&root).unwrap();
    let identity = guard::RootIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    assert_eq!(held.identity(), identity);
    assert_eq!(held.provenance(), root);
    assert_eq!(held.try_clone().unwrap().identity(), identity);
    assert_eq!(
        stat(&File::from(held.as_fd().try_clone_to_owned().unwrap())),
        (metadata.dev(), metadata.ino())
    );

    for (path, expected) in [
        (root.join("missing"), Some(libc::ENOENT)),
        (root.join("file.txt"), Some(libc::ENOTDIR)),
        (root.join("link_dir"), None),
        (tree.base.join("swapped/inner"), None),
    ] {
        let error = guard::GrantedRoot::acquire(&path).unwrap_err();
        if let Some(errno) = expected {
            assert_eq!(error.raw_os_error(), Some(errno), "{path:?}");
        } else {
            assert!(guard::is_resolution_refusal(&error), "{path:?}: {error}");
        }
    }
    assert_eq!(
        guard::GrantedRoot::acquire(Path::new("/fdguard-differential\0nul"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );

    for rel in ["file.txt", "sub/nested.txt", "sub", ".", ""] {
        assert_eq!(
            read(held.open_read(Path::new(rel)).unwrap()),
            read(File::open(root.join(rel)).unwrap()),
            "{rel}"
        );
    }
    for rel in [
        "sub/../file.txt",
        "../outside/victim.txt",
        "/fdguard-differential-absolute",
        "nul\0byte",
    ] {
        assert_eq!(
            held.open_read(Path::new(rel)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "{rel:?}"
        );
    }
    for rel in [
        "link_to_file",
        "link_dir/nested.txt",
        "escape/victim.txt",
        "afile/inner",
    ] {
        assert!(
            guard::is_resolution_refusal(&held.open_read(Path::new(rel)).unwrap_err()),
            "{rel}"
        );
    }
    assert_eq!(
        held.open_read(Path::new("missing.txt"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOENT)
    );

    for (rel, append) in [("", false), ("../escaped.txt", false)] {
        assert_eq!(
            held.open_write(Path::new(rel), append).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    for (rel, append) in [("link_to_file", false), ("escape/victim.txt", true)] {
        assert!(guard::is_resolution_refusal(
            &held.open_write(Path::new(rel), append).unwrap_err()
        ));
    }
    for (path, directory) in [(root.clone(), true), (root.join("file.txt"), false)] {
        let result = guard::GrantedRoot::from_owned_fd(File::open(&path).unwrap().into(), &path);
        if directory {
            assert_eq!(result.unwrap().identity(), identity);
        } else {
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOTDIR));
        }
    }

    let shared = Path::new("written.txt");
    for (append, bytes, expected) in [
        (false, &b"first"[..], &b"first"[..]),
        (true, &b"+second"[..], &b"first+second"[..]),
        (false, &b"t"[..], &b"t"[..]),
    ] {
        let mut file = held.open_write(shared, append).unwrap();
        file.write_all(bytes).unwrap();
        drop(file);
        let (_, _, got) = read(held.open_read(shared).unwrap());
        assert_eq!(got.as_deref(), Some(expected));
        assert_eq!(std::fs::read(root.join(shared)).unwrap(), expected);
    }
}

/// Descriptor number the re-run child reports on.
const PROBE_ENV: &str = "NEWT_FDGUARD_DIFFERENTIAL_PROBE";

/// NOT A TEST: the child entrypoint `deny_inherited_fds_matches_the_unguarded_control` re-runs. With
/// `PROBE_ENV` set it reports whether that descriptor is open in its own table.
#[test]
#[ignore = "child entrypoint for deny_inherited_fds_matches_the_unguarded_control, not a test"]
fn differential_child_entrypoint_not_a_test() {
    let Ok(target) = std::env::var(PROBE_ENV) else {
        return;
    };
    let fd: libc::c_int = target.parse().expect("probe target");
    // SAFETY: `fcntl(F_GETFD)` is defined for any integer; it reports EBADF for a closed one.
    let open = unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
    println!("\nFDPROBE {fd} {}", if open { "open" } else { "closed" });
    println!("PROBE_DONE");
}

#[derive(Clone, Copy, Debug)]
enum Guard {
    Unguarded,
    Guarded,
}

fn install(cmd: &mut Command, guard: Guard) {
    match guard {
        Guard::Unguarded => {}
        Guard::Guarded => guard::deny_inherited_fds(cmd),
    }
}

/// Re-run this binary on the entrypoint under `guard`; `true` if the child saw `fd` open.
fn child_sees(fd: libc::c_int, guard: Guard) -> bool {
    let mut cmd = Command::new(std::env::current_exe().expect("current_exe"));
    cmd.args([
        "--exact",
        "differential_child_entrypoint_not_a_test",
        "--include-ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(PROBE_ENV, fd.to_string())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    install(&mut cmd, guard);
    let out = cmd.output().expect("run the probe child");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("PROBE_DONE"),
        "{guard:?}: the child did not report:\n{text}"
    );
    text.contains(&format!("FDPROBE {fd} open"))
}

/// An inheritable duplicate of `file`, placed 32 above every descriptor open now so that the
/// child's own start-up cannot take its number.
fn place_ambient(file: &File) -> libc::c_int {
    let highest = std::fs::read_dir("/dev/fd")
        .expect("/dev/fd")
        .filter_map(|entry| {
            entry
                .ok()?
                .file_name()
                .to_str()?
                .parse::<libc::c_int>()
                .ok()
        })
        .max()
        .unwrap_or(2);
    let target = highest + 32;
    // SAFETY: `fcntl` and `dup2` on descriptor numbers; the target is checked free first, and
    // `dup2` leaves the new descriptor inheritable.
    unsafe {
        assert_eq!(
            libc::fcntl(target, libc::F_GETFD),
            -1,
            "fd {target} must be free"
        );
        assert!(
            libc::dup2(file.as_raw_fd(), target) >= 0,
            "dup2 to {target}"
        );
        assert_eq!(libc::fcntl(target, libc::F_GETFD) & libc::FD_CLOEXEC, 0);
    }
    target
}

/// Grounds the guard's refusal contract with a real child: an unguarded control inherits
/// the descriptor, a guarded child does not, and failed exec still reports NotFound.
#[test]
#[ignore = "real child process: run with --ignored --test-threads=1"]
fn deny_inherited_fds_matches_the_unguarded_control() {
    let _lock = lock();
    let file = File::open("/dev/null").expect("open /dev/null");
    let fd = place_ambient(&file);
    let unguarded = child_sees(fd, Guard::Unguarded);
    let guarded = child_sees(fd, Guard::Guarded);
    // SAFETY: closing the descriptor this test placed.
    unsafe {
        libc::close(fd);
    }
    assert!(unguarded, "control: an unguarded child must see fd {fd}");
    assert!(!guarded, "the guard left fd {fd} open in the child");
    let mut cmd = Command::new("/nonexistent/fdguard-differential-missing");
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    install(&mut cmd, Guard::Guarded);
    assert_eq!(cmd.spawn().unwrap_err().kind(), io::ErrorKind::NotFound);
}
