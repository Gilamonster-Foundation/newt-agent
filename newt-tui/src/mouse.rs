//! Mouse-capture RAII guard + process-level panic-hook chaining (#1303).
//!
//! Feature-gated under the interactive surfaces (`live-spill` / `rich-tui`); a
//! `--no-default-features` wyvern build never links this module. Mouse capture
//! is turned on for the duration of one live-spill turn and released as the
//! turn-scoped [`MouseCaptureGuard`] leaves scope (decision clause B / rule 7):
//!   * normal return / `?` — the guard drops at end of scope;
//!   * panic-unwind — the guard drops while the stack unwinds (newt builds
//!     unwind, never `panic=abort`, so a propagating panic always runs `Drop`);
//!   * the rule-7 abandon / teardown-miss path — that path emits nothing
//!     through the renderer, so release rides this guard's `Drop` (a direct
//!     stdout write), never a renderer write.
//!
//! Release rides the RAII guard (unwind + normal) and its `Drop` only. A
//! CAUGHT/recovered panic — the rule-7 sink `catch_unwind` in `newt-core`'s
//! `run_live_output_dispatch`, the exact path the design expects — does NOT
//! drop the guard and MUST NOT emit disable bytes mid-turn (doing so corrupts
//! the live viewport and clears `CAPTURE_ACTIVE` while capture is physically
//! on). So the panic hook ([`install_panic_release_hook`]) only chains the
//! previous hook; it never emits capture bytes and never touches
//! `CAPTURE_ACTIVE`. Only [`MouseCaptureGuard::enable`] sets the flag and only
//! `Drop` clears it, so the flag can never be cleared while the guard is alive.
//!
//! KNOWN GAP: a signal-driven death (SIGTERM / SIGHUP) runs neither `Drop` nor
//! any hook, so it leaves the terminal in mouse-reporting mode. Signal handling
//! is out of scope for #1303; this is the same follow-on gap the pre-existing
//! `CbreakGuard` raw-mode restore already carries (a SIGTERM/SIGHUP mid-turn
//! already leaves raw mode un-restored), so the mouse tier only adds an
//! incremental symptom on the same signal, not a new failure class.
//!
//! Capture is BUTTON-ONLY (`?1000h` + SGR `?1006h`), deliberately NOT
//! crossterm's `EnableMouseCapture`, which also enables `?1002h`(drag) /
//! `?1003h`(any-motion) reporting — a stdin motion flood we only discard, and
//! the flood that could coalesce a Ctrl-C `0x03` out of a single read. Every
//! emit is idempotent. The guard mirrors `RawGuard` in `lean_input.rs`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Button-only mouse capture: `?1000h` (button press/release reporting) + SGR
/// `?1006h` (extended `ESC[<btn;col;rowM` coordinates). Deliberately narrower
/// than crossterm's `EnableMouseCapture` (`?1000h ?1002h ?1003h ?1015h ?1006h`)
/// — we consume only wheel + left-press, so `?1002h`(drag)/`?1003h`(any-motion)
/// would only flood stdin with events we discard (#1303 FIX A).
const ENABLE_MOUSE_CAPTURE: &[u8] = b"\x1b[?1000h\x1b[?1006h";
/// Release, in reverse: disable SGR ext (`?1006l`) then button reporting
/// (`?1000l`).
const DISABLE_MOUSE_CAPTURE: &[u8] = b"\x1b[?1006l\x1b[?1000l";

