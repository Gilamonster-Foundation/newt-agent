//! **The one raw-mode guard** — save the terminal's mode, restore exactly it.
//!
//! Promoted out of `tty::modal` by C2b (#1891), where it had been private
//! since #1770. It is a tty primitive rather than a modal detail, and the
//! second surface that needed it (the RichTUI interaction frame) had already
//! reached for `crossterm::terminal::enable_raw_mode` and inherited the exact
//! bug this type exists to avoid.
//!
//! # Why not `crossterm::terminal::enable_raw_mode`
//!
//! **crossterm keeps ONE process-global "mode prior to raw" and makes a
//! second `enable_raw_mode` a no-op while it is set.** Two consequences, and
//! both have bitten this repo:
//!
//! - **The inner enter does nothing.** Under the cockpit (#1669) the terminal
//!   thread already owns raw mode, so a modal's request was a no-op while
//!   `StdinToken::acquire` had just put the tty in canonical+echo. Keys were
//!   line-buffered until Enter and echoed by the kernel over the editor row —
//!   a prompt that looked hung.
//! - **The inner drop restores GLOBALLY.** A frame opened over a frame hands
//!   the terminal back when the INNER one closes, while the outer is still
//!   drawn. That is the mirror failure, and
//!   `interaction_view_pty_test::a_nested_frame_does_not_restore_the_terminal_early`
//!   caught the RichTUI frame doing exactly it (C2b, #1891).
//!
//! Saving and restoring the termios instead — the way `StdinToken` does for
//! line mode — makes nested ownership simply compose: each guard restores
//! what IT found, so an inner guard hands back "raw", not "cooked".
//!
//! # Restoration is a `Drop` obligation
//!
//! Never happy-path cleanup. A `disable_raw_mode()` statement after a loop is
//! reached by an error return and skipped by a panic, which leaves an
//! operator in a shell that no longer echoes (see #1889 for a live instance).

use std::io;

/// Raw mode for as long as this lives; on drop, EXACTLY the prior mode.
///
/// Composes under nesting: each guard restores what it found, so an inner
/// guard dropping inside an outer one restores to raw rather than to cooked.
pub struct RawModeGuard {
    #[cfg(unix)]
    prev: Option<libc::termios>,
}

impl RawModeGuard {
    /// Take raw mode, remembering what to put back.
    ///
    /// # Errors
    ///
    /// The termios round-trip failed — most often because the fd is not a
    /// terminal.
    pub fn enter() -> io::Result<Self> {
        #[cfg(unix)]
        {
            // SAFETY: termios round-trip on stdin, restored on drop.
            let prev = unsafe {
                let fd = libc::STDIN_FILENO;
                let mut prev: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(fd, &mut prev) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut raw = prev;
                libc::cfmakeraw(&mut raw);
                if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                    return Err(io::Error::last_os_error());
                }
                prev
            };
            Ok(Self { prev: Some(prev) })
        }
        #[cfg(not(unix))]
        {
            // No termios; fall back to crossterm's global, which carries the
            // nesting hazard above. Windows nesting is untested here and is
            // the reason this arm is documented rather than silent.
            crossterm::terminal::enable_raw_mode()?;
            Ok(Self {})
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(prev) = self.prev.take() {
            // SAFETY: restoring the termios captured in `enter`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &prev);
            }
        }
        #[cfg(not(unix))]
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
