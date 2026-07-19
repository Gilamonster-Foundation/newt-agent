//! Mouse-capture RAII guard + process-level panic-hook release (#1303).
//!
//! Feature-gated under the interactive surfaces (`live-spill` / `rich-tui`); a
//! `--no-default-features` wyvern build never links this module. Mouse capture
//! is turned on for the duration of one live-spill turn and released on EVERY
//! exit path (decision clause B / rule 7):
//!   * normal return / `?` — the guard drops at end of scope;
//!   * panic-unwind — the guard drops while the stack unwinds;
//!   * the rule-7 abandon / teardown-miss path — that path emits nothing
//!     through the renderer, so release rides this guard's `Drop` (a direct
//!     stdout write), never a renderer write;
//!   * a hard abort where the guard's stack frame is bypassed — the one-time
//!     panic hook ([`install_panic_release_hook`]) also emits
//!     `DisableMouseCapture`.
//!
//! Every emit is idempotent, so the guard drop and the panic hook may both fire
//! harmlessly. The guard mirrors `RawGuard` in `lean_input.rs`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};

/// Set while mouse capture is live. The panic hook consults it so a panic on a
/// NON-interactive path (`newt worker` / `newt mcp` / piped) — where capture was
/// never enabled — emits nothing, preserving the byte-for-byte stdout contract
/// (decision clause E).
static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Where the capture escape sequences are written. Production writes stdout; a
/// test injects a shared buffer so the guard's `Drop` side effect is observable
/// WITHOUT touching any renderer writer (the rule-7 abandon acceptance test
/// asserts release via this handle, not the renderer's writer).
pub(crate) enum MouseSink {
    Stdout,
    #[cfg(test)]
    Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>),
}

impl MouseSink {
    fn emit(&self, cmd: impl crossterm::Command) {
        match self {
            Self::Stdout => {
                let mut out = std::io::stdout();
                let _ = crossterm::queue!(out, cmd);
                let _ = out.flush();
            }
            #[cfg(test)]
            Self::Shared(buf) => {
                let mut guard = buf.lock().unwrap_or_else(|p| p.into_inner());
                let _ = crossterm::queue!(*guard, cmd);
            }
        }
    }
}

/// RAII: `EnableMouseCapture` on construction, `DisableMouseCapture` on drop —
/// on normal return, `?`, and panic-unwind alike.
pub(crate) struct MouseCaptureGuard {
    sink: MouseSink,
}

impl MouseCaptureGuard {
    pub(crate) fn enable(sink: MouseSink) -> Self {
        sink.emit(EnableMouseCapture);
        CAPTURE_ACTIVE.store(true, Ordering::SeqCst);
        Self { sink }
    }

    /// Turn-scoped stdout guard when `on` is true; `None` leaves the terminal
    /// untouched (the keyboard tier / opted-out / non-mouse path). Nothing is
    /// emitted when `on` is false, so the non-mouse path is byte-identical.
    pub(crate) fn maybe(on: bool) -> Option<Self> {
        on.then(|| Self::enable(MouseSink::Stdout))
    }
}

impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        // Unconditional (idempotent) — the process-level panic hook may also
        // have emitted the disable; a second `?1000l/?1006l` is harmless.
        self.sink.emit(DisableMouseCapture);
        CAPTURE_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// Install — exactly once — a panic hook that releases mouse capture before the
/// process dies, covering a hard abort where the guard's stack frame is
/// bypassed. Idempotent with the guard's own `Drop`. The disable is emitted
/// ONLY when capture is currently active, so a panic on a non-interactive path
/// (where capture was never enabled) stays byte-clean. Chains to the previous
/// hook so the default panic message still prints.
pub fn install_panic_release_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if CAPTURE_ACTIVE.swap(false, Ordering::SeqCst) {
                let mut out = std::io::stdout();
                let _ = crossterm::queue!(out, DisableMouseCapture);
                let _ = out.flush();
            }
            prev(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn shared() -> (MouseSink, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        (MouseSink::Shared(buf.clone()), buf)
    }

    #[test]
    fn guard_enables_on_construct_and_disables_on_drop() {
        let (sink, buf) = shared();
        {
            let _guard = MouseCaptureGuard::enable(sink);
            let enabled = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
            // crossterm's EnableMouseCapture turns SGR (?1006) tracking on.
            assert!(
                enabled.contains("\u{1b}[?1006h"),
                "enable emitted: {enabled:?}"
            );
            assert!(!enabled.contains("\u{1b}[?1006l"), "not yet disabled");
        }
        // Dropping the guard releases capture.
        let after = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            after.contains("\u{1b}[?1006l"),
            "drop emitted disable: {after:?}"
        );
    }

    #[test]
    fn maybe_false_emits_nothing() {
        // The opted-out / keyboard tier holds no guard and writes no bytes.
        assert!(MouseCaptureGuard::maybe(false).is_none());
    }

    #[test]
    fn guard_release_survives_a_panic_unwind() {
        let (sink, buf) = shared();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = MouseCaptureGuard::enable(sink);
            panic!("turn blew up mid-frame");
        }));
        assert!(result.is_err(), "panic propagated");
        let after = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            after.contains("\u{1b}[?1006l"),
            "capture released during unwind: {after:?}"
        );
    }
}
