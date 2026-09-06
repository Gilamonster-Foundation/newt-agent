//! Reading the terminal while a turn runs: decode keypresses, watch for an
//! interrupt, and restore cbreak on the way out.
//!
//! Almost all of this is `cfg(unix)` — there is no termios elsewhere — so
//! `with_live_spill_watch` has a `cfg(not(unix))` twin that just calls the
//! closure. The mouse/CSI decoding on top is additionally `feature =
//! "live-spill"`. Every attribute is kept verbatim from `lib.rs`; see the
//! per-config table in the PR that moved this.

use super::*;

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnKey {
    Up,
    Down,
    ToggleExpanded,
    /// Ctrl-t (#1704): expand the spill viewport to half the visible console
    /// height — a middle stop between collapsed and full-expand (Space).
    ExpandHalf,
    // #1303 step 5: editor-mode nav targets (vi `gg`/`G`/`C-d`/`C-u`, emacs
    // paging). Produced and dispatched only under `live-spill` — the wyvern
    // build never links them.
    #[cfg(feature = "live-spill")]
    Top,
    #[cfg(feature = "live-spill")]
    Bottom,
    #[cfg(feature = "live-spill")]
    HalfPageUp,
    #[cfg(feature = "live-spill")]
    HalfPageDown,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TurnKeyState {
    #[default]
    Ground,
    Escape,
    Csi,
    Ss3,
    // #1303 FIX C: legacy X10 mouse encoding (`ESC[M` then 3 raw bytes
    // Cb,Cx,Cy) — a terminal that honors `?1000` but not the SGR `?1006`
    // reports here. Consume the 3 bytes (`remaining` counts down) so they never
    // leak as ground keys. Only reachable under `live-spill` (mouse capture).
    #[cfg(feature = "live-spill")]
    X10Mouse {
        remaining: u8,
    },
    // #1303 FIX E follow-up: a CSI whose params overflowed the cap. The rest
    // of the malformed sequence is swallowed here until its terminator —
    // resyncing to Ground mid-sequence would leak the tail as ground bytes,
    // which type-ahead capture now makes visible as prefill garbage.
    #[cfg(feature = "live-spill")]
    CsiDiscard,
}

/// #1303 FIX E: cap on accumulated CSI parameter bytes. A well-behaved SGR-mouse
/// param run (`<65;9999;9999`) is ~13 bytes; 32 is generous. A non-terminating
/// or malformed CSI (continuous `;` with no `0x40..=0x7e` terminator) is dropped
/// at the cap and the decoder resyncs to Ground rather than grow without bound.
#[cfg(all(unix, feature = "live-spill"))]
const MAX_CSI_PARAM_BYTES: usize = 32;

#[cfg(unix)]
#[derive(Default)]
struct TurnKeyDecoder {
    state: TurnKeyState,
    // #1303: accumulated CSI parameter bytes, for SGR-mouse decode. Only ever
    // populated under `live-spill` — the wyvern build never enables capture, so
    // never sees mouse bytes, so this field does not exist there.
    #[cfg(feature = "live-spill")]
    params: Vec<u8>,
    // #1303 step 5: the resolved editor keybinding for viewport nav, plus the
    // `gg` two-key latch (vi). `default()` yields `EditMode`'s own default; the
    // watcher builds the decoder with the live-resolved mode via `with_mode`.
    #[cfg(feature = "live-spill")]
    mode: newt_core::EditMode,
    #[cfg(feature = "live-spill")]
    pending_g: bool,
    // #1303 FIX F: the editor-mode nav keys (vi `j`/`k`/`gg`/`G`/`C-d`/`C-u`,
    // emacs `C-n`/`C-p`/`C-v`) only activate with the mouse opt-in — the
    // decision keeps the keyboard tier unchanged for operators who don't opt in.
    // `false` (the default) = base keys only (`↑`/`↓`/`Space`/`Enter`); the
    // watcher sets it from the resolved mouse-tier flag. Base keys are always on.
    #[cfg(feature = "live-spill")]
    mode_nav: bool,
    // Persistent-prompt phase 1 (docs/decisions/persistent_prompt.md):
    // ground-state bytes that are neither interrupts nor viewport-nav keys used
    // to be DROPPED here — typing during a turn vanished. They now accumulate
    // as type-ahead text; the watcher drains this into `type_ahead` ONCE at
    // exit (a per-read drain would reset the Space/Enter latch and backspace
    // editing between keystrokes) and the next prompt pre-fills with it.
    text: Vec<u8>,
}

#[cfg(all(unix, feature = "live-spill"))]
impl TurnKeyDecoder {
    /// Build a decoder bound to the session's editor keybinding WITH mode-aware
    /// nav enabled — the mouse-tier (opt-in ON) constructor. Base keys
    /// (`↑`/`↓`/`Space`/`Enter`) work in every mode regardless; the mode-aware
    /// keys (`j`/`k`/`gg`… ) additionally activate here. Resolved once per spill
    /// turn by the watcher, never on the hot path of a non-spill turn.
    fn with_mode(mode: newt_core::EditMode) -> Self {
        Self {
            mode,
            mode_nav: true,
            ..Default::default()
        }
    }
}

#[cfg(unix)]
impl TurnKeyDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Vec<TurnKey> {
        let mut keys = Vec::new();
        for &byte in bytes {
            self.state = match self.state {
                TurnKeyState::Ground if byte == 0x1b => {
                    // #1303 FIX D: an escape sequence (arrow / SS3 / SGR or X10
                    // mouse / Alt-chord) is a non-`g` event — clear the vi `gg`
                    // latch so a `g` before it can't mis-fire `Top` on a later
                    // `g`. Every escape sequence starts with this `0x1b`, so one
                    // clear here covers CSI/SS3/mouse/escape alike. The armed
                    // `g` was typed text, not nav — keep it in the prefill.
                    #[cfg(feature = "live-spill")]
                    if std::mem::take(&mut self.pending_g) {
                        self.push_text_byte(b'g');
                    }
                    TurnKeyState::Escape
                }
                TurnKeyState::Ground => {
                    self.push_ground_key(byte, &mut keys);
                    TurnKeyState::Ground
                }
                TurnKeyState::Escape if byte == b'[' => {
                    // A fresh CSI — reset the mouse-parameter accumulator.
                    #[cfg(feature = "live-spill")]
                    self.params.clear();
                    TurnKeyState::Csi
                }
                TurnKeyState::Escape if byte == b'O' => TurnKeyState::Ss3,
                TurnKeyState::Escape if byte == 0x1b => TurnKeyState::Escape,
                TurnKeyState::Escape => TurnKeyState::Ground,
                TurnKeyState::Csi if (0x40..=0x7e).contains(&byte) => {
                    self.push_csi_terminal(byte, &mut keys)
                }
                TurnKeyState::Csi if byte == 0x1b => TurnKeyState::Escape,
                TurnKeyState::Csi => {
                    // Accumulate parameter/intermediate bytes (`0x20..=0x3f`,
                    // e.g. `<`, digits, `;`) for the SGR-mouse terminal decode.
                    // #1303 FIX E: bounded — on overflow, drop the malformed /
                    // non-terminating CSI and resync to Ground.
                    #[cfg(feature = "live-spill")]
                    {
                        if (0x20..=0x3f).contains(&byte) {
                            if self.params.len() >= MAX_CSI_PARAM_BYTES {
                                self.params.clear();
                                TurnKeyState::CsiDiscard
                            } else {
                                self.params.push(byte);
                                TurnKeyState::Csi
                            }
                        } else {
                            TurnKeyState::Csi
                        }
                    }
                    #[cfg(not(feature = "live-spill"))]
                    {
                        TurnKeyState::Csi
                    }
                }
                #[cfg(feature = "live-spill")]
                TurnKeyState::CsiDiscard if byte == 0x1b => TurnKeyState::Escape,
                #[cfg(feature = "live-spill")]
                TurnKeyState::CsiDiscard if (0x40..=0x7e).contains(&byte) => TurnKeyState::Ground,
                #[cfg(feature = "live-spill")]
                TurnKeyState::CsiDiscard => TurnKeyState::CsiDiscard,
                #[cfg(feature = "live-spill")]
                TurnKeyState::X10Mouse { remaining } => {
                    // #1303 FIX C: consume Cb,Cx,Cy. Decode the button from the
                    // FIRST byte (Cb); discard the two coordinate bytes so a
                    // coord that happens to equal `j`/`g`/space never leaks as a
                    // ground key. Split across reads is handled by the state.
                    if remaining == 3 {
                        if let Some(key) = Self::x10_button_key(byte) {
                            keys.push(key);
                        }
                    }
                    match remaining - 1 {
                        0 => TurnKeyState::Ground,
                        left => TurnKeyState::X10Mouse { remaining: left },
                    }
                }
                TurnKeyState::Ss3 => {
                    match byte {
                        b'A' => keys.push(TurnKey::Up),
                        b'B' => keys.push(TurnKey::Down),
                        _ => {}
                    }
                    TurnKeyState::Ground
                }
            };
        }
        keys
    }

    /// A CSI terminal byte closed the sequence: an SGR-mouse event (params carry
    /// a `<` intro), the legacy X10 mouse form (`ESC[M` with no `<`), or a plain
    /// arrow. Returns the next decoder state — `X10Mouse` when 3 raw bytes must
    /// still be consumed, else `Ground`.
    fn push_csi_terminal(&self, byte: u8, keys: &mut Vec<TurnKey>) -> TurnKeyState {
        #[cfg(feature = "live-spill")]
        {
            // SGR form (`ESC[<btn;col;rowM`): params begin with `<`.
            if let Some(key) = self.mouse_key_for(byte) {
                keys.push(key);
                return TurnKeyState::Ground;
            }
            // #1303 FIX C: legacy X10 form is `ESC[M` (terminal `M`) with NO
            // SGR `<` params — 3 raw bytes (Cb,Cx,Cy) follow. Route to the
            // consuming state so they don't leak as ground keys. (An SGR event
            // whose button we ignore, e.g. right-click, keeps its `<` params and
            // is NOT mistaken for X10.)
            if byte == b'M' && self.params.first() != Some(&b'<') {
                return TurnKeyState::X10Mouse { remaining: 3 };
            }
        }
        match byte {
            b'A' => keys.push(TurnKey::Up),
            b'B' => keys.push(TurnKey::Down),
            _ => {}
        }
        TurnKeyState::Ground
    }

    /// #1303 FIX C: map an X10 mouse button byte (Cb = button + 32) to a nav
    /// action, mirroring [`Self::mouse_key_for`]'s SGR button mapping. Wheel-up
    /// = 64, wheel-down = 65, left-press = 0; other buttons / releases are
    /// ignored. The coordinate bytes (Cx,Cy) are consumed by the caller and
    /// never decoded.
    #[cfg(feature = "live-spill")]
    fn x10_button_key(cb: u8) -> Option<TurnKey> {
        match cb.wrapping_sub(32) {
            0 => Some(TurnKey::ToggleExpanded),
            64 => Some(TurnKey::Up),
            65 => Some(TurnKey::Down),
            _ => None,
        }
    }

    /// Decode an SGR-mouse event from the accumulated params + terminal byte:
    /// `ESC [ < btn ; col ; row (M|m)`. Only the press form (`M`) reports —
    /// wheels have no release and a click's release (`m`) is a deliberate no-op,
    /// so one click = one toggle. Wheel-up = 64 → scroll toward older, wheel-down
    /// = 65 → scroll toward newer, plain left-press = 0 → expand/collapse. A
    /// bare left click toggles regardless of which frame row it lands on:
    /// per-glyph hit-testing (the `⧉`/`▣`/`▲`/`▼` targets) needs the renderer's
    /// screen geometry, a refinement seam left for a follow-up. Right/middle
    /// buttons and drag/motion (button ≥ 32) are ignored.
    #[cfg(feature = "live-spill")]
    fn mouse_key_for(&self, final_byte: u8) -> Option<TurnKey> {
        if final_byte != b'M' {
            return None;
        }
        let params = std::str::from_utf8(&self.params).ok()?;
        let btn = params
            .strip_prefix('<')?
            .split(';')
            .next()?
            .parse::<u32>()
            .ok()?;
        match btn {
            0 => Some(TurnKey::ToggleExpanded),
            64 => Some(TurnKey::Up),
            65 => Some(TurnKey::Down),
            _ => None,
        }
    }

    /// A Ground-state byte (not part of an escape sequence). The base keys —
    /// `Space`/`Enter` → expand — keep their live-spill contract while nothing
    /// has been typed; once type-ahead has begun they are text (the user is
    /// writing a message, not steering a viewport). Editor-mode nav (when opted
    /// in) is layered on top; every remaining ground byte becomes type-ahead
    /// text instead of vanishing.
    fn push_ground_key(&mut self, byte: u8, keys: &mut Vec<TurnKey>) {
        // Ctrl-t (#1704): expand to half the console. A control key, never
        // type-ahead text, and valid whether or not the buffer is empty.
        if byte == 0x14 {
            keys.push(TurnKey::ExpandHalf);
            #[cfg(feature = "live-spill")]
            {
                self.pending_g = false;
            }
            return;
        }
        if matches!(byte, b' ' | b'\r' | b'\n') && self.text.is_empty() {
            keys.push(TurnKey::ToggleExpanded);
            #[cfg(feature = "live-spill")]
            {
                self.pending_g = false;
            }
            return;
        }
        // Editor-mode nav (live-spill only); the lean build has none. A byte
        // the active mode consumes is nav, never text.
        #[cfg(feature = "live-spill")]
        if self.push_mode_ground_key(byte, keys) {
            return;
        }
        self.push_text_byte(byte);
    }

    /// Accumulate one ground byte as type-ahead text. Backspace edits in place
    /// (UTF-8 aware), CR/LF normalize to `\n`, tabs soften to a space, and the
    /// remaining C0 controls are dropped — everything else (including UTF-8
    /// lead/continuation bytes) is kept verbatim for the lossy decode at drain.
    fn push_text_byte(&mut self, byte: u8) {
        match byte {
            0x08 | 0x7f => {
                // Pop one whole character: continuation bytes (0x80..=0xbf)
                // fall until the lead (or ASCII) byte that starts the char.
                while let Some(popped) = self.text.pop() {
                    if !(0x80..=0xbf).contains(&popped) {
                        break;
                    }
                }
            }
            b'\r' | b'\n' => self.text.push(b'\n'),
            b'\t' => self.text.push(b' '),
            0x20..=0xff => self.text.push(byte),
            _ => {}
        }
    }

    /// Drain the accumulated type-ahead bytes (the watcher forwards them to
    /// [`type_ahead`] after every read).
    fn take_text(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.text)
    }

    /// Editor-mode-aware viewport nav (#1303 clause 4). vi is implemented fully
    /// (`j`/`k` line, `gg`/`G` top/bottom, `C-d`/`C-u` half-page); emacs gets
    /// `C-n`/`C-p`/`C-v` (line + page-down). nano rides the universal arrows.
    /// Modes' remaining bindings (emacs `M-v`/`M-<`/`M->`, nano `C-y`/`M-\`) are
    /// a documented follow-on seam — the `↑`/`↓`/`Space` base always works.
    /// Returns `true` when the byte was consumed as a nav key (or armed the vi
    /// `gg` latch); `false` hands it to type-ahead text.
    #[cfg(feature = "live-spill")]
    fn push_mode_ground_key(&mut self, byte: u8, keys: &mut Vec<TurnKey>) -> bool {
        use newt_core::EditMode;
        // #1303 FIX F: the mode-aware keys only fire with the mouse opt-in. When
        // it's off (keyboard tier / opted-out), this is a no-op — `j`/`k`/`gg`…
        // are plain type-ahead text — while the base keys (`↑`/`↓` and the
        // empty-buffer `Space`/`Enter`, handled in `push_ground_key` and the
        // CSI/SS3 arms) stay unconditional.
        if !self.mode_nav {
            return false;
        }
        // `gg` (vi) is the only two-key sequence: a pending `g` consumes the
        // next byte. `gg` → Top; anything else re-processes the byte normally —
        // and the armed `g` turns out to have been typed TEXT, so flush it to
        // the type-ahead buffer rather than dropping it ("great" must not
        // prefill as "reat").
        if std::mem::take(&mut self.pending_g) {
            if byte == b'g' {
                keys.push(TurnKey::Top);
                return true;
            }
            self.push_text_byte(b'g');
        }
        match self.mode {
            EditMode::Vi => match byte {
                b'j' => keys.push(TurnKey::Down),
                b'k' => keys.push(TurnKey::Up),
                b'G' => keys.push(TurnKey::Bottom),
                b'g' => self.pending_g = true,
                0x04 => keys.push(TurnKey::HalfPageDown), // C-d
                0x15 => keys.push(TurnKey::HalfPageUp),   // C-u
                _ => return false,
            },
            EditMode::Emacs => match byte {
                0x0e => keys.push(TurnKey::Down),         // C-n
                0x10 => keys.push(TurnKey::Up),           // C-p
                0x16 => keys.push(TurnKey::HalfPageDown), // C-v (page down)
                _ => return false,
            },
            // nano is modeless/emacs-like; the universal arrows already cover it.
            EditMode::Nano => return false,
        }
        true
    }
}

