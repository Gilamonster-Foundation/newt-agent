//! One pty harness for the real-resource test tier.
//!
//! # Why this crate exists
//!
//! Per `CLAUDE.md`, the real-resource tier grounds the mocked tier: "the prompt
//! is visible" and "a notice damages nothing" are properties of an *actual*
//! terminal, and no mock can observe one writer scribbling over another's
//! bytes. Two such tests exist today — `newt_tui::prompt_visibility_test` and
//! `newt_core::tty::pty_notice_test` — and until #1410 each carried its own
//! byte-identical copy of the pty plumbing below.
//!
//! That is precisely the sprawl the repo's reuse discipline names: the terminal
//! code reached 5 spinners, 3 frame arrays, and 4 erase strategies one copy at
//! a time. A third copy was about to be added for the live-spill viewport's
//! regression proof, so the copies became a crate instead.
//!
//! # Scope
//!
//! Unix only — it needs a real pty pair. The crate is `libc`-only on purpose;
//! see the dependency note in `Cargo.toml` before adding anything.

#![cfg(unix)]

use std::os::unix::io::{FromRawFd, RawFd};

/// A pty pair: we hold the master and read what the terminal was shown, while
/// a child process runs against the slave believing it owns a real terminal.
pub struct Pty {
    master: RawFd,
    slave: RawFd,
}

impl Pty {
    /// Open a pty pair with a realistic geometry.
    ///
    /// The 50×200 winsize is load-bearing, not decoration: a 0×0 pty drives
    /// `term_cols()` to its 8-column floor, and both existing consumers then
    /// see their output truncated into something their assertions cannot
    /// recognize (a fitted label, a clipped spinner).
    pub fn open() -> Self {
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "posix_openpt failed");
            assert_eq!(libc::grantpt(master), 0, "grantpt failed");
            assert_eq!(libc::unlockpt(master), 0, "unlockpt failed");
            let name = libc::ptsname(master);
            assert!(!name.is_null(), "ptsname failed");
            let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
            assert!(slave >= 0, "opening the pty slave failed");

            let ws = libc::winsize {
                ws_row: 50,
                ws_col: 200,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::ioctl(slave, libc::TIOCSWINSZ, &ws);

            Self { master, slave }
        }
    }

    /// Send bytes as if the operator typed them.
    pub fn type_in(&self, s: &str) {
        let n = unsafe { libc::write(self.master, s.as_ptr().cast::<libc::c_void>(), s.len()) };
        assert!(n > 0, "writing the operator's keystrokes to the pty failed");
    }

    /// Everything the terminal has been shown so far.
    ///
    /// Switches the master to non-blocking and drains it, so this returns what
    /// is available rather than waiting for more. Call it after the child has
    /// exited (or after a deliberate settling delay) or the screen may be
    /// partial.
    pub fn screen(&self) -> String {
        unsafe {
            let flags = libc::fcntl(self.master, libc::F_GETFL);
            libc::fcntl(self.master, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = unsafe {
                libc::read(
                    self.master,
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A fresh duplicate of the slave as a `Stdio`, for handing to a child.
    ///
    /// One call per stream: `Stdio::from(File)` takes ownership and closes the
    /// fd on drop, so a child wanting both stdin and stdout on the pty needs
    /// two calls. Duplicating also keeps our own `slave` alive for `Drop`.
    pub fn slave_stdio(&self) -> std::process::Stdio {
        // SAFETY: `dup` returns a fresh owned descriptor for a slave fd this
        // struct keeps open for its whole lifetime, so the `File` is the sole
        // owner of the duplicate.
        let file = unsafe { std::fs::File::from_raw_fd(libc::dup(self.slave)) };
        std::process::Stdio::from(file)
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.slave);
            libc::close(self.master);
        }
    }
}
