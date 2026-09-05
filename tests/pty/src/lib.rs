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
    /// Set once [`Pty::screen_to_eof`] has closed the slave, so [`Drop`] does
    /// not close the same descriptor twice — a second `close` of a number the
    /// kernel has already handed back would shut down whatever unrelated file
    /// took it.
    slave_closed: bool,
    /// The drain thread, and the signal it sends when it stops.
    ///
    /// Detaching it was right while nothing could ever observe its end: the
    /// master stayed open for the harness's whole life, so there was no end to
    /// observe. [`Pty::screen_to_eof`] creates one, and joining it there is the
    /// exact "every byte has been handed over" proof that a sleep only guessed
    /// at.
    reader: Option<std::thread::JoinHandle<()>>,
    reader_done: std::sync::mpsc::Receiver<()>,
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
            // `done` fires when the reader stops, which happens only at EOF —
            // see `screen_to_eof`. Sent by dropping the sender, so it fires on
            // a panic in the reader too rather than hanging a waiter.
            let (done_tx, reader_done) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                let _done = done_tx;
                let mut buf = [0u8; 8192];
                loop {
                    // This thread is the sole reader of `master`, which
                    // outlives it — the harness closes the master only in
                    // `Drop`, after `screen_to_eof` has joined this thread or
                    // the process is tearing down.
                    let n = libc::read(master, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len());
                    if n <= 0 {
                        // EOF (macOS) or EIO (Linux) once the last slave
                        // descriptor closes, and only then: every byte written
                        // before that close has already been delivered above.
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
                slave_closed: false,
                drained,
                reader: Some(reader),
                reader_done,
            }
        }
    }

    /// Send bytes as if the operator typed them.
    pub fn type_in(&self, s: &str) {
        let n = unsafe { libc::write(self.master, s.as_ptr().cast::<libc::c_void>(), s.len()) };
        assert!(n > 0, "writing the operator's keystrokes to the pty failed");
    }

    /// Whatever has been drained SO FAR — a snapshot of a live stream, with no
    /// claim that the child is finished writing.
    ///
    /// **This no longer sleeps, and it never promised what the sleep implied.**
    /// It used to pause 20 ms "to give the drain a moment to catch up", which
    /// is a guess about the scheduler wearing the shape of a guarantee: the
    /// child's `write` returning, or the child exiting, says nothing about when
    /// the drain thread appends those bytes here. Under load the guess is
    /// wrong and the caller gets a partial screen — as this function's own
    /// documentation used to concede.
    ///
    /// Two seams replace it, and which one a caller wants is not a matter of
    /// taste:
    ///
    /// - the child has exited and you want everything → [`Pty::screen_to_eof`],
    ///   which is exact;
    /// - you are waiting for something to appear → [`Pty::wait_for_screen`],
    ///   which waits for that evidence and nothing else.
    ///
    /// Reach for this one only to sample a stream you are accumulating, where a
    /// short read costs a lap of the loop rather than a false failure.
    pub fn screen(&self) -> String {
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

    /// Wait until `needle` has been drained, then report whether it arrived.
    ///
    /// The read-side twin of waiting on a child: it waits for the EVIDENCE the
    /// caller is about to assert on, so a slow drain costs latency instead of a
    /// false failure. The timeout bounds a FAILURE — on the passing path this
    /// returns as soon as the bytes land — so a loaded runner makes it slower,
    /// never wrong.
    ///
    /// **Peeks; it does not take.** A `screen()` loop that took and discarded
    /// while hunting for a marker could split that marker across two takes and
    /// never see it, which is a second flake wearing the first one's clothes.
    /// Waiting leaves every byte in place for the `screen()` or
    /// [`Pty::screen_to_eof`] that follows.
    pub fn wait_for_screen(&self, needle: &str, timeout: std::time::Duration) -> bool {
        wait_for_bytes(&self.drained, 0, needle, timeout)
    }

    /// Everything the child wrote, waiting for the pty to reach EOF first.
    ///
    /// **This is the exact replacement for the sleep, and it is not a longer
    /// guess.** The harness keeps its OWN slave descriptor open for its whole
    /// life, so the master never sees EOF and the drain thread never stops —
    /// there was no completion signal to wait for, which is why a sleep stood
    /// in for one. Once the child has exited its descriptors are closed, so
    /// closing ours makes it the LAST close: the master then delivers every
    /// buffered byte and reports EOF, the drain thread returns, and joining it
    /// proves the buffer is complete. The kernel answers the question the
    /// sleep was guessing at.
    ///
    /// # Preconditions
    ///
    /// **The child must have exited** (`child.wait()` returned). A child still
    /// holding a slave descriptor keeps the pty open and there is no EOF to
    /// reach; the wait below then fails the test rather than hanging it, but
    /// the honest fix is to wait for the child first.
    ///
    /// The `Pty` is consumed: after EOF there is nothing further to read, and
    /// [`Pty::is_raw`] needs the slave this closes, so sample that BEFORE
    /// calling this.
    ///
    /// # Panics
    ///
    /// If the drain does not reach EOF within ten seconds — a bound on
    /// failure, not on success. A test that hangs in CI reports nothing; this
    /// says which harness invariant broke.
    pub fn screen_to_eof(mut self) -> String {
        // SAFETY: closing a descriptor this struct owns, exactly once — the
        // flag below keeps `Drop` from closing it again.
        unsafe { libc::close(self.slave) };
        self.slave_closed = true;
        // The reader never SENDS; it drops its sender as it returns, so
        // `Disconnected` IS the "drain finished" signal and only `Timeout` is a
        // failure. Treating a disconnect as an error would fail every healthy
        // call — which is exactly what this crate's own tests said when it did.
        match self
            .reader_done
            .recv_timeout(std::time::Duration::from_secs(10))
        {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "the pty drain never reached EOF after the slave closed — a \
                 child is still holding a slave descriptor, so wait for it \
                 before calling this"
            ),
        }
        if let Some(reader) = self.reader.take() {
            // Already finished: the signal above fires as the thread returns,
            // so this only reaps it.
            let _ = reader.join();
        }
        let mut drained = self
            .drained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let screen = String::from_utf8_lossy(&std::mem::take(&mut *drained)).into_owned();
        drop(drained);
        screen
    }

    /// The final screen, exact when the child finished and honest when it did
    /// not.
    ///
    /// A child that exited has released its slave descriptors, so
    /// [`Pty::screen_to_eof`] can reach EOF and return every byte. A child that
    /// hung or was killed still holds one, so there is no EOF to reach — and
    /// what has been drained so far is exactly what a hang should report,
    /// rather than a ten-second wait ending in a panic that buries the real
    /// failure.
    ///
    /// Exists so its three callers do not each write that conditional slightly
    /// differently; the choice is a property of the harness, not of any one
    /// test.
    pub fn screen_when_finished(self, child_exited: bool) -> String {
        if child_exited {
            self.screen_to_eof()
        } else {
            self.screen()
        }
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
            // `screen_to_eof` may already have closed the slave to reach EOF.
            // Closing a descriptor the kernel has handed back would shut down
            // whatever unrelated file inherited the number.
            if !self.slave_closed {
                libc::close(self.slave);
            }
            libc::close(self.master);
        }
    }
}

