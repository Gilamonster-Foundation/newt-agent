//! A real pty standing in for the operator's terminal, for cockpit tests.
//!
//! The cockpit's interesting properties are the ones a unit test cannot see:
//! it takes fd 0/1, puts the terminal in raw mode, and must give all of that
//! back. So these tests need a terminal they own — not a mock — and they must
//! never touch the developer's real one.
//!
//! [`TestTty::install`] puts a pty slave on **fd 0 and fd 1**. Both matter:
//! crossterm resolves its terminal as "stdin if it is a tty, else `/dev/tty`",
//! so without fd 0 the raw-mode calls would target the developer's terminal.
//!
//! `Presenter::open` also asks the terminal where the cursor is (`ESC[6n`) and
//! blocks for the reply. Nothing is on the other end of a test pty, so
//! [`TestTty`] runs the answering half of a terminal: one thread owns the
//! master, accumulates everything painted, and replies to a cursor query the
//! moment it appears.
//!
//! Pre-loading the reply instead does NOT work, and the failure is worth
//! recording: bytes written before `open` land in the slave's canonical line
//! discipline, which holds them for a newline that never comes, so crossterm
//! switches to raw and times out with "the cursor position could not be read
//! within a normal duration". The reply has to arrive while the application is
//! actually waiting for it.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) struct TestTty {
    master: RawFd,
    saved_in: RawFd,
    saved_out: RawFd,
    painted: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    responder: Option<std::thread::JoinHandle<()>>,
}

impl TestTty {
    /// Take fd 0 and fd 1 onto a fresh pty slave.
    pub(crate) fn install() -> Self {
        // SAFETY: openpty + dup/dup2 on descriptors this test owns; every one
        // is restored or closed in Drop.
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
                "openpty for the test's terminal"
            );
            let saved_in = libc::dup(0);
            let saved_out = libc::dup(1);
            assert!(libc::dup2(slave, 0) >= 0, "dup2 onto fd 0");
            assert!(libc::dup2(slave, 1) >= 0, "dup2 onto fd 1");
            libc::close(slave);
            assert_eq!(libc::isatty(0), 1, "fd 0 must be a tty for crossterm");

            // Clear crossterm's process-global raw-mode static before handing
            // this pty over. It caches the FIRST terminal's saved mode, so a
            // second `enable_raw_mode` in the same process is a no-op — the
            // very hazard #1770 fixed in the modal. Left set, the cockpit's
            // `open` would find the new pty still canonical and its cursor
            // query would time out, failing every test after the first.
            let _ = crossterm::terminal::disable_raw_mode();

            let painted = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let responder = {
                let painted = Arc::clone(&painted);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || answer_terminal_queries(master, &painted, &stop))
            };
            Self {
                master,
                saved_in,
                saved_out,
                painted,
                stop,
                responder: Some(responder),
            }
        }
    }

    /// Feed bytes to the application as if typed.
    pub(crate) fn type_bytes(&self, bytes: &[u8]) {
        // SAFETY: writing bytes we own to the master we own.
        let n = unsafe { libc::write(self.master, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n as usize, bytes.len(), "write to pty master");
    }

    /// Everything the application has painted so far.
    pub(crate) fn painted(&self) -> String {
        let buf = self
            .painted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// The answering half of a terminal: accumulate what the application paints,
/// and reply to a cursor-position query (`ESC[6n`) as a real terminal would.
fn answer_terminal_queries(master: RawFd, painted: &Mutex<Vec<u8>>, stop: &AtomicBool) {
    let mut buf = [0u8; 4096];
    let mut pending = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll on the master this thread owns for its lifetime.
        if unsafe { libc::poll(&mut pfd, 1, 20) } <= 0 {
            continue;
        }
        // SAFETY: reading into our own buffer.
        let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break; // slave closed
        }
        let chunk = &buf[..n as usize];
        pending.extend_from_slice(chunk);
        painted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(chunk);

        // Answer every cursor query in what we have seen, then forget the
        // scanned prefix so a query split across reads is still matched.
        while let Some(at) = find(&pending, b"\x1b[6n") {
            // SAFETY: writing our reply to the master.
            let reply = b"\x1b[1;1R";
            unsafe { libc::write(master, reply.as_ptr().cast(), reply.len()) };
            pending.drain(..at + 4);
        }
        if pending.len() > 8 {
            let keep = pending.len() - 8; // a query cannot straddle more than this
            pending.drain(..keep);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

impl Drop for TestTty {
    fn drop(&mut self) {
        // Leave the static as we found it, for whatever runs next.
        let _ = crossterm::terminal::disable_raw_mode();
        // Stop and join the responder BEFORE closing the master it polls.
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.responder.take() {
            let _ = h.join();
        }
        // SAFETY: putting fd 0/1 back and closing what we opened.
        unsafe {
            libc::dup2(self.saved_in, 0);
            libc::dup2(self.saved_out, 1);
            libc::close(self.saved_in);
            libc::close(self.saved_out);
            libc::close(self.master);
        }
    }
}

/// The full termios of `fd`, so a test can compare exact restoration rather
/// than the control bytes an implementation happened to emit.
pub(crate) fn termios_of(fd: RawFd) -> libc::termios {
    // SAFETY: tcgetattr into a zeroed struct on a live descriptor.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(fd, &mut t), 0, "tcgetattr fd {fd}");
        t
    }
}

/// Put `fd` in a known canonical, echoing state — the mode a shell hands over.
pub(crate) fn set_canonical_echo(fd: RawFd) {
    // SAFETY: termios round-trip on a descriptor the test owns.
    unsafe {
        let mut t = termios_of(fd);
        t.c_lflag |= libc::ICANON | libc::ECHO;
        assert_eq!(libc::tcsetattr(fd, libc::TCSANOW, &t), 0, "tcsetattr");
    }
}

pub(crate) fn is_canonical(fd: RawFd) -> bool {
    termios_of(fd).c_lflag & libc::ICANON != 0
}

pub(crate) fn echoes(fd: RawFd) -> bool {
    termios_of(fd).c_lflag & libc::ECHO != 0
}

/// Compare the fields that decide how a terminal behaves. `termios` is not
/// `PartialEq`, and padding/speed fields are not the contract.
pub(crate) fn modes_equal(a: &libc::termios, b: &libc::termios) -> bool {
    a.c_lflag == b.c_lflag
        && a.c_iflag == b.c_iflag
        && a.c_oflag == b.c_oflag
        && a.c_cflag == b.c_cflag
        && a.c_cc == b.c_cc
}
