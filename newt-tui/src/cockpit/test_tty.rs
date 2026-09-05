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
        // Start far enough down the screen that the cockpit acceptance test
        // has real transcript rows above its mounted chat block. Row/column
        // replies are 1-based, as required by the terminal protocol.
        Self::install_at(10, 1)
    }

    /// Take fd 0 and fd 1 onto a fresh pty slave and answer the presenter's
    /// initial cursor query with `row`, `col`.
    pub(crate) fn install_at(row: u16, col: u16) -> Self {
        assert!(row > 0 && col > 0, "cursor replies are one-based");
        // SAFETY: openpty + dup/dup2 on descriptors this test owns; every one
        // is restored or closed in Drop.
        unsafe {
            let (mut master, mut slave) = (-1, -1);
            let mut size = libc::winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut::<libc::termios>(),
                    std::ptr::from_mut(&mut size),
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
            let cursor_reply = format!("\x1b[{row};{col}R").into_bytes();
            let responder = {
                let painted = Arc::clone(&painted);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    answer_terminal_queries(master, &painted, &stop, &cursor_reply);
                })
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

    /// Type `bytes` only after `needle` has reached the emulated operator's
    /// screen. The fallback write after the deadline keeps a regression from
    /// deadlocking the test forever; the returned boolean still makes the
    /// missing-before-answer prompt fail loudly.
    pub(crate) fn type_when_painted(
        &self,
        needle: &str,
        bytes: &[u8],
    ) -> std::thread::JoinHandle<bool> {
        let master = self.master;
        let painted = Arc::clone(&self.painted);
        let needle = needle.as_bytes().to_vec();
        let bytes = bytes.to_vec();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let saw_prompt = loop {
                let saw = painted
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .windows(needle.len())
                    .any(|window| window == needle);
                if saw || std::time::Instant::now() >= deadline {
                    break saw;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            };
            // SAFETY: `master` remains owned by TestTty until the caller joins
            // this handle, and `bytes` is live for the duration of the write.
            let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
            assert_eq!(n as usize, bytes.len(), "write delayed terminal input");
            saw_prompt
        })
    }

    /// Block until `needle` has appeared in the painted screen AT OR AFTER
    /// byte offset `from`, or `timeout` elapses. Returns whether it was seen.
    ///
    /// The write side (`Screen::draw`/`write_all`+`flush`) completing does
    /// NOT mean the read side has caught up: `answer_terminal_queries` drains
    /// the pty master on its own thread, so a caller that calls [`painted`]
    /// immediately after a blocking `handle_request` returns is racing that
    /// thread, not synchronized with it — unlike input, where
    /// [`type_when_painted`] already closes exactly this race by waiting
    /// before it writes. This is the read-side twin: same bounded-poll
    /// technique, so a snapshot taken after a `true` return is safe to
    /// assert against in full.
    ///
    /// `from` matters and is not optional: several expected sequences (cursor
    /// Show, for one) legitimately occur earlier in the same session, so
    /// "appears anywhere" would return immediately without ever waiting for
    /// the LATER occurrence under test.
    pub(crate) fn wait_for_painted_after(
        &self,
        from: usize,
        needle: &str,
        timeout: std::time::Duration,
    ) -> bool {
        wait_for_after(&self.painted, from, needle, timeout)
    }
}

/// Free-function core of [`TestTty::wait_for_painted_after`], taking the
/// shared buffer directly so the polling logic is testable against a plain
/// `Mutex<Vec<u8>>` — no real pty, no responder thread, nothing for a
/// `TestTty::drop` to restore.
///
/// **Delegates to `tests_pty::wait_for_bytes` rather than repeating it**
/// (#2075). The pty harness grew the same bounded poll for the same reason —
/// its drain thread is a reader a caller can outrun — and two copies of "has
/// the reader caught up?" is how this crate's terminal code reached five
/// spinners and four erase strategies. Kept as a named local function because
/// the tests below pin the behaviour this module depends on, and because the
/// call sites read better for saying `after`.
fn wait_for_after(
    buf: &Mutex<Vec<u8>>,
    from: usize,
    needle: &str,
    timeout: std::time::Duration,
) -> bool {
    tests_pty::wait_for_bytes(buf, from, needle, timeout)
}

