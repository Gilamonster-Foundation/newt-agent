//! While agent-bridle-fdguard is still a dependency (#3160), newt-fdguard must behave as the
//! crate it copies: the same refusal classification, the same identity encoding, and, in the
//! two ignored grounds, the same outcome for each acquire and bounded open of a real tree and
//! the same descriptor table in a guarded child. tests/vendor_identity.rs compares the bytes;
//! this file compares behaviour, so it would see a difference the bytes cannot show, such as
//! a different cfg arm chosen in this crate. A later #3160 step may delete this file with the
//! dev-dependency; the identity test stays.
//!
//! The grounds use a real filesystem and a real child process, so they stay out of the per-PR
//! tier. Run them on Linux and on macOS:
//! `cargo test -p newt-fdguard --test bridle_differential -- --ignored --test-threads=1`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use agent_bridle_fdguard as theirs;
use newt_fdguard as ours;

/// Every errno from 0 to 4095, and errors that carry none, classify alike. Both sides must
/// refuse exactly the four resolution errnos, so two functions that always answer `false`
/// cannot agree here.
#[test]
fn refusal_classification_agrees_on_every_errno() {
    let mut refused = BTreeSet::new();
    for errno in 0..=4095 {
        let error = io::Error::from_raw_os_error(errno);
        let answer = ours::is_resolution_refusal(&error);
        assert_eq!(
            answer,
            theirs::is_resolution_refusal(&error),
            "errno {errno}"
        );
        if answer {
            refused.insert(errno);
        }
    }
    let expected = BTreeSet::from([libc::ELOOP, libc::EXDEV, libc::EMLINK, libc::ENOTDIR]);
    assert_eq!(refused, expected);
    for error in [
        io::Error::from(io::ErrorKind::NotFound),
        io::Error::new(io::ErrorKind::InvalidInput, "no os code"),
    ] {
        assert!(!ours::is_resolution_refusal(&error), "{error}");
        assert!(!theirs::is_resolution_refusal(&error), "{error}");
    }
}

/// `RootIdentity` encodes, displays and debug-prints alike, and the encoding is the documented
/// one: the device, then the inode, each little-endian.
#[test]
fn identity_encoding_agrees() {
    let values = [0, 1, 0x0102_0304_0506_0708, u64::MAX];
    for device in values {
        for inode in values {
            let a = ours::RootIdentity { device, inode };
            let b = theirs::RootIdentity { device, inode };
            assert_eq!(a.to_bytes(), b.to_bytes(), "device {device}, inode {inode}");
            assert_eq!(a.to_string(), b.to_string());
            assert_eq!(format!("{a:?}"), format!("{b:?}"));
        }
    }
    let pinned = ours::RootIdentity {
        device: 1,
        inode: 2,
    };
    let bytes = [1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(pinned.to_bytes(), bytes);
    assert_eq!(pinned.to_string(), "dev1:ino2");
}

/// A relative root is refused before any system call, with the same kind and message.
#[test]
fn a_relative_root_is_refused_alike() {
    let path = Path::new("relative/root");
    let a = ours::GrantedRoot::acquire(path).expect_err("ours refuses a relative root");
    let b = theirs::GrantedRoot::acquire(path).expect_err("bridle refuses a relative root");
    assert_eq!(a.kind(), io::ErrorKind::InvalidInput);
    assert_eq!((a.kind(), a.to_string()), (b.kind(), b.to_string()));
}

/// The two grounds below both place descriptors; one at a time.
static FD_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    FD_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// What one call gave: the object it opened (its identity and, for a file read, its bytes), or
/// how it was refused.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Opened {
        device: u64,
        inode: u64,
        bytes: Option<Vec<u8>>,
    },
    Refused {
        kind: io::ErrorKind,
        errno: Option<i32>,
    },
}

fn refused(error: &io::Error) -> Outcome {
    Outcome::Refused {
        kind: error.kind(),
        errno: error.raw_os_error(),
    }
}

/// The outcome of a read open; a regular file is read to its end.
fn read(result: io::Result<File>) -> Outcome {
    match result {
        Ok(mut file) => {
            let meta = file.metadata().expect("fstat the opened object");
            let mut bytes = Vec::new();
            if meta.is_file() {
                file.read_to_end(&mut bytes).expect("read the opened file");
            }
            Outcome::Opened {
                device: meta.dev(),
                inode: meta.ino(),
                bytes: meta.is_file().then_some(bytes),
            }
        }
        Err(error) => refused(&error),
    }
}