/// Wait until `needle` appears in `buf` at or after byte offset `from`.
///
/// **The ONE bounded poll for "has the reader caught up?"**, shared rather than
/// copied: `newt_tui::cockpit::test_tty` grew this same loop for its own
/// capture thread (#2071), and a second copy is how the terminal code reached
/// five spinners and four erase strategies one reasonable-looking copy at a
/// time. The two harnesses drain different descriptors; the question they ask
/// is identical.
///
/// `from` matters and is not optional: expected sequences legitimately recur —
/// a cursor `Show`, a repainted row — so "appears anywhere" would return on an
/// EARLIER occurrence without ever waiting for the one under test. A caller
/// whose buffer is emptied as it is read (see [`Pty::screen`]) passes 0,
/// because for it the buffer already holds only what has not been returned.
///
/// The timeout bounds a FAILURE. On the passing path this returns as soon as
/// the bytes land, so a loaded runner makes it slower, never wrong.
pub fn wait_for_bytes(
    buf: &std::sync::Mutex<Vec<u8>>,
    from: usize,
    needle: &str,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let seen = {
            let buf = buf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let text = String::from_utf8_lossy(&buf);
            text.get(from.min(text.len())..)
                .is_some_and(|tail| tail.contains(needle))
        };
        if seen || std::time::Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
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

        // #2075: `screen()` at this point read whatever the drain happened to
        // have appended in the 20 ms it slept, which for 64 KiB is a race this
        // test would lose under load and report as a lost tail. EOF is the
        // kernel saying the same thing exactly.
        let screen = pty.screen_to_eof();
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
        let second = pty.screen_to_eof();
        assert!(second.contains("SECOND"), "second read: {second:?}");
        assert!(
            !second.contains("FIRST"),
            "a byte was returned twice: {second:?}"
        );
    }

    /// **The sleep's replacement is exact, not longer.**
    ///
    /// The child writes far more than any pty buffer and exits; every byte
    /// must be present the instant `screen_to_eof` returns, with no settling
    /// pause anywhere. A `screen()` here would be reading a live stream and
    /// could legitimately return a prefix — which is precisely what the 20 ms
    /// sleep was papering over, and what CI lost under load.
    #[test]
    fn screen_to_eof_returns_every_byte_with_no_settling_time() {
        let pty = Pty::open();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("yes newt | head -c 200000; printf 'THE-VERY-LAST-BYTES'")
            .stdout(pty.slave_stdio())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the writer");
        assert!(
            wait(&mut child, Duration::from_secs(20)).is_some_and(|s| s.success()),
            "the child never finished writing"
        );

        let screen = pty.screen_to_eof();
        assert!(
            screen.ends_with("THE-VERY-LAST-BYTES"),
            "the final bytes must be present, and last; tail was {:?}",
            &screen[screen.len().saturating_sub(40)..]
        );
        assert!(
            screen.len() >= 200_000 + "THE-VERY-LAST-BYTES".len(),
            "every byte must survive; got {}",
            screen.len()
        );
    }

    /// `wait_for_screen` waits for EVIDENCE, and leaves it in place.
    ///
    /// Both halves matter. Returning early would put the guess back; taking
    /// the bytes would break the caller that reads them next — and a marker
    /// split across two destructive reads is the flake this seam exists to
    /// prevent.
    #[test]
    fn wait_for_screen_waits_for_late_output_without_consuming_it() {
        let pty = Pty::open();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf EARLY; sleep 0.5; printf THE-MARKER")
            .stdout(pty.slave_stdio())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the writer");

        // Not there yet: proves the wait below is doing the waiting, rather
        // than the marker having arrived before we looked.
        assert!(
            !pty.wait_for_screen("THE-MARKER", Duration::from_millis(50)),
            "the marker cannot already be on screen"
        );
        assert!(
            pty.wait_for_screen("THE-MARKER", Duration::from_secs(10)),
            "the marker never arrived"
        );

        // Peeked, not taken — and EARLY, drained long before, is still here
        // too, so waiting consumed nothing at either end of the buffer.
        let screen = pty.screen();
        assert!(screen.contains("THE-MARKER"), "waiting ate it: {screen:?}");
        assert!(screen.contains("EARLY"), "waiting ate the head: {screen:?}");

        assert!(wait(&mut child, Duration::from_secs(10)).is_some());
    }

    /// A needle that never comes is a bounded FALSE, not a hang.
    #[test]
    fn wait_for_screen_gives_up_and_says_so() {
        let pty = Pty::open();
        let started = std::time::Instant::now();
        assert!(
            !pty.wait_for_screen("NEVER-WRITTEN", Duration::from_millis(120)),
            "nothing wrote this"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must bound the wait"
        );
    }

    /// The shared poll honours `from`, so a LATER occurrence can be waited for.
    ///
    /// `newt_tui::cockpit::test_tty` depends on exactly this: a cursor `Show`
    /// legitimately appears earlier in the session, and "appears anywhere"
    /// would return on that one without ever waiting for the one under test.
    #[test]
    fn wait_for_bytes_can_wait_for_a_later_occurrence() {
        let buf = std::sync::Mutex::new(b"MARK and then some".to_vec());
        assert!(
            wait_for_bytes(&buf, 0, "MARK", Duration::from_millis(50)),
            "the first occurrence is visible from 0"
        );
        assert!(
            !wait_for_bytes(&buf, 4, "MARK", Duration::from_millis(50)),
            "an occurrence BEFORE `from` must not satisfy the wait"
        );

        let buf = std::sync::Arc::new(std::sync::Mutex::new(b"MARK".to_vec()));
        let writer = std::sync::Arc::clone(&buf);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(b" ... MARK again");
        });
        assert!(
            wait_for_bytes(&buf, 4, "MARK", Duration::from_secs(10)),
            "the later occurrence must satisfy it once written"
        );
    }
}
