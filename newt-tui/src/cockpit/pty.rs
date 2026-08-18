//! Taking fd 1 and fd 2 away from the process — onto a pty, not a pipe.
//!
//! While the cockpit owns the terminal, everything the session prints must
//! reach the screen through the presenter, or a raw `println!` lands on the
//! editor. So for the session's lifetime fd 1/2 point at the slave side of a
//! pseudo-terminal this module opens, and the presenter reads the master.
//!
//! **A pty, not a pipe, and the distinction is load-bearing.** The process
//! asks `is_terminal()` about its own stdout in at least three places that
//! decide behaviour, not styling: `LineCaps::detect` (may a spinner paint at
//! all), the permission gate's `interactive` (default-DENY without asking when
//! false — §6.10), and the modal prompt's raw-mode path. On a pipe every one
//! of those flips: no spinner, permissions silently denied, prompts falling to
//! the line-reader path against a raw-mode stdin. On a pty slave they all keep
//! their answer, and the bytes still come to us. Children of `run_command`
//! that inherit fd 1 see a terminal too, exactly as they do today.
//!
//! **fd 2 is captured only when it is itself a terminal.** fd 1 is always the
//! presenter's to take — the cockpit opens only when stdout is a tty. But
//! stderr may have been redirected (`newt 2>log`, a pipe): swinging *that* onto
//! the pty would divert into the presenter the very bytes the operator pointed
//! at their file, and a child inheriting fd 2 would lose the redirection too.
//! So we redirect fd 2 only if `isatty(2)`; otherwise it is left exactly as it
//! was and stderr keeps flowing to its original destination.
//!
//! The real terminal survives as a `dup` of the original fd 1, which is the
//! ONLY writer the presenter uses. `Drop` puts fd 1 back — and fd 2 too when it
//! was captured — on the normal exit and on a panic, so the process never ends
//! with its stdout still pointed at a pty nobody is draining.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};

/// The capture: owns the master, the real terminal, and the restore.
pub(crate) struct PtyCapture {
    /// The pty master. Read for session output; `TIOCSWINSZ` on resize.
    master: File,
    /// The real terminal (a `dup` of the original fd 1). The presenter's one
    /// and only writer.
    tty: File,
    /// The original fd 2, restored on drop alongside fd 1 — but only present
    /// when fd 2 was itself a terminal and we redirected it onto the slave.
    /// `None` when stderr was pointed elsewhere and left untouched.
    saved_err: Option<RawFd>,
}

impl PtyCapture {
    /// Open a pty sized like the real terminal and swing fd 1/2 onto its
    /// slave. Fails closed: any error leaves the process's fds untouched.
    pub(crate) fn install(cols: u16, rows: u16) -> io::Result<Self> {
        // SAFETY: plain libc calls on descriptors we own; every failure path
        // closes what it opened and returns before touching fd 1/2.
        unsafe {
            let mut master: libc::c_int = -1;
            let mut slave: libc::c_int = -1;
            let ws = winsize(cols, rows);
            if libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &ws,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            // Output side raw: no ONLCR, so `\n` reaches us as `\n` and the
            // stream model sees exactly the bytes the session wrote. Input
            // side irrelevant — nobody reads the slave's stdin.
            let mut tio: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(slave, &mut tio) == 0 {
                tio.c_oflag &= !libc::OPOST;
                tio.c_lflag &= !libc::ECHO;
                let _ = libc::tcsetattr(slave, libc::TCSANOW, &tio);
            }
            // fd 1 is always ours (the cockpit opens only when stdout is a
            // terminal). fd 2 is captured ONLY when it is a terminal too:
            // hijacking a redirected stderr (`newt 2>log`, a pipe) onto the pty
            // would swallow the bytes the operator aimed at that destination.
            // When it is not a tty we never dup or dup2 it — it stays put.
            let capture_err = libc::isatty(2) == 1;
            let saved_out = libc::dup(1);
            let saved_err = if capture_err { libc::dup(2) } else { -1 };
            if saved_out < 0 || (capture_err && saved_err < 0) {
                let err = io::Error::last_os_error();
                libc::close(master);
                libc::close(slave);
                if saved_out >= 0 {
                    libc::close(saved_out);
                }
                if saved_err >= 0 {
                    libc::close(saved_err);
                }
                return Err(err);
            }
            // The saved terminal and the master must not leak into children.
            libc::fcntl(saved_out, libc::F_SETFD, libc::FD_CLOEXEC);
            if capture_err {
                libc::fcntl(saved_err, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
            if libc::dup2(slave, 1) < 0 {
                let err = io::Error::last_os_error();
                libc::dup2(saved_out, 1);
                libc::close(master);
                libc::close(slave);
                libc::close(saved_out);
                if saved_err >= 0 {
                    libc::close(saved_err);
                }
                return Err(err);
            }
            if capture_err && libc::dup2(slave, 2) < 0 {
                let err = io::Error::last_os_error();
                // Undo the fd 1 half we just landed, then fd 2.
                libc::dup2(saved_out, 1);
                libc::dup2(saved_err, 2);
                libc::close(master);
                libc::close(slave);
                libc::close(saved_out);
                libc::close(saved_err);
                return Err(err);
            }
            libc::close(slave);
            Ok(Self {
                master: File::from_raw_fd(master),
                tty: File::from_raw_fd(saved_out),
                saved_err: capture_err.then_some(saved_err),
            })
        }
    }

    /// The real terminal — the presenter's only writer.
    pub(crate) fn tty(&self) -> &File {
        &self.tty
    }

    /// The pty master, for `poll` + `read`.
    pub(crate) fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// Read whatever the session has written since the last call. Non-
    /// blocking in effect: call after `poll` reports the master readable.
    /// `Ok(0)` means nothing (a spurious wake), never EOF — the slave stays
    /// open for as long as fd 1/2 point at it.
    pub(crate) fn read_available(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: reading into a buffer we own, on a descriptor we own.
        let n = unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(0),
                // EIO on a pty master means the slave went away — impossible
                // while we hold fd 1/2 on it, so treat it as "nothing" too.
                _ if err.raw_os_error() == Some(libc::EIO) => Ok(0),
                _ => Err(err),
            };
        }
        Ok(n as usize)
    }