/// The outcome of a write open or of a root acquired by either crate, by identity.
fn object(result: io::Result<(u64, u64)>) -> Outcome {
    match result {
        Ok((device, inode)) => Outcome::Opened {
            device,
            inode,
            bytes: None,
        },
        Err(error) => refused(&error),
    }
}

fn stat(file: &File) -> (u64, u64) {
    let meta = file.metadata().expect("fstat");
    (meta.dev(), meta.ino())
}

fn ours_root(path: &Path) -> io::Result<(u64, u64)> {
    ours::GrantedRoot::acquire(path).map(|r| (r.identity().device, r.identity().inode))
}

fn theirs_root(path: &Path) -> io::Result<(u64, u64)> {
    theirs::GrantedRoot::acquire(path).map(|r| (r.identity().device, r.identity().inode))
}

/// A fixture tree under the temp directory, canonical (macOS keeps it behind the `/var`
/// symlink), removed when dropped.
struct Tree {
    base: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let name = format!("newt-fdguard-differential-{}", std::process::id());
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

/// Both crates give the same outcome, object for object and errno for errno, for every acquire
/// and bounded open of one real tree, and a file written through one reads back through the
/// other. The table must reach at least 5 opened and 8 refused rows on each side, with an
/// ordinary failure, a resolution refusal and an up-front refusal among them, so an all-open
/// or all-refused table cannot agree vacuously. Grounds `vendor_identity`: equal bytes give
/// equal behaviour when built into this crate.
#[test]
#[ignore = "real filesystem: run with --ignored --test-threads=1"]
fn acquire_and_bounded_opens_agree_on_a_real_tree() {
    let _lock = lock();
    let tree = Tree::new();
    let root = tree.root();
    let mut rows: Vec<(String, Outcome, Outcome)> = Vec::new();

    for path in [
        root.clone(),
        root.join("missing"),
        root.join("file.txt"),
        root.join("link_dir"),
        tree.base.join("swapped/inner"),
        PathBuf::from("/fdguard-differential\0nul"),
    ] {
        let name = format!("acquire {}", path.display());
        rows.push((name, object(ours_root(&path)), object(theirs_root(&path))));
    }

    let a = ours::GrantedRoot::acquire(&root).expect("ours acquires the root");
    let b = theirs::GrantedRoot::acquire(&root).expect("bridle acquires the root");
    assert_eq!(a.identity().to_bytes(), b.identity().to_bytes());
    assert_eq!(a.provenance(), b.provenance());
    let a_fd = File::from(a.as_fd().try_clone_to_owned().unwrap());
    let b_fd = File::from(b.as_fd().try_clone_to_owned().unwrap());
    assert_eq!(stat(&a_fd), stat(&b_fd));
    assert_eq!(a.try_clone().unwrap().identity(), a.identity());
    assert_eq!(b.try_clone().unwrap().identity(), b.identity());

    for rel in [
        "file.txt",
        "sub/nested.txt",
        "sub",
        ".",
        "",
        "sub/../file.txt",
        "../outside/victim.txt",
        "/fdguard-differential-absolute",
        "link_to_file",
        "link_dir/nested.txt",
        "escape/victim.txt",
        "missing.txt",
        "afile/inner",
        "nul\0byte",
    ] {
        let rel = Path::new(rel);
        let name = format!("open_read {rel:?}");
        rows.push((name, read(a.open_read(rel)), read(b.open_read(rel))));
    }

    for (rel, append) in [
        ("", false),
        ("link_to_file", false),
        ("escape/victim.txt", true),
        ("../escaped.txt", false),
    ] {
        let rel = Path::new(rel);
        let name = format!("open_write {rel:?} append={append}");
        let ours_result = a.open_write(rel, append).map(|f| stat(&f));
        let theirs_result = b.open_write(rel, append).map(|f| stat(&f));
        rows.push((name, object(ours_result), object(theirs_result)));
    }

    for (name, adopt) in [
        ("a directory", root.clone()),
        ("a file", root.join("file.txt")),
    ] {
        let fd = || std::os::fd::OwnedFd::from(File::open(adopt.as_path()).unwrap());
        let ours_result = ours::GrantedRoot::from_owned_fd(fd(), adopt.as_path())
            .map(|r| (r.identity().device, r.identity().inode));
        let theirs_result = theirs::GrantedRoot::from_owned_fd(fd(), adopt.as_path())
            .map(|r| (r.identity().device, r.identity().inode));
        let name = format!("from_owned_fd {name}");
        rows.push((name, object(ours_result), object(theirs_result)));
    }

    // A write through one crate reads back through the other: create, append, truncate.
    let shared = Path::new("written.txt");
    for (writer, append, bytes, expected) in [
        ("ours", false, &b"ours"[..], &b"ours"[..]),
        ("bridle", true, &b"+bridle"[..], &b"ours+bridle"[..]),
        ("ours", false, &b"t"[..], &b"t"[..]),
    ] {
        let mut file = if writer == "ours" {
            a.open_write(shared, append)
                .expect("ours opens for writing")
        } else {
            b.open_write(shared, append)
                .expect("bridle opens for writing")
        };
        file.write_all(bytes).unwrap();
        drop(file);
        let reader = if writer == "ours" {
            b.open_read(shared)
        } else {
            a.open_read(shared)
        };
        let Outcome::Opened { bytes: got, .. } = read(reader) else {
            panic!("the other crate cannot read what {writer} wrote");
        };
        assert_eq!(
            got.as_deref(),
            Some(expected),
            "{writer} wrote, append={append}"
        );
    }

    for (name, ours_outcome, theirs_outcome) in &rows {
        assert_eq!(ours_outcome, theirs_outcome, "{name}");
    }
    let opened = rows
        .iter()
        .filter(|(_, o, _)| matches!(o, Outcome::Opened { .. }))
        .count();
    let errnos: BTreeSet<Option<i32>> = rows
        .iter()
        .filter_map(|(_, o, _)| match o {
            Outcome::Refused { errno, .. } => Some(*errno),
            Outcome::Opened { .. } => None,
        })
        .collect();
    let refusals = rows.len() - opened;
    assert!(
        opened >= 5 && refusals >= 8,
        "{opened} opened, {refusals} refused"
    );
    assert!(
        errnos.contains(&Some(libc::ENOENT)),
        "no ordinary failure: {errnos:?}"
    );
    assert!(errnos.contains(&None), "no up-front refusal: {errnos:?}");
    let resolution = |errno: &Option<i32>| {
        errno.is_some_and(|e| ours::is_resolution_refusal(&io::Error::from_raw_os_error(e)))
    };
    assert!(
        errnos.iter().any(resolution),
        "no resolution refusal: {errnos:?}"
    );
}

/// Descriptor number the re-run child reports on.
const PROBE_ENV: &str = "NEWT_FDGUARD_DIFFERENTIAL_PROBE";

/// NOT A TEST: the child entrypoint `deny_inherited_fds_agrees_with_bridles` re-runs. With
/// `PROBE_ENV` set it reports whether that descriptor is open in its own table.
#[test]
#[ignore = "child entrypoint for deny_inherited_fds_agrees_with_bridles, not a test"]
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
    Ours,
    Bridle,
}

