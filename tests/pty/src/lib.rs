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
    /// Bytes the child has written, collected by a background reader and
    /// handed to [`Pty::screen`] on demand.
    ///
    /// **Somebody has to be listening while the child talks.** `screen()` was
    /// once the only reader, and consumers call it after the child exits — so
    /// nothing drained the pty while the child ran. Past the kernel's buffer
    /// (~1 KiB on macOS) the child blocked in `write`, never reached its own
    /// read, and the consumer reported a timeout over a half-drawn screen. The
    /// symptom looked like a dead child; the cause was a deaf harness.
    ///
    /// A real terminal drains continuously, and so does the cockpit's own
    /// reader. This makes the harness behave like the thing it stands in for.
    drained: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
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

            // From here the drain is the SOLE reader of the master, and it
            // reads BLOCKING.
            //
            // `screen()` must therefore never touch the fd. An earlier attempt
            // had it flip the master to `O_NONBLOCK` to sweep up a tail; the
            // drain's next read then returned `EAGAIN`, the thread took that
            // for EOF and exited, and after the first `screen()` nothing
            // drained again. One reader, one blocking mode, no flag races.
            //
            // Detached on purpose: it ends when the master closes, which is
            // what `Drop` does. A join handle would have to be threaded
            // through every consumer to buy nothing.
            let drained = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = drained.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    let n = libc::read(master, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len());
                    if n <= 0 {
                        return;
                    }
                    sink.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend_from_slice(&buf[..n as usize]);
                }
            });

            Self {
                master,
                slave,
                drained,
            }
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
        // Give the drain a moment to catch up. A caller reading the instant a
        // child exits would otherwise race bytes still in flight — the same
        // race the old read-at-the-end had, now visible because the reading
        // happens on another thread rather than hidden inside this call.
        std::thread::sleep(std::time::Duration::from_millis(20));
        // TAKE, do not peek. The contract every consumer relies on is "the
        // bytes not yet returned": `settings_form_pty_test` reads in a loop and
        // accumulates its own transcript, so re-returning the whole history
        // would hand it each earlier frame again.
        //
        // Deliberately does not touch the fd — see the drain's note above.
        let mut drained = self
            .drained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&std::mem::take(&mut *drained)).into_owned()
    }

    /// Change the pty's window size mid-test — the resize probe for
    /// full-screen surfaces (#1677: the transcript pager must survive a
    /// resize without corrupting its scroll state).
    ///
    /// Note: the kernel sends SIGWINCH to the pty's foreground process
    /// *group*, but a child handed the slave via `slave_stdio()` never made
    /// it a controlling terminal (`O_NOCTTY`, no `setsid`), so no signal is
    /// delivered automatically. Pair this with [`signal_winch`] on the
    /// child's pid so its event loop actually observes the change.
    pub fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(self.slave, libc::TIOCSWINSZ, &ws) };
        assert_eq!(rc, 0, "TIOCSWINSZ (pty resize) failed");
    }

    /// Is the pty currently in RAW mode?
    ///
    /// The strongest postcondition a terminal-restoration test can assert
    /// (#1677). Line-discipline settings belong to the pty *device*, not to a
    /// particular file descriptor, so a `tcgetattr` on the parent's own slave
    /// fd observes the state the CHILD installed with `enable_raw_mode()`.
    /// That makes this KERNEL state — not an inference from escape bytes the
    /// child may have emitted, buffered, or never flushed.
    ///
    /// Raw here means what crossterm's `enable_raw_mode` does: canonical mode
    /// and echo off. Cooked (restored) is both back on.
    pub fn is_raw(&self) -> bool {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::tcgetattr(self.slave, &mut t) };
        assert_eq!(rc, 0, "tcgetattr on the pty slave failed");
        (t.c_lflag & libc::ICANON) == 0 || (t.c_lflag & libc::ECHO) == 0
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

/// Deliver SIGWINCH to a pty child so it notices a [`Pty::resize`] — see the
/// controlling-terminal note there. Best-effort by design: the child may
/// already have exited, and this must not race that exit into a panic.
pub fn signal_winch(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGWINCH);
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

// ---------------------------------------------------------------------------
// The screen as a GRID.
//
// Promoted here from `newt_tui::interaction_view_pty_test` (C2b, #1891) when
// #1979 needed it too. It was private to one test file, which is the exact
// condition under which the next caller writes a second, worse copy — the
// failure this epic keeps paying for. One reader of the escape stream.
// ---------------------------------------------------------------------------

