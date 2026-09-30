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
/// directly:
///   cargo test --manifest-path vendor/crossterm-0.28.1-patched/Cargo.toml newt_2644 -- --test-threads=1
/// Mirrored by the "vendor crossterm regression gate" CI step and `just check-vendor-crossterm`.
/// See `NEWT_PATCH_NOTE.md` for context.
#[cfg(all(test, unix))]
mod newt_2644_tests {
    use super::read_position_raw;
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    // When this env var is set the process is the subprocess body. The outer
    // test spawns a fresh child with this var and stdin=/dev/null.
    const SUBPROCESS_VAR: &str = "NEWT_2644_SUBPROCESS";

    /// Subprocess body: setsid → verify /dev/tty is gone → print start
    /// marker → call read_position_raw → print result → exit.
    ///
    /// Separated from the #[test] so the compiler can check it shares the
    /// same crossterm version as the outer test (same crate, same binary).
    fn run_as_subprocess() -> ! {
        use std::io::Write as _;
        let mut err = std::io::stderr();

        // Remove the controlling terminal so /dev/tty → ENXIO on every open.
        // stdin is /dev/null (parent set Stdio::null()), so isatty(stdin)=false
        // and tty_fd() falls through to the open("/dev/tty") path.
        //
        // SAFETY: no pointers; the child was just spawned so it is not yet a
        // session leader.
        let rc = unsafe { libc::setsid() };
        if rc < 0 {
            let _ = writeln!(err, "NEWT_2644 FAILED_SETSID {}", std::io::Error::last_os_error());
            std::process::exit(2);
        }
        // Verify: /dev/tty must be unreachable now.
        if std::fs::File::open("/dev/tty").is_ok() {
            let _ = writeln!(err, "NEWT_2644 FAILED_PRECONDITION /dev/tty still reachable after setsid");
            std::process::exit(3);
        }

        // Signal to parent that we have reached read_position_raw with the
        // intended preconditions met.
        let _ = write!(err, "STARTED");
        let _ = err.flush();

        let start = std::time::Instant::now();
        let result = read_position_raw();
        let elapsed = start.elapsed();

        match result {
            // Patched: the poll error is returned promptly. A swallowed error that only
            // surfaces when the absolute deadline expires is the pre-fix bug, so it fails.
            Err(e) if elapsed < Duration::from_millis(1000) => {
                let _ = writeln!(err, " OK err_kind={:?} elapsed_ms={}", e.kind(), elapsed.as_millis());
                let _ = err.flush();
                std::process::exit(0);
            }
            Err(e) => {
                let _ = writeln!(err, " FAIL_SLOW_ERR err_kind={:?} elapsed_ms={}", e.kind(), elapsed.as_millis());
                let _ = err.flush();
                std::process::exit(2);
            }
            Ok(pos) => {
                let _ = writeln!(err, " FAIL_GOT_OK pos={:?}", pos);
                let _ = err.flush();
                std::process::exit(1);
            }
        }
    }

    /// Regression gate: `read_position_raw` must return `Err` promptly when
    /// `poll_internal` keeps erroring, not spin forever (newt #2644).
    ///
    /// Runs the assertion in a deterministic subprocess so:
    /// - stdin = /dev/null → `isatty(stdin)` is false; the crossterm tty fd
    ///   path falls through to `open("/dev/tty")`.
    /// - `setsid()` in the child removes the controlling terminal so every
    ///   `open("/dev/tty")` fails with ENXIO → `poll_internal` errors on
    ///   every call → the pre-fix `Err(_) => {}` arm looped forever.
    /// - A fresh child process gives crossterm a fresh internal event reader,
    ///   so no shared global state from other tests interferes.
    /// - `setsid()` is isolated to the child; it cannot affect this process
    ///   or any sibling test.
    ///
    /// Three assertions gate the green result:
    /// 1. Subprocess returns within 4 s (pre-fix: never returned).
    /// 2. "STARTED" marker in stderr (test reached read_position_raw with
    ///    preconditions met — not an earlier failure like a write error).
    /// 3. Subprocess exited 0 (function returned Err, not Ok or panic).
    #[test]
    fn repeated_poll_errors_return_promptly_not_forever() {
        if std::env::var(SUBPROCESS_VAR).is_ok() {
            run_as_subprocess(); // never returns
        }

        let exe = std::env::current_exe().expect("current_exe must succeed in a test binary");

        // Spawn a fresh process. stdin=/dev/null ensures isatty(stdin)=false.
        // stderr is piped so we can read the start marker and result.
        let mut child = Command::new(&exe)
            .env(SUBPROCESS_VAR, "1")
            // Filter to this exact test and pass --nocapture so eprint!/eprintln!
            // in the subprocess go directly to our piped stderr.
            .args(["repeated_poll_errors_return_promptly_not_forever", "--nocapture"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn subprocess");

        // Drain stderr in a background thread to avoid pipe-buffer deadlock.
        let stderr_thread = {
            let mut pipe = child.stderr.take().expect("stderr pipe");
            std::thread::spawn(move || {
                let mut buf = String::new();
                pipe.read_to_string(&mut buf).ok();
                buf
            })
        };

        // Wait up to 4 s. Pre-fix: never returned (fresh 2 s poll on every
        // error). 4 s >> 2 s hard deadline the patch imposes.
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        let exit_status = loop {
            match child.try_wait().expect("try_wait") {
                Some(s) => break Some(s),
                None if std::time::Instant::now() >= deadline => {
                    child.kill().ok();
                    child.wait().ok();
                    break None;
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };

        let stderr = stderr_thread.join().unwrap_or_default();

        assert!(
            exit_status.is_some(),
            "subprocess did not return within 4 s — poll-error retry loop is \
             still spinning (pre-fix behaviour).\nstderr: {stderr}"
        );
        assert!(
            stderr.contains("STARTED"),
            "start marker absent — subprocess did not reach read_position_raw \
             with preconditions met (setsid failed or /dev/tty still reachable).\n\
             stderr: {stderr}"
        );
        assert!(
            exit_status.map(|s| s.success()).unwrap_or(false),
            "subprocess exited non-zero — read_position_raw did not return Err \
             as expected, or a precondition check failed.\nstderr: {stderr}"
        );
    }
}