    /// Tell the pty the terminal's new size, so a child's `ioctl(1,
    /// TIOCGWINSZ)` — and anything else that sizes by fd 1 — sees the truth.
    pub(crate) fn resize(&self, cols: u16, rows: u16) {
        let ws = winsize(cols, rows);
        // SAFETY: ioctl on our own master with a properly-sized struct.
        unsafe {
            let _ = libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }
}

impl Drop for PtyCapture {
    fn drop(&mut self) {
        // SAFETY: restoring the descriptors this capture displaced. `tty`
        // (saved_out) is closed by its own File drop after this body. fd 2 is
        // put back only when it was captured; a redirected stderr was never
        // touched, so there is nothing to restore.
        unsafe {
            libc::dup2(self.tty.as_raw_fd(), 1);
            if let Some(saved_err) = self.saved_err {
                libc::dup2(saved_err, 2);
                libc::close(saved_err);
            }
        }
    }
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inode(fd: RawFd) -> u64 {
        // SAFETY: fstat on a live descriptor into a zeroed struct.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            assert_eq!(libc::fstat(fd, &mut st), 0);
            st.st_ino
        }
    }

    fn write_fd(fd: RawFd, bytes: &[u8]) {
        // SAFETY: writing bytes we own to a descriptor we own for the test.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n as usize, bytes.len());
    }

