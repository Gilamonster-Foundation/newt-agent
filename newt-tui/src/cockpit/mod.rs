//! The cockpit: the terminal owned by ONE thread for the whole session.
//!
//! Before this, "the terminal" during a turn belonged to the session thread —
//! it entered cbreak, spawned the keyboard watcher, and printed the transcript
//! with raw `println!` — while the UI thread sat in `recv()`. That is why there
//! was no prompt while a turn ran: the only thread that could draw one was
//! asleep, and the transcript was written by a thread that did not know a
//! prompt existed. Painting one anyway would have put the next `⚙ read_file…`
//! ON it — one garbled transcript line per miss, forever.
//!
//! So the cockpit inverts ownership rather than bracketing writes:
//!
//! - [`pty::PtyCapture`] takes fd 1/2 away from the process onto a pty slave
//!   for the session's lifetime. Every byte the session prints — transcript,
//!   spinner frames, a permission question, a `WARN` — arrives on the master.
//! - [`ansi::TranscriptStream`] turns those bytes into finished rows, an
//!   in-progress row (the spinner lives there), and the few sequences the
//!   real terminal must still see (mouse capture, bracketed paste).
//! - [`presenter::Presenter`] is the only writer to the real terminal: rows
//!   go into scrollback above a bottom block it keeps mounted — status row,
//!   editor, tab bar — and it reads the keyboard, so typing works while a
//!   turn runs and Ctrl-C interrupts it.
//!
//! What this is NOT: a VT emulator. Cursor motion from the session is dropped,
//! and the two cursor-relative renderers (live spill, completed spill) are not
//! constructed under the cockpit in v1. The tool spinner (#1727) covers
//! liveness for them.

// Platform-agnostic: the byte scanner (stream model, DEC-mode allowlist, UTF-8
// boundary carry) has no OS dependency and is reused by the Windows-cockpit
// feasibility work (#1746). On Windows it is exercised by `conpty_probe`'s tests
// but has no live presenter consuming it yet, so its items read as dead there.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) mod ansi;

// The live cockpit — the fd 1/2 capture and the real-terminal presenter — is
// unix-only: it is built on `openpty`/`dup2`/termios. Windows keeps the classic
// per-turn surface until a ConPTY backend lands (#1746).
#[cfg(unix)]
pub(crate) mod presenter;
#[cfg(unix)]
pub(crate) mod pty;
// The cockpit's test terminal is `openpty`/termios too, so it follows `pty`
// onto unix only. Without this the spike's relaxed module gate (below) would
// pull it into the Windows build, where it cannot compile.
#[cfg(all(test, unix))]
pub(crate) mod test_tty;

#[cfg(unix)]
pub(crate) use presenter::Presenter;

// #1746 feasibility spike: does ConPTY let a Windows cockpit do what the unix
// pty capture does? Two probes, run on Windows CI. See the module and
// `docs/decisions/windows_cockpit_conpty.md`.
#[cfg(windows)]
mod conpty_probe;