/// The screen as a GRID, by applying cursor positioning rather than stripping it.
///
/// This exists because the first cut of the width assertion was wrong in a way
/// worth recording: `ratatui` does not emit padding, it MOVES the cursor
/// (`ESC[1;3H`) and prints a fragment. Stripping escapes therefore
/// concatenates every fragment onto one line — `⊘run_commandwantstorun…` —
/// and a width check over that measures nothing about the screen. Only by
/// honouring `CUP` does "no line exceeds the terminal width" become a claim
/// about what the operator sees.
///
/// Deliberately tiny: `CUP`, `\r`, `\n`, and printable text. Anything else is
/// consumed and ignored, which is sound here because the assertions are about
/// WHERE text lands, not how it is coloured.
pub fn screen_grid(screen: &str) -> Vec<String> {
    let mut grid: Vec<Vec<char>> = Vec::new();
    let (mut row, mut col) = (0usize, 0usize);
    let put = |grid: &mut Vec<Vec<char>>, row: usize, col: usize, ch: char| {
        while grid.len() <= row {
            grid.push(Vec::new());
        }
        let line = &mut grid[row];
        while line.len() <= col {
            line.push(' ');
        }
        line[col] = ch;
    };
    let mut chars = screen.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                row += 1;
                col = 0;
            }
            '\r' => col = 0,
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut final_byte = '\0';
                    for f in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&f) {
                            final_byte = f;
                            break;
                        }
                        params.push(f);
                    }
                    if final_byte == 'H' {
                        // CUP is 1-based, and an omitted parameter is 1.
                        let mut it = params.split(';');
                        let r: usize = it.next().unwrap_or("").parse().unwrap_or(1);
                        let c2: usize = it.next().unwrap_or("").parse().unwrap_or(1);
                        row = r.saturating_sub(1);
                        col = c2.saturating_sub(1);
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(f) = chars.next() {
                        if f == '\u{7}' || (f == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            printable if !printable.is_control() => {
                put(&mut grid, row, col, printable);
                col += 1;
            }
            _ => {}
        }
    }
    grid.into_iter()
        .map(|l| l.into_iter().collect::<String>().trim_end().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Wait for a child, or give up. Deliberately not `child.wait()`: the
    /// defect under test is a child that never exits, and a test that hangs
    /// forever reports nothing.
    fn wait(child: &mut std::process::Child, budget: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return None,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        None
    }

    /// **A child may write more than the pty buffer holds before anyone
    /// reads.** That is the whole defect this harness had.
    ///
    /// `screen()` used to be the only reader, and consumers call it after the
    /// child exits — so nothing drained while the child ran. Past the kernel's
    /// pty buffer (~1 KiB on macOS) the child blocked in `write`, never
    /// reached its own read, and every consumer reported it as a timeout with
    /// a half-drawn screen. The bug looked like "the child died"; it was "the
    /// harness stopped listening".
    ///
    /// 64 KiB is far past any plausible buffer, so this fails loudly on a
    /// harness that does not drain rather than depending on one platform's
    /// exact size.
    #[test]
    fn a_child_writing_past_the_pty_buffer_is_not_deadlocked_by_the_harness() {
        let pty = Pty::open();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            // `yes` is unbounded; `head -c` bounds it without needing seq.
            .arg("yes newt | head -c 65536; printf 'ALL-WRITTEN'")
            .stdout(pty.slave_stdio())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the writer");

        let status = wait(&mut child, Duration::from_secs(10));
        assert!(
            status.is_some_and(|s| s.success()),
            "the child never finished writing — the harness is not draining, \
             so it blocked in `write` long before it could exit"
        );

        let screen = pty.screen();
        assert!(
            screen.contains("ALL-WRITTEN"),
            "the tail of a large write must survive; got {} bytes",
            screen.len()
        );
        assert!(
            screen.len() >= 65536,
            "every byte the child wrote must be readable; got {}",
            screen.len()
        );
    }

    /// `screen()` returns the bytes NOT YET RETURNED, not the whole history.
    ///
    /// `settings_form_pty_test` reads in a loop and accumulates its own
    /// transcript, so a `screen()` that re-returned everything would hand it
    /// each earlier frame again and its grid would be assembled from the
    /// transcript rather than the screen. The drain changes who empties the
    /// pty, never what a caller sees.
    #[test]
    fn screen_returns_only_what_has_not_been_returned_before() {
        let pty = Pty::open();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf FIRST; sleep 0.4; printf SECOND")
            .stdout(pty.slave_stdio())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the writer");

        // Long enough for FIRST to land and short enough to precede SECOND.
        std::thread::sleep(Duration::from_millis(200));
        let first = pty.screen();
        assert!(first.contains("FIRST"), "first read: {first:?}");
        assert!(!first.contains("SECOND"), "read the future: {first:?}");

        assert!(wait(&mut child, Duration::from_secs(10)).is_some());
        let second = pty.screen();
        assert!(second.contains("SECOND"), "second read: {second:?}");
        assert!(
            !second.contains("FIRST"),
            "a byte was returned twice: {second:?}"
        );
    }
}
