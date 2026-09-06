use super::*;

/// mark_fds_cloexec must never touch stdin/stdout/stderr.
///
/// Serialized on `tty_arbiter`: the cockpit pty tests `dup2` the pty slave
/// onto fd 1 (and a terminal fd 2) and restore them in `Drop`. Reading the
/// process fd table during that window sees fd 1 mid-swap and reports it
/// closed. Rust runs a crate's tests in ONE process, so this is a real
/// shared resource, not an isolated one.
#[serial_test::serial(tty_arbiter)]
#[test]
fn mark_fds_cloexec_preserves_stdio() {
    mark_fds_cloexec();
    // fds 0-2 must remain open and CLOEXEC-free.
    for fd in 0..3i32 {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(
            flags >= 0,
            "stdio fd {fd} must remain open after mark_fds_cloexec"
        );
        assert_eq!(
            flags & libc::FD_CLOEXEC,
            0,
            "stdio fd {fd} must not have FD_CLOEXEC set"
        );
    }
}

/// A freshly-opened fd that lacks CLOEXEC gets the flag set by mark_fds_cloexec.
///
/// Same `tty_arbiter` key: this walks the whole fd table, so it must not run
/// while another test is opening and closing pty descriptors.
#[serial_test::serial(tty_arbiter)]
#[test]
fn mark_fds_cloexec_sets_flag_on_new_fd() {
    let f = std::fs::File::open("/dev/null").expect("open /dev/null");
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);

    // Ensure CLOEXEC is NOT set initially (it may or may not be,
    // depending on the std implementation; clear it to be sure).
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
    }
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0,
        "pre-condition: CLOEXEC should be clear"
    );

    mark_fds_cloexec();

    let flags_after = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert_ne!(
        flags_after & libc::FD_CLOEXEC,
        0,
        "mark_fds_cloexec must set FD_CLOEXEC on an open fd (fd={fd})"
    );
}

/// Under normal conditions there is always at least one free fd slot.
#[test]
fn terminal_fd_available_true_normally() {
    assert!(
        terminal_fd_available(),
        "fd table should have free slots normally"
    );
}

/// If the fd-table probe cannot open the null device, terminal_fd_available
/// returns false. Do not exhaust the real process fd table here: Rust runs
/// tests in one process, so a real EMFILE window can starve unrelated tests.
#[test]
fn terminal_fd_available_false_when_probe_open_fails() {
    assert!(
        !terminal_fd_available_from_probe(|| {
            Err(std::io::Error::from_raw_os_error(libc::EMFILE))
        }),
        "terminal_fd_available must return false when the probe cannot open"
    );
    assert!(
        terminal_fd_available(),
        "fd table should still have free slots after the synthetic failure"
    );
}

/// mark_fds_cloexec is idempotent — calling it twice changes nothing.
///
/// Same `tty_arbiter` key, for the same fd-table reason.
#[serial_test::serial(tty_arbiter)]
#[test]
fn mark_fds_cloexec_is_idempotent() {
    let f = std::fs::File::open("/dev/null").expect("open /dev/null");
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);
    mark_fds_cloexec();
    let flags_first = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    mark_fds_cloexec();
    let flags_second = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert_eq!(
        flags_first, flags_second,
        "second call must not change fd flags"
    );
}
