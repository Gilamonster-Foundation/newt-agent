//! The process-wide terminal-line arbiter, its RAII leases, and the
//! [`PromptWindow`] capability token.
//!
//! # The two halves
//!
//! **RAII lease (dynamic, cross-stack).** A spinner and a permission prompt sit
//! in different call stacks, so no purely-static scheme can relate them.
//! Ephemeral writers register with this singleton and hold a lease on the
//! bottom line; [`Terminal::suspend_for_prompt`] erases and quiesces every one
//! of them before it returns.
//!
//! **Capability typestate (static, compile-time).** [`PromptWindow`] has no
//! public constructor — its only field is a private sealed ZST, so no crate can
//! build one with struct-literal syntax either. Every function that may block
//! on a human takes `&PromptWindow`. You cannot obtain the argument without
//! having suspended, so *a prompt printed onto a live spinner does not compile*.
//! The failure mode is not "remembered"; it is unrepresentable.
//!
//! This generalizes two disciplines the codebase already trusts: the
//! `LiveOutputSession` RAII `Drop → finish()` pattern, and the `StdinOwnership`
//! singleton (a `OnceLock<(Mutex<_>, Condvar)>`) that the permission prompt used
//! for the *other* half of the terminal. Stdin ownership moves in here so one
//! object arbitrates both directions.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{execute, queue};

use super::caps::LineCaps;

/// Which stream an ephemeral writer paints on.
///
/// Explicit and defaulted nowhere on purpose: the setup wizard and the model
/// downloader write progress to **stderr**, and silently relocating those bytes
/// to stdout would break someone's `2>/dev/null`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Sink {
    Stdout,
    Stderr,
}

impl Sink {
    fn with<R>(self, f: impl FnOnce(&mut LineWriter<'_>) -> R) -> R {
        match self {
            Self::Stdout => {
                let stdout = io::stdout();
                let mut lock = stdout.lock();
                f(&mut LineWriter(&mut lock))
            }
            Self::Stderr => {
                let stderr = io::stderr();
                let mut lock = stderr.lock();
                f(&mut LineWriter(&mut lock))
            }
        }
    }
}

/// A **sized** `Write` handle over the locked sink.
///
/// crossterm's `execute!`/`queue!` call `by_ref()`, which a bare
/// `&mut dyn Write` cannot provide. Erasing the concrete stream behind this
/// newtype keeps [`Sink`] a runtime choice while still letting every draw go
/// through the crossterm macros — so the erase stays ONE implementation instead
/// of a per-stream copy.
pub struct LineWriter<'a>(&'a mut (dyn Write + 'a));

impl Write for LineWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// An ephemeral writer: owns rows it can erase on demand.
///
/// Implemented by the unified `Spinner` today; the live-spill viewport joins in
/// a later step (it is the workspace's other cursor owner, and its
/// `Clear(FromCursorDown)` rewind can destroy rows it does not own).
pub trait Ephemeral: Send + Sync {
    /// Erase every row this writer painted; leave the cursor at column 0 of the
    /// first row it owned. Must be idempotent.
    fn erase(&self);
    /// Repaint after a suspension ends. May be a no-op — the shared ticker will
    /// repaint a spinner on its own within one frame.
    fn restore(&self);
}

// ---------------------------------------------------------------------------
// The singleton
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    /// At most ONE writer may own the ephemeral line at a time.
    line_held: bool,
    /// Registered ephemerals, weakly held so a leaked handle cannot pin them.
    registered: Vec<(u64, Weak<dyn Ephemeral>)>,
    /// A `PromptWindow` is alive: every writer paints nothing.
    suspended: bool,
    next_id: u64,
    /// Stdin ownership — the other half of the terminal. `prompt_owner` is the
    /// thread currently blocked on a human; `watcher_reading` is the turn
    /// watcher's exclusive read token. A prompt cannot enter while the watcher
    /// reads, and the watcher cannot acquire while a prompt owns stdin, which
    /// closes the check-then-read race at permission transitions.
    prompt_owner: Option<std::thread::ThreadId>,
    prompt_depth: usize,
    watcher_reading: bool,
}

fn arbiter() -> &'static (Mutex<Inner>, Condvar) {
    static ARBITER: OnceLock<(Mutex<Inner>, Condvar)> = OnceLock::new();
    ARBITER.get_or_init(|| (Mutex::new(Inner::default()), Condvar::new()))
}