/// The answering half of a terminal: accumulate what the application paints,
/// and reply to a cursor-position query (`ESC[6n`) as a real terminal would.
fn answer_terminal_queries(
    master: RawFd,
    painted: &Mutex<Vec<u8>>,
    stop: &AtomicBool,
    cursor_reply: &[u8],
) {
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
            unsafe { libc::write(master, cursor_reply.as_ptr().cast(), cursor_reply.len()) };
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

/// Kernel-managed transient bits in `c_lflag` — state, not configuration.
///
/// `PENDIN` ("input pending, retype at next read") and `FLUSHO` ("output being
/// flushed") are set and cleared by the tty layer as a side effect of I/O and
/// mode changes; no application sets them deliberately, and nothing is expected
/// to restore them. macOS surfaces this where Linux happened not to: after a
/// raw-mode round trip the observed diff was exactly
/// `c_lflag 0x5cb -> 0x200005cb`, i.e. `PENDIN` alone. Comparing them would
/// assert on kernel bookkeeping and call it a restoration failure.
const TRANSIENT_LFLAGS: libc::tcflag_t = libc::PENDIN | libc::FLUSHO;

/// Compare the fields that decide how a terminal behaves. `termios` is not
/// `PartialEq`, padding/speed fields are not the contract, and
/// [`TRANSIENT_LFLAGS`] are kernel state rather than configuration.
pub(crate) fn modes_equal(a: &libc::termios, b: &libc::termios) -> bool {
    mode_diff(a, b).is_empty()
}

/// Which mode fields differ, named — so a failure says WHAT was not restored
/// instead of only that something was not.
pub(crate) fn mode_diff(a: &libc::termios, b: &libc::termios) -> String {
    let mut out = Vec::new();
    if a.c_lflag & !TRANSIENT_LFLAGS != b.c_lflag & !TRANSIENT_LFLAGS {
        out.push(format!("c_lflag {:#x} -> {:#x}", a.c_lflag, b.c_lflag));
    }
    if a.c_iflag != b.c_iflag {
        out.push(format!("c_iflag {:#x} -> {:#x}", a.c_iflag, b.c_iflag));
    }
    if a.c_oflag != b.c_oflag {
        out.push(format!("c_oflag {:#x} -> {:#x}", a.c_oflag, b.c_oflag));
    }
    if a.c_cflag != b.c_cflag {
        out.push(format!("c_cflag {:#x} -> {:#x}", a.c_cflag, b.c_cflag));
    }
    for (i, (x, y)) in a.c_cc.iter().zip(b.c_cc.iter()).enumerate() {
        if x != y {
            out.push(format!("c_cc[{i}] {x} -> {y}"));
        }
    }
    out.join(", ")
}

#[cfg(test)]
mod wait_for_after_tests {
    use super::wait_for_after;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Already present after `from` → returns `true` immediately (no need to
    /// exhaust the timeout, though a short one keeps the test fast either way).
    #[test]
    fn finds_a_needle_already_present_after_the_offset() {
        let buf = Mutex::new(b"before SHOWN after".to_vec());
        assert!(wait_for_after(&buf, 7, "SHOWN", Duration::from_millis(200)));
    }

    /// The offset is load-bearing: a needle that only occurs BEFORE `from`
    /// must not satisfy the wait — this is the exact bug class the helper
    /// exists to avoid (cursor Show legitimately appears earlier too).
    #[test]
    fn ignores_an_occurrence_before_the_offset() {
        let buf = Mutex::new(b"SHOWN before, nothing after".to_vec());
        assert!(!wait_for_after(
            &buf,
            11,
            "SHOWN",
            Duration::from_millis(200)
        ));
    }

    /// The race this exists to close: the needle is not there yet, but a
    /// concurrent writer appends it shortly after the wait begins. A
    /// snapshot-only check (no poll) would have missed this.
    #[test]
    fn observes_a_needle_appended_after_the_wait_begins() {
        let buf = Arc::new(Mutex::new(b"before ".to_vec()));
        let writer = std::thread::spawn({
            let buf = Arc::clone(&buf);
            move || {
                std::thread::sleep(Duration::from_millis(30));
                buf.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(b"SHOWN after");
            }
        });
        assert!(wait_for_after(&buf, 7, "SHOWN", Duration::from_secs(1)));
        writer.join().expect("writer thread");
    }

    /// A needle that never arrives times out and returns `false` rather than
    /// hanging — the timeout is a real bound, not decoration.
    #[test]
    fn times_out_and_returns_false_when_the_needle_never_arrives() {
        let buf = Mutex::new(b"before after".to_vec());
        let start = std::time::Instant::now();
        assert!(!wait_for_after(
            &buf,
            7,
            "SHOWN",
            Duration::from_millis(100)
        ));
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "must actually wait out the timeout, not return early"
        );
    }
}