fn install(cmd: &mut Command, guard: Guard) {
    match guard {
        Guard::Unguarded => {}
        Guard::Ours => ours::deny_inherited_fds(cmd),
        Guard::Bridle => theirs::deny_inherited_fds(cmd),
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

/// The same child sees an ambient descriptor open with no guard, which proves the probe can
/// see one, and closed under either crate's guard; a missing program is still reported as
/// `NotFound` under both.
#[test]
#[ignore = "real child process: run with --ignored --test-threads=1"]
fn deny_inherited_fds_agrees_with_bridles() {
    let _lock = lock();
    let file = File::open("/dev/null").expect("open /dev/null");
    let fd = place_ambient(&file);
    let unguarded = child_sees(fd, Guard::Unguarded);
    let ours_open = child_sees(fd, Guard::Ours);
    let bridle_open = child_sees(fd, Guard::Bridle);
    // SAFETY: closing the descriptor this test placed.
    unsafe {
        libc::close(fd);
    }
    assert!(unguarded, "control: an unguarded child must see fd {fd}");
    assert!(
        !bridle_open,
        "bridle's guard left fd {fd} open in the child"
    );
    assert!(
        !ours_open,
        "newt-fdguard's guard left fd {fd} open, where bridle's closes it"
    );

    for guard in [Guard::Ours, Guard::Bridle] {
        let mut cmd = Command::new("/nonexistent/newt-fdguard-differential-missing");
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        install(&mut cmd, guard);
        let error = cmd.spawn().expect_err("a missing program must not spawn");
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{guard:?}");
    }
}
