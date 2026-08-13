//! Turn-time type-ahead: keystrokes typed while a turn is running.
//!
//! Persistent-prompt phase 1 (docs/decisions/persistent_prompt.md). During a
//! turn the keyboard watcher owns stdin, and it used to DROP every ground byte
//! that wasn't an interrupt or a viewport-nav key — typing while the model was
//! thinking simply vanished, which is a large part of "the TUI feels hung."
//! The watcher now drains those bytes here after every read, and the input
//! surfaces (lean + rich) pre-fill the next prompt with them, so nothing typed
//! during a turn is lost.
//!
//! Process-wide for the same reason the watcher is: there is exactly one turn
//! and one keyboard. Echoing the buffer live under the spinner is phase 2 —
//! it needs the line arbiter to grow a two-row lease and is deliberately NOT
//! attempted here (one line, one writer stays the law until then).

use std::sync::{Mutex, OnceLock};

/// Upper bound on buffered type-ahead. A human queueing a follow-up message is
/// hundreds of bytes; the cap only exists so a wedged terminal or a cat on the
/// keyboard cannot grow the buffer without bound. Overflow drops the newest
/// bytes (the visible prefill makes the loss obvious, unlike a silent head-drop).
const MAX_BYTES: usize = 4096;

fn buffer() -> &'static Mutex<Vec<u8>> {
    static BUFFER: OnceLock<Mutex<Vec<u8>>> = OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(Vec::new()))
}

/// Append drained decoder bytes. Called from the turn keyboard watcher only.
#[cfg(unix)]
pub(crate) fn push_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut buf = buffer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let room = MAX_BYTES.saturating_sub(buf.len());
    let taken = bytes.len().min(room);
    buf.extend_from_slice(&bytes[..taken]);
    if taken < bytes.len() {
        // The cap cut mid-stream. If it split a multi-byte UTF-8 sequence,
        // back the buffer off the incomplete tail so the lossy decode at
        // `take()` drops the whole character instead of rendering U+FFFD.
        trim_incomplete_utf8_tail(&mut buf);
    }
}

/// Drop a trailing INCOMPLETE UTF-8 sequence (a cap cut is the only caller).
/// Bytes that are invalid outright are left alone — they are the input's to
/// own, and `take()`'s lossy decode already handles them.
#[cfg(unix)]
fn trim_incomplete_utf8_tail(buf: &mut Vec<u8>) {
    if let Err(e) = std::str::from_utf8(buf) {
        if e.error_len().is_none() {
            buf.truncate(e.valid_up_to());
        }
    }
}

/// Take everything typed during the last turn, ready to pre-fill a prompt.
/// Lossy-decodes so a split UTF-8 tail can never poison the buffer.
pub(crate) fn take() -> String {
    let drained = {
        let mut buf = buffer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *buf)
    };
    String::from_utf8_lossy(&drained).into_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use super::{push_bytes, take, MAX_BYTES};

    /// Global buffer ⇒ serial; each test starts by draining leftovers.
    #[serial_test::serial(type_ahead)]
    #[test]
    fn push_then_take_round_trips_and_empties() {
        let _ = take();
        push_bytes("fix the".as_bytes());
        push_bytes(" test\n".as_bytes());
        assert_eq!(take(), "fix the test\n");
        assert_eq!(take(), "", "take drains");
    }

    #[serial_test::serial(type_ahead)]
    #[test]
    fn overflow_drops_the_newest_bytes_at_the_cap() {
        let _ = take();
        push_bytes(&vec![b'a'; MAX_BYTES]);
        push_bytes(b"overflow");
        let got = take();
        assert_eq!(got.len(), MAX_BYTES);
        assert!(!got.contains("overflow"));
    }

    #[serial_test::serial(type_ahead)]
    #[test]
    fn invalid_utf8_decodes_lossily() {
        let _ = take();
        push_bytes(b"caf\xff!");
        assert_eq!(take(), "caf\u{fffd}!");
    }

    /// A cap cut that lands mid-character drops the WHOLE character rather
    /// than leaving a lead byte that would lossy-decode to U+FFFD.
    #[serial_test::serial(type_ahead)]
    #[test]
    fn cap_cut_never_splits_a_utf8_character() {
        let _ = take();
        push_bytes(&vec![b'a'; MAX_BYTES - 1]);
        push_bytes("é".as_bytes()); // 2 bytes; only 1 byte of room
        let got = take();
        assert_eq!(got.len(), MAX_BYTES - 1);
        assert!(!got.contains('\u{fffd}'), "no replacement char: {got:?}");
    }
}
