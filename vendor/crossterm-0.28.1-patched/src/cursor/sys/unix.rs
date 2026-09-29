use std::{
    io::{self, Error, ErrorKind, Write},
    time::Duration,
};

use crate::{
    event::{filter::CursorPositionFilter, poll_internal, read_internal, InternalEvent},
    terminal::{disable_raw_mode, enable_raw_mode, sys::is_raw_mode_enabled},
};

/// Returns the cursor position (column, row).
///
/// The top left cell is represented as `(0, 0)`.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
pub fn position() -> io::Result<(u16, u16)> {
    if is_raw_mode_enabled() {
        read_position_raw()
    } else {
        read_position()
    }
}

fn read_position() -> io::Result<(u16, u16)> {
    enable_raw_mode()?;
    let pos = read_position_raw();
    disable_raw_mode()?;
    pos
}

fn read_position_raw() -> io::Result<(u16, u16)> {
    // Use `ESC [ 6 n` to and retrieve the cursor position.
    let mut stdout = io::stdout();
    stdout.write_all(b"\x1B[6n")?;
    stdout.flush()?;

    // newt #2644: upstream's `Err(_) => {}` arm below fell through to the top
    // of the loop instead of returning, so a pty that answers `poll`/`read`
    // with an *error* (vhs's bundled `ttyd`, not a plain timeout) made this
    // retry forever with a fresh 2s window each call and never return at
    // all — even past its own advertised 2s timeout. Still present upstream
    // in 0.28.1, 0.29.0 and current `master` as of 2026-09-29. Patched here:
    // propagate a poll error immediately, and also propagate a `read_internal`
    // error instead of silently retrying that arm (the same shape of bug one
    // line down). An absolute deadline bounds the loop either way, so no
    // reachable path can spin past `2000ms` total regardless of how many
    // errors arrive.
    let deadline = std::time::Instant::now() + Duration::from_millis(2000);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match poll_internal(Some(remaining), &CursorPositionFilter) {
            Ok(true) => match read_internal(&CursorPositionFilter) {
                Ok(InternalEvent::CursorPosition(x, y)) => return Ok((x, y)),
                Ok(_) => {}
                Err(err) => return Err(err),
            },
            Ok(false) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    "The cursor position could not be read within a normal duration",
                ));
            }
            Err(err) => return Err(err),
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::TimedOut,
                "The cursor position could not be read within a normal duration",
            ));
        }
    }
}

/// #2644/#2646 dependency-level regression: `read_position_raw`'s retry loop
/// must not spin forever when `poll_internal` keeps erroring.
///
/// Not wired into any workspace `cargo test` invocation — this crate is a
/// vendored patch (`[patch.crates-io]`, not a workspace member), so run it
/// directly: `cargo test --manifest-path vendor/crossterm-0.28.1-patched/Cargo.toml`.
/// See `NEWT_PATCH_NOTE.md` and `../../.handoff/helper-jobs/issue-2644/RESULT-r2.md`
/// for the red-then-green proof against the unpatched source.
#[cfg(all(test, unix))]
mod newt_2644_tests {
    use super::read_position_raw;
    use std::time::{Duration, Instant};

    /// `setsid()` detaches this process from any controlling terminal, so
    /// `tty_fd()`'s `open("/dev/tty")` fails with `ENXIO` on every attempt —
    /// `poll_internal` therefore errors every single time it is tried, which
    /// is exactly the shape of the swallowed-error bug (a `ttyd` pty that
    /// answers `poll`/`read` with an error, not a plain timeout). This is a
    /// separate process per integration/unit test binary, so `setsid()`
    /// cannot affect any other test.
    #[test]
    fn repeated_poll_errors_return_promptly_not_forever() {
        // SAFETY: takes no pointers; this test binary has not forked and is
        // not already a session leader, so this succeeds.
        let rc = unsafe { libc::setsid() };
        assert!(
            rc >= 0,
            "setsid() failed ({}) — test process was already a session \
             leader, cannot model \"no controlling terminal\" here",
            std::io::Error::last_os_error()
        );
        assert!(
            std::fs::File::open("/dev/tty").is_err(),
            "precondition not met: /dev/tty is still reachable after \
             setsid(), so this would not exercise the repeated-poll-error path"
        );

        let start = Instant::now();
        let result = read_position_raw();
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "no controlling terminal must surface as an error, not a \
             fabricated position"
        );
        // Generous, but nowhere near "forever": the pre-fix bug retried with
        // a fresh 2s window on every poll error and never returned at all.
        assert!(
            elapsed < Duration::from_secs(5),
            "read_position_raw took {elapsed:?} with no controlling \
             terminal — the retry loop did not return promptly on a \
             repeated poll error"
        );
    }
}