fn lock() -> MutexGuard<'static, Inner> {
    arbiter()
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Every registered ephemeral still alive, pruning the dead weak refs as it
/// goes. The caller must release the lock before erasing any of them — erasing
/// writes to the terminal and must not happen under the arbiter's mutex.
fn live_ephemerals(state: &mut Inner) -> Vec<Arc<dyn Ephemeral>> {
    state.registered.retain(|(_, w)| w.strong_count() > 0);
    state
        .registered
        .iter()
        .filter_map(|(_, w)| w.upgrade())
        .collect()
}

/// Is a `PromptWindow` alive right now? The shared ticker checks this before
/// every frame, so a redraw can never race a question onto the screen.
pub(crate) fn suspended() -> bool {
    lock().suspended
}

/// A **non-exclusive** registration with the arbiter (#1410). Deregisters on
/// drop.
///
/// Distinct from [`LineLease`] on purpose. A lease is exclusive ownership of
/// the one ephemeral bottom row and carries that row's erase strategy. This
/// carries no row, no erase strategy and no exclusion — only the promise that
/// [`Terminal::suspend_for_prompt`] will call [`Ephemeral::erase`] before a
/// question renders, and the right to ask whether a question is on screen.
///
/// A multi-row surface that owns its own geometry registers; it does not lease.
#[must_use = "dropping the registration immediately deregisters the ephemeral"]
pub struct EphemeralRegistration {
    id: u64,
}

impl EphemeralRegistration {
    /// Is a [`PromptWindow`] alive right now? A registered ephemeral **must
    /// not** paint while this is true: `suspend_for_prompt` has already erased
    /// it, and a repaint would land on top of the question.
    ///
    /// [`LineLease::paint`] gets this check for free because it paints through
    /// the arbiter. A writer with its own paint path has to ask, and asks
    /// *here* rather than through a free function so the query and the
    /// obligation travel together: only a writer that actually registered can
    /// pose the question.
    pub fn suspended(&self) -> bool {
        suspended()
    }
}

impl Drop for EphemeralRegistration {
    fn drop(&mut self) {
        lock().registered.retain(|(id, _)| *id != self.id);
    }
}

// ---------------------------------------------------------------------------
// The line lease
// ---------------------------------------------------------------------------

/// Exclusive ownership of the terminal's ephemeral bottom line.
///
/// Erasure is unforgettable: [`Drop`] erases whatever this lease painted. That
/// is what closes the two residue paths that leaked before the arbiter — a `?`
/// propagating a mid-stream transport error past a hand-placed `finish()`, and
/// a spinner future dropped rather than completed.
pub struct LineLease {
    id: u64,
    sink: Sink,
    /// Has this lease put bytes on the current row? The single flag that makes
    /// erase idempotent and keeps us from clearing a row we do not own.
    painted: AtomicBool,
}

impl LineLease {
    /// Which stream this lease paints on.
    pub fn sink(&self) -> Sink {
        self.sink
    }

    /// THE erase. One implementation for the whole workspace.
    ///
    /// `\r` + `ESC[K` — the same two escapes the hand-rolled sites emitted, but
    /// expressed through crossterm and issued from exactly one place. Byte-wise
    /// identical to the literal it replaces (`crossterm`'s
    /// `Clear(UntilNewLine)` *is* `ESC[K`), so no capture changes.
    ///
    /// Deliberately NOT `MoveToColumn(0)`, which would emit `ESC[1G` — visually
    /// the same, but a third undeclared byte-level delta in golden captures.
    pub fn erase(&self) {
        if !self.painted.swap(false, Ordering::SeqCst) {
            return;
        }
        self.sink.with(|w| {
            let _ = execute!(w, Print("\r"), Clear(ClearType::UntilNewLine));
            let _ = w.flush();
        });
    }