/// Tracks whether a `MouseCaptureGuard` is currently alive (capture physically
/// on). Set ONLY by [`MouseCaptureGuard::enable`] and cleared ONLY by its
/// `Drop`, so it can never be cleared while the guard is alive — a caught
/// sink-panic mid-turn leaves it correctly set. (Consulted by the release
/// tests; a future signal-safe restore path would read it too.)
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
    fn emit(&self, bytes: &[u8]) {
        match self {
            Self::Stdout => {
                let mut out = std::io::stdout();
                let _ = out.write_all(bytes);
                let _ = out.flush();
            }
            #[cfg(test)]
            Self::Shared(buf) => {
                let mut guard = buf.lock().unwrap_or_else(|p| p.into_inner());
                guard.extend_from_slice(bytes);
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
        sink.emit(ENABLE_MOUSE_CAPTURE);
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
        // The single release point: fires on normal return, `?`, and while the
        // stack unwinds on a propagating panic. A caught/recovered panic does
        // NOT run this, so mid-turn recovery leaves capture (and the flag) on.
        self.sink.emit(DISABLE_MOUSE_CAPTURE);
        CAPTURE_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// Install — exactly once — a panic hook that CHAINS the previous hook so the
/// default panic message still prints. It deliberately emits nothing and never
/// touches `CAPTURE_ACTIVE`.
///
/// Why no eager release here (#1303 FIX B): a hook fires for EVERY panic,
/// including the rule-7 sink panics that `run_live_output_dispatch`
/// (`newt-core`) `catch_unwind`s and RECOVERS from mid-turn — and the hook
/// cannot tell a recovered panic from a fatal one. Emitting `DisableMouseCapture`
/// on that recovered path corrupted the live viewport, killed mouse reporting
/// for the rest of the turn, and cleared `CAPTURE_ACTIVE` while the guard was
/// still alive and capturing. Since newt always unwinds (no `panic=abort`), a
/// FATAL panic propagates through [`MouseCaptureGuard`]'s scope and its `Drop`
/// releases capture during the unwind — so the hook has nothing to add and must
/// stay byte-silent. (A double-panic → `abort` bypasses `Drop`, a rare gap
/// shared with the `CbreakGuard` raw-mode restore; see the module docs.)
pub fn install_panic_release_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // No emit, no flag mutation — release rides `MouseCaptureGuard::Drop`.
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

    #[serial_test::serial(mouse_capture_active)]
    #[test]
    fn guard_enables_on_construct_and_disables_on_drop() {
        let (sink, buf) = shared();
        {
            let _guard = MouseCaptureGuard::enable(sink);
            let enabled = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
            // Button-only capture: `?1000h` (button) + SGR `?1006h`, and NOTHING
            // else — no `?1002`(drag)/`?1003`(any-motion) flood (#1303 FIX A).
            assert!(
                enabled.contains("\u{1b}[?1000h"),
                "enable turns on button reporting: {enabled:?}"
            );
            assert!(
                enabled.contains("\u{1b}[?1006h"),
                "enable turns on SGR ext: {enabled:?}"
            );
            assert!(
                !enabled.contains("\u{1b}[?1002") && !enabled.contains("\u{1b}[?1003"),
                "no drag / any-motion tracking: {enabled:?}"
            );
            assert!(!enabled.contains("\u{1b}[?1006l"), "not yet disabled");
        }
        // Dropping the guard releases capture (`?1006l` then `?1000l`).
        let after = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            after.contains("\u{1b}[?1006l") && after.contains("\u{1b}[?1000l"),
            "drop emitted disable: {after:?}"
        );
    }

    #[test]
    fn maybe_false_emits_nothing() {
        // The opted-out / keyboard tier holds no guard and writes no bytes.
        assert!(MouseCaptureGuard::maybe(false).is_none());
    }

    #[serial_test::serial(mouse_capture_active)]
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

    /// #1303 FIX B: a CAUGHT sink panic (the rule-7 `catch_unwind` in
    /// `run_live_output_dispatch`) fires the process panic hook mid-turn while
    /// the guard is still alive. The hook must NOT disable capture and must NOT
    /// clear `CAPTURE_ACTIVE` — otherwise the live viewport is corrupted and a
    /// later genuine abort would emit no release. Here the guard stays alive
    /// ACROSS the recovered panic (as it does across a real sink panic): no
    /// disable bytes may appear and the flag must stay set until the guard drops.
    #[serial_test::serial(mouse_capture_active)]
    #[test]
    fn caught_sink_panic_does_not_release_capture_or_clear_flag() {
        // The real binary installs this at entry; install it here so the test
        // exercises the SAME hook a caught panic would trip mid-turn.
        crate::install_panic_release_hook();

        let (sink, buf) = shared();
        let guard = MouseCaptureGuard::enable(sink);
        assert!(
            CAPTURE_ACTIVE.load(Ordering::SeqCst),
            "capture flag set while the turn's guard is alive"
        );

        // Simulate the rule-7 sink panic the design expects and RECOVERS from,
        // mid-turn, without dropping the guard.
        let recovered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("sink.finish() blew up (rule-7 teardown-miss)");
        }));
        assert!(recovered.is_err(), "the sink panic was caught & recovered");

        // No disable bytes may have been emitted on the recovered path — the
        // guard did not drop, and the hook is byte-silent.
        let mid = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            !mid.contains("\u{1b}[?1006l") && !mid.contains("\u{1b}[?1000l"),
            "no disable emitted while recovering mid-turn: {mid:?}"
        );
        // And the flag still reflects the live guard.
        assert!(
            CAPTURE_ACTIVE.load(Ordering::SeqCst),
            "capture flag stays set after a recovered panic"
        );

        // Normal release still happens when the guard finally drops.
        drop(guard);
        let after = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            after.contains("\u{1b}[?1006l"),
            "guard Drop releases on scope exit: {after:?}"
        );
        assert!(
            !CAPTURE_ACTIVE.load(Ordering::SeqCst),
            "flag cleared once the guard drops"
        );
    }
}