#[cfg(unix)]
fn dispatch_turn_keys(decoder: &mut TurnKeyDecoder, bytes: &[u8], spill: Option<&dyn SpillInput>) {
    let Some(spill) = spill else {
        let _ = decoder.feed(bytes);
        return;
    };
    for key in decoder.feed(bytes) {
        match key {
            TurnKey::Up => {
                spill.scroll_up();
            }
            TurnKey::Down => {
                spill.scroll_down();
            }
            TurnKey::ToggleExpanded => {
                spill.toggle_expanded();
            }
            TurnKey::ExpandHalf => {
                spill.expand_half();
            }
            #[cfg(feature = "live-spill")]
            TurnKey::Top => {
                spill.scroll_to_top();
            }
            #[cfg(feature = "live-spill")]
            TurnKey::Bottom => {
                spill.scroll_to_bottom();
            }
            #[cfg(feature = "live-spill")]
            TurnKey::HalfPageUp => {
                spill.half_page_up();
            }
            #[cfg(feature = "live-spill")]
            TurnKey::HalfPageDown => {
                spill.half_page_down();
            }
        }
    }
}

// #1303 step 3/4/5: SGR-mouse + editor-mode keyboard decode is `live-spill`
// mouse code — stripped from the wyvern build, so these tests are too.
#[cfg(all(test, unix, feature = "live-spill"))]
#[path = "lib_tests/mouse_decode_tests.rs"]
mod mouse_decode_tests;