    /// Paint the ephemeral row. `f` writes the row's content (no newline); this
    /// erases first, so a redraw never leaves a stale tail.
    ///
    /// A no-op while a [`PromptWindow`] is alive — that is the guarantee that a
    /// 100 ms ticker cannot overwrite a question the operator is reading.
    pub fn paint(&self, f: impl FnOnce(&mut LineWriter<'_>) -> io::Result<()>) {
        if suspended() {
            return;
        }
        self.sink.with(|w| {
            let _ = queue!(w, Print("\r"), Clear(ClearType::UntilNewLine));
            let _ = f(w);
            let _ = w.flush();
        });
        self.painted.store(true, Ordering::SeqCst);
    }

    /// Emit a PERMANENT line (it scrolls into scrollback) without losing the
    /// ephemeral row: erase, write, and leave the row unpainted so the next
    /// tick redraws below it. This is the cooperation the dim reasoning
    /// trickle needed and open-coded before.
    pub fn emit_line(&self, f: impl FnOnce(&mut LineWriter<'_>) -> io::Result<()>) {
        self.erase();
        self.sink.with(|w| {
            let _ = f(w);
            let _ = w.flush();
        });
    }
}

impl Drop for LineLease {
    fn drop(&mut self) {
        self.erase();
        let (m, cv) = arbiter();
        let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.line_held = false;
        state.registered.retain(|(id, _)| *id != self.id);
        cv.notify_all();
    }
}

// ---------------------------------------------------------------------------
// The facade
// ---------------------------------------------------------------------------

/// How many times a [`PromptWindow`] has been handed out this process.
///
/// §6.10: the DEFAULT-DENY invariant says a session that cannot answer a TTY
/// prompt must reach a denial *without ever asking* — `should_prompt_permissions`
/// short-circuits on `headless || !interactive` before anything touches the
/// terminal. This counter is how a test proves the negative: not "the prompt
/// looked right", but "no prompt was ever constructed".
static SUSPENSIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The number of prompt windows constructed so far — see [`SUSPENSIONS`].
///
/// Unconditionally public, NOT behind `test-util`. It is a read-only counter
/// that can neither forge a window nor widen anything, and gating it would have
/// forced every crate needing the default-deny witness to enable `test-util` —
/// which, through cargo's feature unification, would have exposed
/// `PromptWindow::test_stub` to the whole build and hollowed out the seal that
/// `tests/prompt_window_is_sealed.rs` exists to protect.
pub fn prompt_windows_constructed() -> u64 {
    SUSPENSIONS.load(Ordering::SeqCst)
}

/// The arbiter's facade — a ZST over the private singleton.
pub struct Terminal;

/// How long `lease` waits for an incumbent writer to release the line before
/// giving up. Bounded on purpose: a spinner is a nicety, and blocking a turn
/// indefinitely to get one would trade a cosmetic problem for a hang.
const LEASE_WAIT: Duration = Duration::from_millis(50);

impl Terminal {
    /// Acquire the ephemeral line. `None` when this process may not own one —
    /// callers then simply have no spinner, with **zero bytes emitted**.
    pub fn lease(sink: Sink) -> Option<LineLease> {
        Self::lease_with_caps(super::caps::detect(), sink)
    }

    /// [`Terminal::lease`] with the capability supplied rather than detected.
    ///
    /// The migration seam: it lets a caller that already computed its own
    /// (weaker, legacy) gate keep deciding, so a step can move a spinner onto
    /// the arbiter without also changing when it appears. New code should call
    /// [`Terminal::lease`] and let `LineCaps::detect()` decide.
    pub fn lease_with_caps(caps: LineCaps, sink: Sink) -> Option<LineLease> {
        // Protocol mode is an absolute veto no override may pierce: fd 1 is a
        // JSON-RPC wire and a single spinner frame corrupts it.
        if super::caps::protocol_mode() || !caps.can_own() {
            return None;
        }
        let (m, cv) = arbiter();
        let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.line_held {
            let (guard, timeout) = cv
                .wait_timeout_while(state, LEASE_WAIT, |s| s.line_held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if timeout.timed_out() {
                return None;
            }
            state = guard;
        }
        state.line_held = true;
        state.next_id += 1;
        let id = state.next_id;
        Some(LineLease {
            id,
            sink,
            painted: AtomicBool::new(false),
        })
    }

    /// [`LineLease::emit_line`] for a writer that does **not** hold the lease.
    ///
    /// # Why this exists
    ///
    /// A permanent line is often produced far from whoever owns the ephemeral
    /// row — a retry notice raised inside an HTTP client while the spinner
    /// covering that call is owned three crates away. Before this, such a
    /// writer had exactly two options and both were wrong: emit nothing, or
    /// re-implement the lease's erase-then-write from outside the arbiter.
    /// `newt-tui`'s `summarizer_progress` chose the second, and it was a live
    /// race in two directions:
    ///
    /// 1. It wrote a raw `\r ESC[K` without clearing anyone's `painted` flag,
    ///    so the next 100 ms tick fired `Clear(UntilNewLine)` on the row the
    ///    notice had just moved to.
    /// 2. It could not see [`suspended`], so it happily erased and printed over
    ///    a permission question the operator was reading.
    ///
    /// Both close here, and neither closes by remembering to check something:
    /// the erase is delegated to each registered [`Ephemeral`], whose own erase
    /// is flag-guarded and idempotent. That is what makes the suspended case
    /// correct *by construction* — under a live [`PromptWindow`] every
    /// ephemeral was already erased when the window was handed out, so their
    /// flags are clear, **no erase escape is written at all**, and the line
    /// lands below the question instead of on top of it.
    ///
    /// Nothing here gates on capability: this is a *permanent* line, and a
    /// caller that must not speak into a pipe gates before it calls (see
    /// `tty::widgets::Notice::emit`).
    pub fn emit_line(sink: Sink, f: impl FnOnce(&mut LineWriter<'_>) -> io::Result<()>) {
        // Collect under the lock, erase outside it: erasing writes to the
        // terminal and must never happen while holding the arbiter's mutex.
        let live: Vec<Arc<dyn Ephemeral>> = live_ephemerals(&mut lock());
        for e in &live {
            e.erase();
        }
        sink.with(|w| {
            let _ = f(w);
            let _ = w.flush();
        });
    }

    /// Register an ephemeral so [`Terminal::suspend_for_prompt`] can erase it.
    /// Held weakly — dropping the writer deregisters it.
    pub(crate) fn register(id: u64, e: &Arc<dyn Ephemeral>) {
        lock().registered.push((id, Arc::downgrade(e)));
    }

    /// Register an ephemeral writer that owns rows of its own, so
    /// [`Terminal::suspend_for_prompt`] erases it before a question renders and
    /// restores it after (#1410).
    ///
    /// **Takes no lease.** This neither acquires nor blocks on
    /// [`Inner::line_held`] — a bottom-row spinner and a multi-row viewport
    /// coexist, and a suspension erases both. That distinction is the whole
    /// point of this entry point: a [`LineLease`] is exclusive ownership of the
    /// ONE ephemeral bottom row and carries *that row's* erase (`\r` +
    /// `ESC[K`), which is the wrong erase for a writer that owns N rows above
    /// the cursor. A lease also never touches `registered` at all, so a
    /// leaseholder is never erased at a suspension — leasing would deliver none
    /// of what this method exists for.
    ///
    /// Held **weakly**: the returned handle stores only an id, so neither the
    /// arbiter nor a leaked handle can pin the writer alive.
    ///
    /// A registered writer with its own paint path MUST consult
    /// [`EphemeralRegistration::suspended`] before painting. Registration alone
    /// is not enough — it guarantees the frame is erased *before* the question,
    /// not that nothing repaints *over* it a moment later.
    pub fn register_ephemeral(e: &Arc<dyn Ephemeral>) -> EphemeralRegistration {
        let mut state = lock();
        state.next_id += 1;
        let id = state.next_id;
        state.registered.push((id, Arc::downgrade(e)));
        EphemeralRegistration { id }
    }

    /// **THE seam.** Erase and quiesce every registered ephemeral, take stdin,
    /// and hand back the only object that can talk to a human. Restores on drop.
    ///
    /// Everything after this point is guaranteed a clean bottom row, and the
    /// shared ticker paints nothing until the returned window is dropped.
    pub fn suspend_for_prompt() -> PromptWindow {
        SUSPENSIONS.fetch_add(1, Ordering::SeqCst);
        // 1. Take stdin FIRST and block until the turn watcher's read finishes,
        //    so we never erase the screen and then wait to be allowed to ask.
        let stdin = StdinToken::acquire();

        // 2. Flip the suspend flag, then erase. Order matters: with the flag set
        //    first, a ticker that wakes mid-erase paints nothing back.
        let live: Vec<Arc<dyn Ephemeral>> = {
            let mut state = lock();
            state.suspended = true;
            live_ephemerals(&mut state)
        };
        for e in &live {
            e.erase();
        }

        // 3. Only NOW is the process truly blocked on a human: stdin ownership
        //    has succeeded and the screen is prompt-ready. Observing earlier
        //    would report intent (possibly still waiting on another stdin
        //    owner) rather than reality.
        notify_prompt_observer(true);

        PromptWindow {
            _seal: Seal,
            stdin: Some(stdin),
            resume: live,
            live: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Stdin ownership
// ---------------------------------------------------------------------------

/// Exclusive stdin, plus the canonical-mode restore.
///
/// The surrounding turn may have put the terminal in cbreak mode to watch for
/// Esc; line-oriented input is restored here so `read_line` actually waits for
/// an answer, and the previous mode is restored on drop.
struct StdinToken {
    #[cfg(unix)]
    restore: Option<libc::termios>,
}

impl StdinToken {
    fn acquire() -> Self {
        let thread = std::thread::current().id();
        let (m, cv) = arbiter();
        let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.watcher_reading
            || state
                .prompt_owner
                .as_ref()
                .is_some_and(|owner| *owner != thread)
        {
            state = cv
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.prompt_owner = Some(thread);
        state.prompt_depth += 1;
        drop(state);
        Self {
            #[cfg(unix)]
            restore: enter_prompt_line_mode().ok(),
        }
    }
}

impl Drop for StdinToken {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(prev) = self.restore.take() {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &prev);
            }
        }
        let (m, cv) = arbiter();
        let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.prompt_depth = state.prompt_depth.saturating_sub(1);
        if state.prompt_depth == 0 {
            state.prompt_owner = None;
            cv.notify_all();
        }
    }
}

/// Put stdin into canonical (line) mode, returning the previous settings.
#[cfg(unix)]
fn enter_prompt_line_mode() -> io::Result<libc::termios> {
    unsafe {
        let fd = libc::STDIN_FILENO;
        let mut prev: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut prev) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut line = prev;
        line.c_lflag |= libc::ICANON | libc::ECHO;
        line.c_cc[libc::VMIN] = 1;
        line.c_cc[libc::VTIME] = 0;
        if libc::tcsetattr(fd, libc::TCSANOW, &line) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(prev)
    }
}

/// The turn watcher's exclusive stdin read token. A prompt cannot enter while
/// this exists, and this cannot be acquired while a prompt owns stdin.
pub struct WatcherStdinGuard;

/// Try to take the watcher's read token. `None` means a prompt owns stdin (or
/// another watcher read is in flight) and the caller must not read.
pub fn try_watch_stdin() -> Option<WatcherStdinGuard> {
    let mut state = lock();
    if state.prompt_owner.is_some() || state.watcher_reading {
        return None;
    }
    state.watcher_reading = true;
    Some(WatcherStdinGuard)
}

impl Drop for WatcherStdinGuard {
    fn drop(&mut self) {
        let (m, cv) = arbiter();
        let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.watcher_reading = false;
        cv.notify_all();
    }
}

/// Whether a prompt currently owns stdin — for ownership tests and for the
/// watcher's own diagnostics.
pub fn prompt_stdin_active() -> bool {
    lock().prompt_owner.is_some()
}

// ---------------------------------------------------------------------------
// PromptWindow — the unforgeable capability
// ---------------------------------------------------------------------------

/// A private, sealed ZST. `PromptWindow` holds one, which is what makes the
/// struct unconstructible outside this module *even with struct-literal
/// syntax* — privacy on the type, not merely on the constructor.
struct Seal;

/// The capability to talk to a human.
///
/// There is no public constructor. The only ways to obtain one are
/// [`Terminal::suspend_for_prompt`] — which erases every ephemeral writer
/// before it returns — and [`PromptWindow::test_stub`] under `cfg(test)`.
/// Because every blocking prompt takes `&PromptWindow`, a question printed onto
/// a live spinner is not a bug you can write.
pub struct PromptWindow {
    _seal: Seal,
    stdin: Option<StdinToken>,
    resume: Vec<Arc<dyn Ephemeral>>,
    /// `false` for the test stub: it arbitrates nothing and must not clear the
    /// process-wide suspend flag on drop.
    live: bool,
}

impl PromptWindow {
    /// The ONLY sanctioned way to write a question.
    ///
    /// Guarantees a clean row. The bottom line has just been erased and the
    /// cursor parked at column 0, so the question starts where the operator is
    /// looking rather than appended to spinner chrome.
    pub fn ask(&self, text: &str) -> io::Result<()> {
        let mut out = io::stdout();
        write!(out, "{text}")?;
        out.flush()
    }

    /// The ONLY sanctioned blocking read. Stdin is already exclusively owned and
    /// already back in canonical line mode, so this actually waits for a human.
    pub fn read_line(&self) -> io::Result<String> {
        let mut buf = String::new();
        self.read_line_into(&mut buf)?;
        Ok(buf)
    }

    /// [`PromptWindow::read_line`] with `io::BufRead::read_line`'s exact shape.
    ///
    /// Callers that must distinguish EOF (`Ok(0)` - the operator pressed
    /// Ctrl-D, a deliberate empty answer) from a genuine read error (no human at
    /// all) need the byte count, not just the string.
    pub fn read_line_into(&self, buf: &mut String) -> io::Result<usize> {
        io::stdin().read_line(buf)
    }

    /// A notice printed while suspended (a deny explanation, a narrator line).
    /// Routed through the window so it lands on the clean rows below the
    /// question rather than racing the ticker.
    pub fn notice(&self, text: &str) -> io::Result<()> {
        let mut out = io::stdout();
        writeln!(out, "{text}")?;
        out.flush()
    }

    /// The only other constructor: an inert window for tests, which arbitrates
    /// nothing and touches no terminal. Exists so the prompt functions stay
    /// unit-testable now that they require the capability.
    #[cfg(any(test, feature = "test-util"))]
    pub fn test_stub() -> Self {
        Self {
            _seal: Seal,
            stdin: None,
            resume: Vec::new(),
            live: false,
        }
    }
}

impl Drop for PromptWindow {
    fn drop(&mut self) {
        if !self.live {
            return;
        }
        // Clear the suspend flag BEFORE restoring, so a restore that repaints
        // is not silently swallowed.
        lock().suspended = false;
        for e in &self.resume {
            e.restore();
        }
        // Stdin last: the terminal mode goes back to whatever the turn watcher
        // had set up only after the screen is whole again.
        self.stdin = None;
        notify_prompt_observer(false);
    }
}

/// Observer notified when a live [`PromptWindow`] opens (`true`) and closes
/// (`false`) — i.e. exactly when the process starts and stops blocking on a
/// human. Registered at most once, by the UI tier; this crate knows nothing
/// about what the observer does with the signal. The test stub never fires it.
static PROMPT_OBSERVER: OnceLock<Box<dyn Fn(bool) + Send + Sync>> = OnceLock::new();

/// Register the process-wide prompt observer. Later registrations are ignored.
pub fn set_prompt_observer(observer: impl Fn(bool) + Send + Sync + 'static) {
    let _ = PROMPT_OBSERVER.set(Box::new(observer));
}

fn notify_prompt_observer(open: bool) {
    if let Some(observer) = PROMPT_OBSERVER.get() {
        observer(open);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Counter {
        erased: AtomicUsize,
        restored: AtomicUsize,
    }

    impl Ephemeral for Counter {
        fn erase(&self) {
            self.erased.fetch_add(1, Ordering::SeqCst);
        }
        fn restore(&self) {
            self.restored.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counter() -> Arc<Counter> {
        Arc::new(Counter {
            erased: AtomicUsize::new(0),
            restored: AtomicUsize::new(0),
        })
    }

    /// §6.4(a): at most ONE ephemeral lease exists at a time. The second
    /// acquirer is refused rather than becoming a second writer on the same row.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_line_admits_exactly_one_writer() {
        let first = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout)
            .expect("an Own capability yields the line");
        assert!(
            Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout).is_none(),
            "a second writer must NOT get the line while the first holds it"
        );
        drop(first);
        let third = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout);
        assert!(third.is_some(), "the line is reusable once released");
    }

    /// The gate is honored: `LineCaps::None` yields no lease, so a caller emits
    /// zero bytes rather than painting into a pipe.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn no_capability_means_no_lease_and_no_bytes() {
        assert!(Terminal::lease_with_caps(LineCaps::None, Sink::Stdout).is_none());
    }

    /// §6.5's mechanism, at the unit tier: suspending for a prompt erases every
    /// registered ephemeral BEFORE the window exists, and restores on drop.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn suspending_erases_every_ephemeral_then_restores() {
        let c = counter();
        let dynamic: Arc<dyn Ephemeral> = c.clone();
        Terminal::register(9_001, &dynamic);

        assert_eq!(c.erased.load(Ordering::SeqCst), 0);
        {
            let w = Terminal::suspend_for_prompt();
            assert_eq!(
                c.erased.load(Ordering::SeqCst),
                1,
                "the ephemeral must be erased before the window is handed out"
            );
            assert!(suspended(), "the ticker must see the suspend flag");
            // Painting is inert while a question is on screen — this is the
            // property that stops a 100ms ticker overwriting the prompt.
            assert_eq!(c.restored.load(Ordering::SeqCst), 0);
            drop(w);
        }
        assert!(!suspended(), "the flag clears when the window drops");
        assert_eq!(c.restored.load(Ordering::SeqCst), 1);
    }

    /// A lease held while a window is alive paints nothing, so the question
    /// stays the most recent thing on the terminal.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_live_prompt_window_makes_painting_a_no_op() {
        let lease = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout).expect("lease");
        let w = Terminal::suspend_for_prompt();
        lease.paint(|_w| Ok(()));
        assert!(
            !lease.painted.load(Ordering::SeqCst),
            "a paint during a prompt must be dropped, not deferred onto the question"
        );
        drop(w);
        lease.paint(|_w| Ok(()));
        assert!(
            lease.painted.load(Ordering::SeqCst),
            "painting resumes once the window is gone"
        );
    }

    /// Erase is idempotent and flag-guarded, so a `Drop` after an explicit
    /// teardown cannot clear a row someone else has since taken.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn erase_is_idempotent() {
        let lease = Terminal::lease_with_caps(LineCaps::Own, Sink::Stdout).expect("lease");
        lease.paint(|_w| Ok(()));
        assert!(lease.painted.load(Ordering::SeqCst));
        lease.erase();
        assert!(!lease.painted.load(Ordering::SeqCst));
        lease.erase(); // no-op, no panic, no stray escape
        assert!(!lease.painted.load(Ordering::SeqCst));
    }

    /// Nested prompts keep ownership until the LAST one releases — dropping an
    /// inner guard must not hand stdin back to the turn watcher mid-question.
    /// (Moved here from `newt-tui/src/permissions.rs` with the mechanism.)
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn nested_prompts_hold_stdin_until_the_outermost_releases() {
        assert!(
            !prompt_stdin_active(),
            "test starts with no active prompt stdin owner"
        );
        {
            let _outer = StdinToken::acquire();
            assert!(
                try_watch_stdin().is_none(),
                "the watcher cannot read while a prompt owns stdin"
            );
            assert!(prompt_stdin_active());
            {
                let _nested = StdinToken::acquire();
                assert!(prompt_stdin_active(), "nested prompts keep ownership");
            }
            assert!(
                prompt_stdin_active(),
                "dropping one nested guard must not release stdin early"
            );
        }
        assert!(
            !prompt_stdin_active(),
            "prompt stdin ownership must clear when the last guard drops"
        );
    }

    /// The watcher's protected read BLOCKS a prompt from entering, closing the
    /// check-then-read race at permission transitions. (Also moved here.)
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn watcher_read_token_blocks_prompt_entry_until_the_read_finishes() {
        let watcher = try_watch_stdin().expect("watcher acquires idle stdin");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let prompt = std::thread::spawn(move || {
            let _prompt = StdinToken::acquire();
            let _ = entered_tx.send(());
        });

        assert!(
            entered_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "prompt entered during the watcher's protected read"
        );
        drop(watcher);
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("prompt enters once the watcher releases stdin");
        prompt.join().unwrap();
    }

    /// The stdin half still interlocks: the watcher's read token blocks a
    /// prompt, and a prompt blocks the watcher. (The behavior the TUI's
    /// `PromptStdinGuard` / `try_watch_stdin` pair had, now under one arbiter.)
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn watcher_and_prompt_exclude_each_other_on_stdin() {
        assert!(!prompt_stdin_active());
        let watcher = try_watch_stdin().expect("the watcher acquires idle stdin");
        assert!(
            try_watch_stdin().is_none(),
            "the watcher's read token is exclusive"
        );
        drop(watcher);

        let token = StdinToken::acquire();
        assert!(prompt_stdin_active());
        assert!(
            try_watch_stdin().is_none(),
            "the watcher must not read while a prompt owns stdin"
        );
        drop(token);
        assert!(!prompt_stdin_active());
        assert!(try_watch_stdin().is_some(), "released on drop");
    }

    /// The test stub is inert: it arbitrates nothing, so it cannot leave the
    /// process suspended for every later test.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_test_stub_arbitrates_nothing() {
        let w = PromptWindow::test_stub();
        assert!(!suspended());
        drop(w);
        assert!(!suspended());
    }

    /// The prompt observer describes reality, not intent: `open` fires only
    /// once stdin ownership and suspension have succeeded (observable here as
    /// stdin already being prompt-owned when the callback runs), `close` fires
    /// on drop, and the inert test stub fires neither.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn prompt_observer_fires_after_stdin_acquisition_and_on_drop() {
        // (thread, open, stdin_owned_at_callback); process-global log because
        // the observer is a process-global OnceLock. Serial-guarded tests
        // cannot interleave windows, so filtering to our thread is exact.
        static LOG: Mutex<Vec<(std::thread::ThreadId, bool, bool)>> = Mutex::new(Vec::new());
        set_prompt_observer(|open| {
            LOG.lock()
                .unwrap()
                .push((std::thread::current().id(), open, prompt_stdin_active()));
        });
        let me = std::thread::current().id();
        LOG.lock().unwrap().retain(|(t, _, _)| *t != me);

        {
            let _stub = PromptWindow::test_stub();
        }
        assert!(
            LOG.lock().unwrap().iter().all(|(t, _, _)| *t != me),
            "the stub must not fire the observer"
        );

        let w = Terminal::suspend_for_prompt();
        drop(w);
        let mine: Vec<(bool, bool)> = LOG
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _, _)| *t == me)
            .map(|(_, open, owned)| (*open, *owned))
            .collect();
        assert_eq!(
            mine,
            vec![(true, true), (false, false)],
            "open fires exactly once, with stdin already owned (post-acquire, \
             not intent); close fires on drop, after stdin is released"
        );
    }
}