    fn drain(cap: &PtyCapture) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        for _ in 0..50 {
            let mut pfd = libc::pollfd {
                fd: cap.master_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll on one descriptor we own.
            let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
            if ready <= 0 {
                if out.is_empty() {
                    continue;
                }
                break;
            }
            let n = cap.read_available(&mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    /// Point `fd` at `target` for the length of a test, putting the original
    /// back on drop — lets a test choose whether stderr is a terminal or a pipe.
    struct RedirectFd {
        fd: RawFd,
        saved: RawFd,
    }

    impl RedirectFd {
        fn to(fd: RawFd, target: RawFd) -> Self {
            // SAFETY: dup/dup2 on descriptors the test owns; restored in Drop.
            unsafe {
                let saved = libc::dup(fd);
                assert!(saved >= 0, "dup of fd {fd}");
                assert!(libc::dup2(target, fd) >= 0, "dup2 onto fd {fd}");
                Self { fd, saved }
            }
        }
    }

    impl Drop for RedirectFd {
        fn drop(&mut self) {
            // SAFETY: putting the descriptor back and closing the save.
            unsafe {
                libc::dup2(self.saved, self.fd);
                libc::close(self.saved);
            }
        }
    }

    /// A bare pty pair the test opens to make some fd a terminal.
    struct TestPty {
        master: RawFd,
        slave: RawFd,
    }

    impl TestPty {
        fn open() -> Self {
            // SAFETY: openpty into locals we own; closed in Drop.
            unsafe {
                let (mut master, mut slave) = (-1, -1);
                assert_eq!(
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                    ),
                    0,
                    "openpty for the test's own terminal"
                );
                Self { master, slave }
            }
        }
    }

    impl Drop for TestPty {
        fn drop(&mut self) {
            // SAFETY: closing the pair we opened.
            unsafe {
                libc::close(self.master);
                libc::close(self.slave);
            }
        }
    }

    /// Ground truth for the whole cockpit: after `install`, a write to FD 1
    /// (not `println!`, which the test harness captures) comes out of the
    /// master byte-for-byte — no ONLCR translation — and fd 1/2 are put back
    /// on drop. fd 2 is made a terminal of the test's own so the capture branch
    /// is exercised whatever the harness did with stderr. Real pty, real
    /// descriptors: the property is about the process's own fds and no mock can
    /// stand in for that. Serial because fd 1 is process-global.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn install_captures_fd1_and_a_terminal_fd2_verbatim_and_drop_restores_them() {
        let err_pty = TestPty::open();
        let _err = RedirectFd::to(2, err_pty.slave);
        // SAFETY: isatty on fd 2, now our pty slave.
        assert_eq!(
            unsafe { libc::isatty(2) },
            1,
            "precondition: stderr is a tty"
        );
        let before_out = inode(1);
        let before_err = inode(2);
        {
            let cap = PtyCapture::install(80, 24).expect("openpty");
            assert_ne!(inode(1), before_out, "fd 1 was swung onto the pty");
            assert_eq!(inode(1), inode(2), "fd 1 and fd 2 share the slave");
            // SAFETY: isatty on fd 1.
            assert_eq!(unsafe { libc::isatty(1) }, 1, "the slave IS a terminal");
            write_fd(1, b"out\n");
            write_fd(2, b"\x1b[31merr\x1b[0m\n");
            let got = drain(&cap);
            let got = String::from_utf8_lossy(&got);
            assert!(got.contains("out\n"), "verbatim, no \\r\\n: {got:?}");
            assert!(got.contains("\x1b[31merr\x1b[0m\n"), "stderr too: {got:?}");
        }
        assert_eq!(inode(1), before_out, "fd 1 restored on drop");
        assert_eq!(inode(2), before_err, "fd 2 restored on drop");
    }

    /// #6 regression: a redirected stderr (`newt 2>log`) must NOT be hijacked
    /// onto the pty. When fd 2 is not a terminal, `install` leaves it exactly
    /// where it was, so stderr bytes still reach that destination (a pipe here)
    /// and never leak into the presenter's scrollback. Grounds the mock belief
    /// that the cockpit preserves fd-2 redirection topology.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn install_leaves_a_redirected_stderr_untouched() {
        // A pipe stands in for `2>log`: a non-terminal stderr.
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe() fills a 2-int array we own.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe()");
        let (pipe_r, pipe_w) = (fds[0], fds[1]);
        let _err = RedirectFd::to(2, pipe_w);
        // SAFETY: isatty on fd 2, now the pipe write end.
        assert_eq!(
            unsafe { libc::isatty(2) },
            0,
            "precondition: stderr is a pipe"
        );
        let before_err = inode(2);
        {
            let cap = PtyCapture::install(80, 24).expect("openpty");
            // SAFETY: isatty on fd 1.
            assert_eq!(unsafe { libc::isatty(1) }, 1, "fd 1 is still captured");
            assert_eq!(
                inode(2),
                before_err,
                "the redirected stderr was left in place"
            );
            assert_ne!(
                inode(1),
                inode(2),
                "fd 2 did not join fd 1 on the pty slave"
            );
            // A write to fd 2 reaches the pipe, not the presenter's master.
            write_fd(2, b"to the file\n");
            let mut buf = [0u8; 64];
            // SAFETY: read from the pipe's read end into a buffer we own.
            let n = unsafe { libc::read(pipe_r, buf.as_mut_ptr().cast(), buf.len()) };
            assert!(n > 0, "the stderr byte should have reached the pipe");
            assert_eq!(&buf[..n as usize], b"to the file\n", "verbatim to the pipe");
            // A stdout marker proves the master is alive and lets `drain` return
            // promptly; the stderr bytes must be absent from it.
            write_fd(1, b"stdout-marker\n");
            let got = drain(&cap);
            let got = String::from_utf8_lossy(&got);
            assert!(
                got.contains("stdout-marker"),
                "stdout still captured: {got:?}"
            );
            assert!(
                !got.contains("to the file"),
                "redirected stderr must not leak to the pty: {got:?}"
            );
        }
        assert_eq!(
            inode(2),
            before_err,
            "fd 2 never moved — nothing to restore"
        );
        // SAFETY: closing the pipe ends the test opened.
        unsafe {
            libc::close(pipe_r);
            libc::close(pipe_w);
        }
    }

    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_slave_reports_the_size_it_was_given_and_follows_resize() {
        let cap = PtyCapture::install(100, 30).expect("openpty");
        let size = |fd: RawFd| {
            // SAFETY: TIOCGWINSZ into a zeroed winsize.
            unsafe {
                let mut ws: libc::winsize = std::mem::zeroed();
                assert_eq!(libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws), 0);
                (ws.ws_col, ws.ws_row)
            }
        };
        assert_eq!(size(1), (100, 30));
        cap.resize(120, 40);
        assert_eq!(size(1), (120, 40), "a child sizing by fd 1 sees the resize");
    }

    /// Acceptance (#1744): entering and leaving the cockpit repeatedly must
    /// restore fd 1/2 EVERY time, not just on the first cycle — a session can
    /// open the cockpit, fall back to the classic surface, and re-enter.
    ///
    /// Deliberately does NOT assert a descriptor count. `/proc/self/fd` is
    /// process-global, so in a parallel suite another test opening files reads
    /// as a cockpit leak (observed: 45 -> 138 with nothing leaked). A guard
    /// that fails for unrelated reasons trains people to ignore it.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn repeated_enter_and_leave_cycles_restore_every_time_and_leak_nothing() {
        let err_pty = TestPty::open();
        let _err = RedirectFd::to(2, err_pty.slave);
        let before_out = inode(1);
        let before_err = inode(2);

        for cycle in 0..8 {
            {
                let cap = PtyCapture::install(80, 24).expect("openpty");
                assert_ne!(inode(1), before_out, "cycle {cycle}: fd 1 took the pty");
                write_fd(1, b"tick\n");
                let got = drain(&cap);
                assert!(
                    String::from_utf8_lossy(&got).contains("tick"),
                    "cycle {cycle}: the capture still carries output"
                );
            }
            assert_eq!(inode(1), before_out, "cycle {cycle}: fd 1 restored");
            assert_eq!(inode(2), before_err, "cycle {cycle}: fd 2 restored");
        }
    }

    /// Acceptance (#1744): an unwind while the cockpit owns the terminal must
    /// still hand fd 1/2 back. Terminal restoration is a safety property — a
    /// process that dies holding the real stdout leaves the operator with a
    /// terminal that echoes nothing.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_panic_while_the_cockpit_owns_the_terminal_still_restores_it() {
        let err_pty = TestPty::open();
        let _err = RedirectFd::to(2, err_pty.slave);
        let before_out = inode(1);
        let before_err = inode(2);

        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // keep the test log readable
        let result = std::panic::catch_unwind(|| {
            let _cap = PtyCapture::install(80, 24).expect("openpty");
            assert_ne!(inode(1), before_out, "precondition: the pty is installed");
            panic!("turn exploded while the cockpit held the terminal");
        });
        std::panic::set_hook(hook);

        assert!(
            result.is_err(),
            "the panic must propagate, not be swallowed"
        );
        assert_eq!(inode(1), before_out, "fd 1 restored through the unwind");
        assert_eq!(inode(2), before_err, "fd 2 restored through the unwind");
    }

    /// Acceptance (#1744): a burst larger than the pty's kernel buffer must not
    /// deadlock the writer. This is the architecture's load-bearing assumption
    /// — the presenter drains the master on its own thread — so the test models
    /// exactly that: a concurrent drainer, and a write that would block without
    /// one. Bounded by a join timeout so a regression FAILS rather than hangs.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_burst_larger_than_the_pty_buffer_does_not_deadlock_the_writer() {
        let cap = PtyCapture::install(80, 24).expect("openpty");
        let master = cap.master_fd();

        // 512 KiB — comfortably past any pty buffer (typically 4-64 KiB).
        const CHUNK: usize = 4096;
        const CHUNKS: usize = 128;
        let want = CHUNK * CHUNKS;

        let reader = std::thread::spawn(move || {
            let mut seen = 0usize;
            let mut buf = [0u8; 8192];
            while seen < want {
                // SAFETY: reading into our own buffer from a descriptor we own.
                let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
                seen += n as usize;
            }
            seen
        });

        let line = vec![b'x'; CHUNK];
        for _ in 0..CHUNKS {
            write_fd(1, &line);
        }
        let seen = reader.join().expect("drainer thread");
        assert!(
            seen >= want,
            "the drainer saw {seen} of {want} bytes — the writer starved or the pty dropped output"
        );
        drop(cap);
    }
}
