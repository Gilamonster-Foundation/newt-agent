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

pub(crate) mod ansi;
pub(crate) mod presenter;

pub(crate) mod pty;
#[cfg(test)]
pub(crate) mod test_tty;

pub(crate) use presenter::Presenter;
