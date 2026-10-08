use super::*;

/// #2813: publishing a private commit must reject a moved HEAD and must use
/// the held admin directory even if its pathname is replaced by another actor.
#[test]
fn private_commit_head_publication_is_cas_and_descriptor_bound() {
    let temp = tempfile::tempdir().unwrap();
    let admin = temp.path().canonicalize().unwrap().join("admin");
    std::fs::create_dir(&admin).unwrap();
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    std::fs::write(admin.join("HEAD"), format!("{old}\n")).unwrap();
    let held = agent_bridle_fdguard::GrantedRoot::acquire(&admin).unwrap();
    publish_detached_head(&held, &old, &new, "verified first\n").unwrap();
    assert!(publish_detached_head(&held, &old, &old, "must not append\n").is_err());
    assert_eq!(
        std::fs::read_to_string(admin.join("HEAD")).unwrap().trim(),
        new
    );
    assert!(!admin.join("HEAD.lock").exists());
    assert_eq!(
        std::fs::read_to_string(admin.join("logs/HEAD")).unwrap(),
        "verified first\n"
    );
    let moved = temp.path().join("held");
    std::fs::rename(&admin, &moved).unwrap();
    std::fs::create_dir(&admin).unwrap();
    std::fs::write(admin.join("HEAD"), "decoy").unwrap();
    publish_detached_head(&held, &new, &old, "verified second\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(moved.join("HEAD")).unwrap().trim(),
        old
    );
    assert_eq!(
        std::fs::read_to_string(admin.join("HEAD")).unwrap(),
        "decoy"
    );
    assert!(!admin.join("logs").exists());
    assert_eq!(
        std::fs::read_to_string(moved.join("logs/HEAD")).unwrap(),
        "verified first\nverified second\n"
    );
}

/// #2813: HEAD-log failure must leave HEAD unchanged and never append through
/// a child-planted link into a shared/sibling log outside the held admin root.
#[test]
fn private_commit_reflog_refuses_escape_before_publishing_head() {
    let temp = tempfile::tempdir().unwrap();
    let admin = temp.path().canonicalize().unwrap().join("admin");
    std::fs::create_dir_all(admin.join("logs")).unwrap();
    let outside = temp.path().join("outside-log");
    std::fs::write(&outside, "untouched\n").unwrap();
    std::os::unix::fs::symlink(&outside, admin.join("logs/HEAD")).unwrap();
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    std::fs::write(admin.join("HEAD"), &old).unwrap();
    let held = agent_bridle_fdguard::GrantedRoot::acquire(&admin).unwrap();
    let error = publish_detached_head(&held, &old, &new, "verified entry\n").unwrap_err();
    assert!(
        error.contains("cannot append worktree HEAD reflog"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(admin.join("HEAD")).unwrap(), old);
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "untouched\n");
    assert!(!admin.join("HEAD.lock").exists());
}

/// #2813 / PR #2818 round 4: a child-writable FIFO must not block the host
/// holding HEAD.lock. Grounds the regular-file append policy in a real FIFO.
/// A timeout opens a rescue reader before joining, so even the old blocking
/// open produces a bounded assertion failure rather than stranding a thread.
#[test]
fn private_commit_reflog_refuses_fifo_without_blocking() {
    let temp = tempfile::tempdir().unwrap();
    let admin = temp.path().canonicalize().unwrap().join("admin");
    std::fs::create_dir_all(admin.join("logs")).unwrap();
    let fifo = admin.join("logs/HEAD");
    make_reflog_fifo(&fifo);
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    std::fs::write(admin.join("HEAD"), &old).unwrap();
    let held = agent_bridle_fdguard::GrantedRoot::acquire(&admin).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = publish_detached_head(&held, &"1".repeat(40), &new, "verified entry\n");
        tx.send(result).unwrap();
    });
    let timely = rx.recv_timeout(std::time::Duration::from_secs(10));
    // The rescue reader is nonblocking and stays alive until the writer exits.
    // The entry fits in the pipe buffer; the old path then fails at fsync.
    let rescue = timely.is_err().then(|| {
        rustix::fs::open(
            &fifo,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .unwrap()
    });
    let result = match timely {
        Ok(result) => result,
        Err(_) => rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
    };
    worker.join().unwrap();
    assert!(result.is_err(), "FIFO must refuse publication");
    assert_eq!(std::fs::read_to_string(admin.join("HEAD")).unwrap(), old);
    assert!(!admin.join("HEAD.lock").exists());
    assert!(
        rescue.is_none(),
        "host reflog open blocked until the rescue reader was opened"
    );
}

/// #2813: an existing regular reflog is appended, never truncated. This also
/// exercises the regular-file check that a reader-present FIFO would bypass
/// if NONBLOCK were the only protection.
#[test]
fn private_commit_reflog_accepts_regular_file_but_never_writes_fifo() {
    use std::io::Read as _;
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("logs")).unwrap();
    let log = temp.path().join("logs/HEAD");
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    let held =
        agent_bridle_fdguard::GrantedRoot::acquire(&temp.path().canonicalize().unwrap()).unwrap();
    std::fs::write(temp.path().join("HEAD"), &old).unwrap();
    std::fs::write(&log, "existing\n").unwrap();
    publish_detached_head(&held, &old, &new, "verified entry\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "existing\nverified entry\n"
    );
    std::fs::remove_file(&log).unwrap();
    make_reflog_fifo(&log);
    let mut reader = std::fs::File::from(
        rustix::fs::open(
            &log,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .unwrap(),
    );
    assert!(publish_detached_head(&held, &new, &old, "must not reach pipe\n").is_err());
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert!(
        bytes.is_empty(),
        "nonregular reflog was written before refusal: {bytes:?}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("HEAD"))
            .unwrap()
            .trim(),
        new
    );
    assert!(!temp.path().join("HEAD.lock").exists());
}

fn make_reflog_fifo(path: &Path) {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: live NUL-terminated path; mkfifo retains no pointer.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}
