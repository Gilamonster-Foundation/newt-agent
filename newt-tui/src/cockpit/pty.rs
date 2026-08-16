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
//! The real terminal survives as a `dup` of the original fd 1, which is the
//! ONLY writer the presenter uses. `Drop` puts fd 1/2 back — on the normal
//! exit and on a panic — so the process never ends with its stdout still
//! pointed at a pty nobody is draining.

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
    /// The original fd 2, restored on drop alongside fd 1.
    saved_err: RawFd,
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
            let saved_out = libc::dup(1);
            let saved_err = libc::dup(2);
            if saved_out < 0 || saved_err < 0 {
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
            libc::fcntl(saved_err, libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
            if libc::dup2(slave, 1) < 0 || libc::dup2(slave, 2) < 0 {
                let err = io::Error::last_os_error();
                // Undo whichever half landed.
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
                saved_err,
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
        // (saved_out) is closed by its own File drop after this body.
        unsafe {
            libc::dup2(self.tty.as_raw_fd(), 1);
            libc::dup2(self.saved_err, 2);
            libc::close(self.saved_err);
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

    /// Ground truth for the whole cockpit: after `install`, a write to FD 1
    /// (not `println!`, which the test harness captures) comes out of the
    /// master byte-for-byte — no ONLCR translation — and fd 1/2 are put back
    /// on drop. Real pty, real descriptors: the property is about the
    /// process's own fds and no mock can stand in for that. Serial because
    /// fd 1 is process-global.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn install_captures_fd1_and_fd2_verbatim_and_drop_restores_them() {
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
}