#[cfg(all(test, unix))]
#[path = "lib_tests/interrupt_tests.rs"]
mod interrupt_tests;

/// Run `f` (the in-place turn) with an Esc watcher active, returning `f`'s value.
/// When `enabled` is false (piped / non-TTY) or the terminal can't be put in
/// cbreak, it simply runs `f` with no watcher. The terminal mode is always
/// restored before returning (RAII), and the watcher thread is joined.
#[cfg(unix)]
pub(crate) fn with_live_spill_watch<T>(
    enabled: bool,
    cancel: &std::sync::atomic::AtomicBool,
    mouse: bool,
    spill: Option<&dyn SpillInput>,
    f: impl FnOnce() -> T,
) -> T {
    // A new turn can never legitimately begin with an interrupt already
    // pending. Clearing on ENTRY (before any early return) also heals a flag a
    // previous turn leaked through a path that could not clear — e.g. a
    // watcherless turn after cbreak starts failing.
    newt_core::tty::set_interrupt_pending(false);
    if !enabled {
        return f();
    }
    let _cbreak = match CbreakGuard::enter() {
        Ok(guard) => guard,
        Err(err) => {
            // Losing the watcher silently is how "Esc/Ctrl-C does nothing"
            // becomes undiagnosable — say so once, before the turn paints.
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!(
                    "⚠ terminal mode unavailable ({err}) — Esc/Ctrl-C cannot interrupt this session"
                );
            });
            return f();
        }
    };
    // #1303: mouse capture is turn-scoped and released on EVERY exit path. The
    // guard drops when this scope unwinds — normal return, `?`, or panic — and
    // its `Drop` is a direct stdout write (NOT a renderer write), so the rule-7
    // abandon path (contractually I/O-free through the renderer) still releases.
    // `None` on the keyboard tier / opt-out; nothing is emitted then.
    // Tell the marker vocabulary whether a click is a true statement here, so
    // a fold can advertise one only where something is listening (#1263).
    #[cfg(any(feature = "rich-tui", feature = "live-spill"))]
    newt_core::agentic::set_mouse_recovery(mouse);
    #[cfg(any(feature = "rich-tui", feature = "live-spill"))]
    let _mouse_capture = crate::mouse::MouseCaptureGuard::maybe(mouse);
    #[cfg(not(any(feature = "rich-tui", feature = "live-spill")))]
    let _ = mouse;
    // #1303 step 5 + FIX F: the editor-mode nav keys only activate WITH the
    // mouse opt-in — the decision leaves the keyboard tier unchanged for
    // operators who don't opt in. So resolve the keybinding (a disk read, in
    // production only) ONLY when `mouse` is on, and pass `mouse` as the decoder's
    // `mode_nav` gate. When off, `mode` is unused (nav disabled) and the base
    // keys still work. Unit tests drive the watcher with an explicit mode.
    #[cfg(feature = "live-spill")]
    let mode = if mouse && spill.is_some() {
        resolve_edit_mode()
    } else {
        newt_core::EditMode::default()
    };
    #[cfg(not(feature = "live-spill"))]
    let mode = newt_core::EditMode::default();
    // Signals the watcher to exit from Drop, so it fires on the normal return
    // AND on a panicking `f()` — without it, a panic would skip the store and
    // `thread::scope`'s implicit join would wait forever on a watcher whose
    // only exit condition never arrives.
    struct StopOnExit<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for StopOnExit<'_> {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    // Clears the acknowledgment flag from Drop AFTER `thread::scope` has
    // joined the watcher (this guard is declared before the scope, so it drops
    // after it — on the panic path too). Clearing before the join would race
    // the watcher's final iteration: a Ctrl-C landing right as the turn ends
    // could re-raise the flag after the clear and stick the "interrupting…"
    // label through the entire next turn.
    struct ClearInterruptOnExit;
    impl Drop for ClearInterruptOnExit {
        fn drop(&mut self) {
            newt_core::tty::set_interrupt_pending(false);
        }
    }
    let _clear_flag = ClearInterruptOnExit;
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        // The watcher polls `stop` with a 100 ms timeout, so it wakes and
        // returns promptly once the guard fires, and the scope joins it before
        // restoring the tty.
        let _stop_watcher = StopOnExit(&stop);
        s.spawn(|| watch_for_interrupt(cancel, &stop, spill, mode, mouse));
        f()
    })
}

#[cfg(not(unix))]
pub(crate) fn with_live_spill_watch<T>(
    _enabled: bool,
    _cancel: &std::sync::atomic::AtomicBool,
    _mouse: bool,
    _spill: Option<&dyn SpillInput>,
    f: impl FnOnce() -> T,
) -> T {
    // No termios on non-unix; the interrupt watcher is unix-only for now.
    f()
}

pub(crate) fn with_interrupt_watch<T>(
    enabled: bool,
    cancel: &std::sync::atomic::AtomicBool,
    f: impl FnOnce() -> T,
) -> T {
    // No live spill viewport ⇒ no mouse tier.
    with_live_spill_watch(enabled, cancel, false, None, f)
}

/// Poll stdin while the turn runs; trip `cancel` on the first interrupt (a lone
/// Esc or Ctrl-C) and count EVERY press for the spinner's acknowledgment
/// (#2010). Keeps watching so a follow-up press is heard, until `stop` is set
/// (the turn finished) — polling with a 100 ms timeout so it never blocks
/// past the turn's end.
#[cfg(unix)]
fn watch_for_interrupt(
    cancel: &std::sync::atomic::AtomicBool,
    stop: &std::sync::atomic::AtomicBool,
    spill: Option<&dyn SpillInput>,
    mode: newt_core::EditMode,
    mode_nav: bool,
) {
    watch_for_interrupt_fd(
        libc::STDIN_FILENO,
        cancel,
        stop,
        spill,
        mode,
        mode_nav,
        100,
        200,
    );
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn watch_for_interrupt_fd(
    fd: libc::c_int,
    cancel: &std::sync::atomic::AtomicBool,
    stop: &std::sync::atomic::AtomicBool,
    spill: Option<&dyn SpillInput>,
    mode: newt_core::EditMode,
    mode_nav: bool,
    poll_timeout_ms: libc::c_int,
    escape_grace_ms: libc::c_int,
) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    let mut buf = [0u8; 64];
    // #1303 step 5 + FIX F: bind the decoder to the session's editor keybinding,
    // but activate the mode-aware nav keys ONLY when `mode_nav` (the mouse
    // opt-in) is on — the base keys (`↑`/`↓`/`Space`/`Enter`) work either way.
    // The lean build has no nav modes.
    #[cfg(feature = "live-spill")]
    let mut decoder = if mode_nav {
        TurnKeyDecoder::with_mode(mode)
    } else {
        TurnKeyDecoder::default()
    };
    #[cfg(not(feature = "live-spill"))]
    let mut decoder = {
        let _ = mode;
        let _ = mode_nav;
        TurnKeyDecoder::default()
    };
    while !stop.load(Ordering::Relaxed) {
        if let Some(spill) = spill {
            spill.refresh_geometry();
        }
        let Some(_stdin) = try_watch_stdin() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, poll_timeout_ms) };
        if n <= 0 || pfd.revents & libc::POLLIN == 0 {
            continue; // timeout or spurious — re-check `stop`
        }
        let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if r <= 0 {
            continue;
        }
        let bytes = &buf[..r as usize];
        let mut interrupt = is_ctrl_c(bytes);
        if !interrupt && is_lone_esc(bytes) {
            // Guard against a split escape sequence (Esc arriving in a separate
            // read from its `[A` tail under load): wait briefly for a
            // continuation. None arriving → a real Esc press.
            let mut pfd2 = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let m = unsafe { libc::poll(&mut pfd2, 1, escape_grace_ms) };
            if m <= 0 {
                // A real lone Esc press. #1704: if the spill viewport is in
                // explore mode (scrolled back off the tail), the FIRST Esc
                // leaves explore mode and restores follow-tail — it must NOT
                // cancel the turn. Only an Esc pressed while following the tail
                // (or with no live spill) is an interrupt.
                if spill.is_some_and(|s| s.is_exploring()) {
                    if let Some(s) = spill {
                        s.exit_explore();
                    }
                } else {
                    interrupt = true;
                }
            } else {
                // Feed Esc and its continuation through one persistent decoder;
                // `[A`/`[B` may themselves be split across later reads.
                dispatch_turn_keys(&mut decoder, bytes, spill);
                let r2 = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                if r2 > 0 {
                    dispatch_turn_keys(&mut decoder, &buf[..r2 as usize], spill);
                }
            }
        } else if !interrupt {
            dispatch_turn_keys(&mut decoder, bytes, spill);
        }
        if interrupt {
            // Graceful interrupt: the turn drops its in-flight request or tool
            // future (`cancellable` in `agentic`) and hands control back to
            // the prompt. `cancel` is a one-way latch, so a repeat is
            // harmless — and it is COUNTED (#2010): the spinner swaps its
            // label within one tick on the 1st press and shows the running
            // count on every press after it, so a slow cancel never reads as
            // a dropped keystroke. There is no second tier: the first press
            // already aborts everything a second one could, so a repeat is
            // honestly answered with "heard — already stopping".
            cancel.store(true, Ordering::Relaxed);
            newt_core::tty::note_interrupt_press();
        }
    }
    // Persistent-prompt phase 1: whatever the decoder classified as text (not
    // interrupts, not nav) becomes type-ahead for the next prompt. Drained
    // ONCE at watcher exit — NOT per read: interactive typing arrives one byte
    // per read, so a per-read drain would empty the decoder's buffer between
    // every keystroke, defeating the Space/Enter "typing has begun" latch and
    // backspace editing (spaces would toggle the viewport and vanish from the
    // prefill). The prompt reads the buffer only after `thread::scope` joins
    // this thread, so the single late push is always visible in time.
    type_ahead::push_bytes(&decoder.take_text());
}

/// RAII cbreak: ICANON + ECHO + ISIG off (per-keystroke, no echo, and Ctrl-C
/// delivered as a raw `0x03` byte rather than a SIGINT) so the keyboard watcher
/// can treat Ctrl-C as a tiered *interrupt* (#530-followup) instead of letting
/// it kill the process mid-turn. OPOST stays ON so streamed output keeps CR-NL
/// translation. Restores the saved attributes on drop.
#[cfg(unix)]
struct CbreakGuard {
    fd: libc::c_int,
    orig: libc::termios,
}

#[cfg(unix)]
impl CbreakGuard {
    fn enter() -> io::Result<Self> {
        let fd = libc::STDIN_FILENO;
        unsafe {
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = orig;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, orig })
        }
    }
}

#[cfg(unix)]
impl Drop for CbreakGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}
