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
//! public constructor and contains a private sealed ZST, so no crate can build
//! one with struct-literal syntax either. Every function that may block
//! on a human takes `&PromptWindow`. You cannot obtain the argument without
//! having suspended, so *a prompt printed onto a live spinner does not compile*.
//! The failure mode is not "remembered"; it is unrepresentable.
//!
//! This generalizes two disciplines the codebase already trusts: the
//! `LiveOutputSession` RAII `Drop → finish()` pattern, and the `StdinOwnership`
//! singleton (a `OnceLock<(Mutex<_>, Condvar)>`) that the permission prompt used
//! for the *other* half of the terminal. Stdin ownership moves in here so one
//! object arbitrates both directions.

use std::fs::File;
use std::io::{self, IsTerminal, Write};
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
    /// Rows held by multi-row writers. `line_held` guards the ONE ephemeral
    /// bottom row; this guards regions, which nothing arbitrated before
    /// #1979 — `register_ephemeral` deliberately takes no lease, so two
    /// viewports could hold the same rows and overpaint each other (#1977).
    regions: Vec<(u64, Region)>,
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

/// The nearest free rows AT OR ABOVE a request.
///
/// Bounded, and it walks UP because every surface here is bottom-anchored: the
/// free space is above the holder, and the screen's top is the natural stop.
/// `None` means there is nowhere to go — the caller degrades rather than
/// drawing through somebody.
fn shift_clear_of(held: &[(u64, Region)], want: Region) -> Option<Region> {
    let Region::Rows { mut top, height } = want else {
        // Whole-screen cannot be shifted anywhere: it is every row by
        // definition, so it either fits alone or it does not fit.
        return held.is_empty().then_some(want);
    };
    // One step per holder is sufficient — each step clears at least one — and
    // the bound makes a malformed table terminate rather than spin.
    for _ in 0..=held.len() {
        let candidate = Region::Rows { top, height };
        match held.iter().find(|(_, h)| h.intersects(candidate)) {
            None => return Some(candidate),
            Some((_, Region::WholeScreen)) => return None,
            Some((
                _,
                Region::Rows {
                    top: holder_top, ..
                },
            )) => {
                top = holder_top.checked_sub(height)?;
            }
        }
    }
    None
}

/// Which rows a writer owns.
///
/// Absolute, resolved by the caller against the screen it already measured —
/// every surface that wants one computes its anchor anyway
/// (`inline_viewport::anchor`, `presenter`'s `self.top`). Keeping the arbiter
/// out of layout is deliberate: #1979's non-goal is a layout engine, and a
/// row range plus a policy is the whole vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// `height` rows starting at `top`, zero-based from the top of the screen.
    Rows { top: u16, height: u16 },
    /// Every row. The alternate screen is this: entering it IS taking them
    /// all, and the arbiter should know.
    WholeScreen,
}

impl Region {
    /// Do these two regions share a row?
    #[must_use]
    pub fn intersects(self, other: Self) -> bool {
        match (self, other) {
            // The alternate screen takes everything, including from itself.
            (Self::WholeScreen, _) | (_, Self::WholeScreen) => true,
            (Self::Rows { top: a, height: ah }, Self::Rows { top: b, height: bh }) => {
                // A zero-height region owns nothing and collides with nothing.
                ah != 0 && bh != 0 && a < b.saturating_add(bh) && b < a.saturating_add(ah)
            }
        }
    }
}

/// What the mint does when the requested rows are already held.
///
/// The caller's DECLARED intent, not a fallback accident. Each of these is a
/// behaviour already in the tree, now with a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnCollision {
    /// Take the rows; the holder is expected to be suspended or erased first.
    /// This is what [`Terminal::suspend_for_prompt`] already does to every
    /// registered ephemeral before a question renders.
    SuspendHolder,
    /// Move the request to the nearest free rows ABOVE the holder. #1977's
    /// panel does this: the prompt keeps its rows and the panel opens over
    /// them rather than through them.
    Shift,
    /// Refuse, and let the caller degrade — #1952's degrade-don't-die rule.
    Refuse,
}

/// Exclusive ownership of a range of terminal rows.
///
/// The N-row sibling of [`LineLease`], NOT its generalisation, and the
/// distinction is the one [`Terminal::register_ephemeral`] already records: a
/// `LineLease` carries the ONE bottom row's erase (`\r` + `ESC[K`), which is
/// the wrong erase for a writer that owns rows above the cursor. Two
/// vocabularies over one authority — this table and `line_held` live in the
/// same [`Inner`], so a single lock orders every ownership decision.
///
/// Drop returns the rows. It does NOT erase them: what a region contains is
/// the holder's business (ratatui restores its own viewport, the pager leaves
/// the alternate screen), and an arbiter that also erased would be painting
/// through a surface that already cleaned up.
pub struct RegionLease {
    id: u64,
    region: Region,
}

impl RegionLease {
    /// The rows this lease holds — which may not be the rows requested, when
    /// the policy was [`OnCollision::Shift`].
    #[must_use]
    pub fn region(&self) -> Region {
        self.region
    }

    /// Move or resize the held rows, keeping ownership CONTINUOUS.
    ///
    /// The cockpit presenter's block moves (`self.top = plan.new_top`) and is
    /// clamped on resize, so a lease it had to drop and re-take would churn
    /// and, worse, would own nothing in the window between.
    ///
    /// **`policy` mirrors the mint's, and #1980 is why it has to.** A move can
    /// be either of two things and they cannot share a rule:
    ///
    /// * a REQUEST, which may be refused — [`OnCollision::Refuse`], the
    ///   checked form: `false` means the move did not happen and the lease
    ///   still holds exactly what it held.
    /// * a REPORT of a move that already happened — [`OnCollision::SuspendHolder`].
    ///   The presenter recomputes its top from the terminal's NEW size on a
    ///   resize, and takes `new_top` from a scroll that has already scrolled.
    ///   Refusing there would not un-move the block; it would only leave the
    ///   lease describing rows the block no longer occupies, which is worse
    ///   than holding no lease at all — a wrong answer instead of no answer.
    ///
    /// [`OnCollision::Shift`] is rejected: a relocation names the rows the
    /// caller is moving TO, and silently landing somewhere else would make the
    /// lease disagree with the caller's own bookkeeping.
    pub fn relocate(&mut self, to: Region, policy: OnCollision) -> bool {
        let mut state = lock();
        let contested = state
            .regions
            .iter()
            .any(|(id, held)| *id != self.id && held.intersects(to));
        match policy {
            OnCollision::Refuse if contested => return false,
            OnCollision::Shift => return false,
            _ => {}
        }
        if let Some(entry) = state.regions.iter_mut().find(|(id, _)| *id == self.id) {
            entry.1 = to;
        }
        self.region = to;
        true
    }
}

impl Drop for RegionLease {
    fn drop(&mut self) {
        let (m, cv) = arbiter();
        let mut state = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.regions.retain(|(id, _)| *id != self.id);
        cv.notify_all();
    }
}

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

/// Whether a prompt may reach a human at all (#1866).
///
/// A pure predicate for the same reason [`super::caps::probe`] is one: protocol
/// mode is a process-global that `enter_protocol_mode` makes **irreversible on
/// purpose**, so a test that set it would poison every sibling test in the
/// binary. The policy is decided here where it can be table-tested, and read
/// once at the seam. The in-vivo half — that a real protocol-mode process
/// reaching a real prompt emits nothing — needs a child process, and lives in
/// `pty_notice_test`.
#[must_use]
pub(crate) fn prompts_permitted(protocol: bool) -> bool {
    !protocol
}

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

    /// Take exclusive ownership of a range of rows.
    ///
    /// The one place "who owns these rows" is decided for multi-row writers.
    /// Before #1979 nothing decided it: `register_ephemeral` takes no lease by
    /// design, so two inline viewports both anchored at the bottom held the
    /// same rows and overpainted each other (#1977).
    ///
    /// `policy` is the caller's DECLARED intent. A caller that has already
    /// quiesced the holder asks for [`OnCollision::SuspendHolder`]; one that
    /// can open elsewhere asks for [`OnCollision::Shift`]; one that would
    /// rather not open asks for [`OnCollision::Refuse`] and degrades.
    ///
    /// **`SuspendHolder` does not suspend anything itself.** It records that
    /// the rows are taken and trusts the caller to have quiesced the holder —
    /// which is what [`Terminal::suspend_for_prompt`] already does by erasing
    /// every registered ephemeral. Making the mint do the erasing would give
    /// the arbiter a second way to paint, and one owner of that is the point.
    pub fn lease_region(region: Region, policy: OnCollision) -> Option<RegionLease> {
        let mut state = lock();
        let granted = match policy {
            OnCollision::SuspendHolder => region,
            OnCollision::Refuse => {
                if state.regions.iter().any(|(_, h)| h.intersects(region)) {
                    return None;
                }
                region
            }
            OnCollision::Shift => shift_clear_of(&state.regions, region)?,
        };
        state.next_id += 1;
        let id = state.next_id;
        state.regions.push((id, granted));
        Some(RegionLease {
            id,
            region: granted,
        })
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
    /// **In protocol mode this hands back a VETOED window** (#1866). Epic
    /// #1803's global acceptance is that headless/protocol modes never wait,
    /// choose defaults, or emit terminal bytes, and a prompt is all three: this
    /// function alone takes stdin (which blocks), erases every ephemeral (which
    /// writes), and hands out the capability to write a question and read an
    /// answer. `Notice::emit` has consulted [`super::caps::protocol_mode`]
    /// since it was written; this did not, and held only because no
    /// protocol-mode entry point happened to reach a prompt. That is
    /// reachability, not construction — the next entry point that reached one
    /// would have broken the invariant silently, because nothing checked.
    ///
    /// The veto lands HERE rather than in [`PromptWindow::ask`] because `ask`
    /// is only one of the three violations. A check there would still leave
    /// this function seizing stdin and erasing the screen before anyone could
    /// refuse.
    ///
    /// Everything after this point is guaranteed a clean bottom row, and the
    /// shared ticker paints nothing until the returned window is dropped.
    pub fn suspend_for_prompt() -> PromptWindow {
        Self::suspend_for_prompt_with_output(PromptOutput::Stdout)
    }

    /// [`Terminal::suspend_for_prompt`] with an explicit terminal output.
    ///
    /// The process may have redirected fd 1 into an internal capture while
    /// retaining a [`File`] for the operator's real terminal. This variant
    /// keeps the same stdin arbitration, protocol veto, lifecycle events, and
    /// ephemeral suspension as the default seam, but routes
    /// [`PromptWindow::ask`] and [`PromptWindow::notice`] directly to that
    /// file. Ownership is moved into the window so the destination remains
    /// alive for the entire prompt.
    pub fn suspend_for_prompt_to(output: File) -> PromptWindow {
        Self::suspend_for_prompt_with_output(PromptOutput::File(output))
    }

    fn suspend_for_prompt_with_output(output: PromptOutput) -> PromptWindow {
        // Counted before the veto ON PURPOSE: a protocol-mode caller reaching
        // this seam is exactly what an operator would want to see in
        // `prompt_windows_constructed`, and silently not counting the attempt
        // would hide the misbehaving caller this veto exists to contain.
        SUSPENSIONS.fetch_add(1, Ordering::SeqCst);
        if !prompts_permitted(super::caps::protocol_mode()) {
            return PromptWindow::vetoed(output);
        }
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
            output,
            live: true,
            vetoed: false,
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

/// Where a prompt capability writes its human-facing bytes.
///
/// `Stdout` preserves the original process-wide behavior. `File` is the
/// direct-terminal seam for a presenter that has intentionally captured fd 1.
enum PromptOutput {
    Stdout,
    File(File),
}

impl PromptOutput {
    fn write_text(&self, text: &str, newline: bool) -> io::Result<()> {
        fn write_to(mut output: impl Write, text: &str, newline: bool) -> io::Result<()> {
            if newline {
                writeln!(output, "{text}")?;
            } else {
                write!(output, "{text}")?;
            }
            output.flush()
        }

        match self {
            Self::Stdout => write_to(io::stdout(), text, newline),
            Self::File(file) => write_to(file, text, newline),
        }
    }

    fn is_terminal(&self) -> bool {
        match self {
            Self::Stdout => io::stdout().is_terminal(),
            Self::File(file) => file.is_terminal(),
        }
    }
}

/// The capability to talk to a human.
///
/// There is no public constructor. The only ways to obtain one are
/// [`Terminal::suspend_for_prompt`] or [`Terminal::suspend_for_prompt_to`] —
/// which erase every ephemeral writer before returning — and
/// [`PromptWindow::test_stub`] under `cfg(test)`.
/// Because every blocking prompt takes `&PromptWindow`, a question printed onto
/// a live spinner is not a bug you can write.
pub struct PromptWindow {
    _seal: Seal,
    stdin: Option<StdinToken>,
    resume: Vec<Arc<dyn Ephemeral>>,
    output: PromptOutput,
    /// `false` for the test stub: it arbitrates nothing and must not clear the
    /// process-wide suspend flag on drop.
    live: bool,
    /// Protocol mode (#1866): fd 1 may be a JSON-RPC wire, so this window
    /// emits zero bytes and refuses to read. Distinct from `live` — the test
    /// stub arbitrates nothing but is still allowed to speak.
    vetoed: bool,
}

impl PromptWindow {
    /// A window that arbitrates nothing and may not speak — protocol mode.
    ///
    /// PRIVATE, and it must stay private: the seal is the capability. The
    /// trybuild proofs under `tests/ui/` pin that there is no public
    /// constructor, that the struct cannot be literaled, and that `test_stub`
    /// is not reachable from outside. This adds a third *internal* shape, not
    /// a fourth door.
    fn vetoed(output: PromptOutput) -> Self {
        Self {
            _seal: Seal,
            stdin: None,
            resume: Vec::new(),
            output,
            live: false,
            vetoed: true,
        }
    }

    /// The refusal a vetoed window returns. `NotConnected` because that is
    /// what is true: there is no operator on the other end of a JSON-RPC wire.
    fn no_operator(action: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "refusing to {action} in protocol mode — fd 1 is a machine                  protocol channel and there is no operator to answer"
            ),
        )
    }

    /// The ONLY sanctioned way to write a question.
    ///
    /// Guarantees a clean row. The bottom line has just been erased and the
    /// cursor parked at column 0, so the question starts where the operator is
    /// looking rather than appended to spinner chrome.
    pub fn ask(&self, text: &str) -> io::Result<()> {
        // LOUDLY, unlike `notice` below. A question that cannot be asked must
        // not report success: a caller that believed it had asked would go on
        // to block for an answer that is never coming.
        if self.vetoed {
            return Err(Self::no_operator("ask"));
        }
        self.output.write_text(text, false)
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
        // An ERROR, not `Ok(0)`. This method's own contract is that EOF means
        // "the operator pressed Ctrl-D, a deliberate empty answer" and an error
        // means "no human at all". Protocol mode is the second, and returning
        // EOF here would synthesise an answer nobody gave — which A3 settled
        // is not what failing closed means.
        if self.vetoed {
            return Err(Self::no_operator("read an answer"));
        }
        io::stdin().read_line(buf)
    }

    /// A notice printed while suspended (a deny explanation, a narrator line).
    /// Routed through the window so it lands on the clean rows below the
    /// question rather than racing the ticker.
    pub fn notice(&self, text: &str) -> io::Result<()> {
        // SILENTLY, unlike `ask` above, and for `Notice::emit`'s reason: a
        // notice is informational and dropping it is the documented protocol-
        // mode behaviour. Nobody is waiting on its return value.
        if self.vetoed {
            return Ok(());
        }
        self.output.write_text(text, true)
    }

    /// Whether the output owned by this prompt is an interactive terminal.
    ///
    /// Modal input uses this instead of probing process stdout: fd 1 may be a
    /// terminal-shaped internal capture while this window writes directly to
    /// the operator's saved terminal.
    pub(crate) fn output_is_terminal(&self) -> bool {
        self.output.is_terminal()
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
            output: PromptOutput::Stdout,
            live: false,
            vetoed: false,
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

/// Announce that a live [`PromptWindow`] opened (`true`) or closed (`false`)
/// as a generic lifecycle event — i.e. exactly when the process starts and
/// stops blocking on a human. This module knows nothing about who listens;
/// see [`crate::lifecycle`]. The test stub never fires it.
fn notify_prompt_observer(open: bool) {
    crate::lifecycle::emit(if open {
        crate::lifecycle::LifecycleEvent::Blocked
    } else {
        crate::lifecycle::LifecycleEvent::Unblocked
    });
}

#[cfg(test)]
mod tests {
    /// **The policy (#1866)**, in the shape `progress_sink`'s
    /// `protocol_mode_vetoes_rendering_from_every_capability` uses, and for the
    /// same reason: `enter_protocol_mode` is documented as one-way, so a test
    /// that set the real flag would veto every sibling test in this binary for
    /// the rest of the run.
    #[test]
    fn protocol_mode_vetoes_every_prompt() {
        assert!(
            !super::prompts_permitted(true),
            "fd 1 may be a JSON-RPC wire; no prompt may reach it"
        );
    }

    /// The anti-vacuous twin: without it `prompts_permitted` could be `false`
    /// always and the test above would still pass. Exactly one shape prompts,
    /// and this names it.
    #[test]
    fn and_a_process_outside_protocol_mode_may_prompt() {
        assert!(
            super::prompts_permitted(false),
            "an ordinary terminal session is the ONE shape that prompts — if \
             this fails the veto test above is vacuous"
        );
    }

    /// The veto as the CALLER meets it: a vetoed window refuses, and refuses in
    /// the two different ways the three methods deliberately chose.
    ///
    /// Reaches `PromptWindow::vetoed(..)` because this module is the seal's
    /// inside. Nothing here widens it — the constructor is private, and the
    /// trybuild proofs under `tests/ui/` still pin that no public constructor
    /// exists, the struct cannot be literaled, and `test_stub` is unreachable
    /// from outside.
    #[test]
    fn a_vetoed_window_refuses_to_ask_or_read_and_drops_notices() {
        let window = super::PromptWindow::vetoed(super::PromptOutput::Stdout);

        assert!(
            window.ask("question > ").is_err(),
            "ask must refuse rather than report a question it never wrote"
        );

        let mut buf = String::new();
        let read = window.read_line_into(&mut buf);
        assert!(
            read.is_err(),
            "read must ERROR, not return Ok(0) — EOF is a deliberate empty \
             answer from a human, and forging one is not failing closed: {read:?}"
        );
        assert!(buf.is_empty(), "nothing may land in the caller's buffer");

        assert!(
            window.notice("fyi").is_ok(),
            "a notice is informational and is dropped silently, which is why \
             it differs from ask"
        );
    }

    /// …and the twin for THAT: an unvetoed window is not refused, so the
    /// assertions above are about protocol mode rather than about every
    /// window being inert.
    #[test]
    fn and_an_unvetoed_window_is_not_refused() {
        let window = super::PromptWindow::test_stub();
        assert!(window.ask("").is_ok(), "an ordinary window may ask");
        assert!(window.notice("").is_ok(), "an ordinary window may narrate");
    }

    /// The explicit-output seam writes both prompt byte families to the file
    /// it owns. A regular file is deliberately non-terminal, which also pins
    /// the capability probe modal input uses instead of process stdout.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn an_explicit_prompt_output_routes_ask_and_notice_to_that_file() {
        let output = tempfile::NamedTempFile::new().expect("prompt output file");
        let c = counter();
        let dynamic: Arc<dyn Ephemeral> = c.clone();
        Terminal::register(9_002, &dynamic);
        let window = Terminal::suspend_for_prompt_to(
            output.reopen().expect("independent prompt output handle"),
        );

        assert_eq!(
            c.erased.load(Ordering::SeqCst),
            1,
            "the alternate output must not bypass prompt arbitration"
        );
        assert!(suspended(), "the alternate output quiesces other writers");
        assert!(
            prompt_stdin_active(),
            "the alternate output still owns prompt stdin"
        );
        assert!(
            !window.output_is_terminal(),
            "a regular-file destination must select the non-TTY modal path"
        );
        window.ask("question > ").expect("write the question");
        window.notice("narration").expect("write the notice");
        drop(window);

        assert!(!suspended(), "dropping the window resumes other writers");
        assert!(!prompt_stdin_active(), "dropping the window releases stdin");
        assert_eq!(c.restored.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_to_string(output.path()).expect("read routed prompt bytes"),
            "question > narration\n"
        );
    }

    /// Real-resource grounding for the modal branch predicate: a duplicated
    /// PTY slave is a `File` just like a presenter's saved terminal, and the
    /// window both recognizes it as interactive and writes to that device.
    #[cfg(unix)]
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn an_explicit_pty_output_is_detected_and_written_as_a_terminal() {
        use std::io::Read as _;
        use std::os::fd::FromRawFd as _;

        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: `openpty` initializes both owned descriptors on success. Each
        // is immediately transferred into exactly one `File` below.
        let opened = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "open the grounding PTY");
        // SAFETY: successful `openpty` returned fresh descriptors, and these
        // `File`s become their sole owners.
        let (mut master, output) =
            unsafe { (File::from_raw_fd(master_fd), File::from_raw_fd(slave_fd)) };
        let window = Terminal::suspend_for_prompt_to(output);

        assert!(
            window.output_is_terminal(),
            "the saved terminal file must select the interactive modal path"
        );
        window.ask("direct prompt").expect("write to the PTY slave");

        let mut painted = [0_u8; "direct prompt".len()];
        master
            .read_exact(&mut painted)
            .expect("read the explicitly routed terminal bytes");
        assert_eq!(&painted, b"direct prompt");
        drop(window);
    }

    /// Protocol veto semantics belong to the window, not to stdout. Supplying
    /// another file must not create a side door that can emit a question.
    #[test]
    fn a_vetoed_explicit_output_still_emits_zero_bytes() {
        let output = tempfile::NamedTempFile::new().expect("vetoed prompt output file");
        let window = PromptWindow::vetoed(PromptOutput::File(
            output.reopen().expect("independent prompt output handle"),
        ));

        assert!(window.ask("question").is_err());
        assert!(window.notice("narration").is_ok());
        drop(window);

        assert_eq!(
            std::fs::read_to_string(output.path()).expect("read vetoed prompt output"),
            ""
        );
    }

    /// The original entry point remains process-stdout-backed. Keeping this
    /// as a distinct assertion prevents a future refactor from silently
    /// making every prompt require an explicit file.
    #[test]
    fn the_default_prompt_window_keeps_the_process_stdout_route() {
        let window = PromptWindow::test_stub();
        assert!(matches!(&window.output, PromptOutput::Stdout));
        assert_eq!(
            window.output_is_terminal(),
            io::stdout().is_terminal(),
            "the legacy window must keep probing the stream it writes"
        );
    }

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

    /// **#1959: the seal has exactly two doors, and both gates sit BELOW the
    /// fork.**
    ///
    /// The seal's value is that its doors are enumerated and each is proven.
    /// A second public constructor is fine; a second constructor that skipped
    /// the protocol veto or the stdin token would be a hole, and the thing
    /// that keeps both honest is that they delegate to ONE private builder
    /// with the gates inside it.
    ///
    /// Stated as a source scan rather than a behaviour, because the property
    /// is structural: "no future door can be added above the gates". A
    /// behavioural test can only cover the doors that exist today.
    ///
    /// Production code only, and cut at the test module — the lesson
    /// `config_panel::enter_panel_raw_mode_is_the_only_way_in` records the
    /// hard way: this test lives IN the file it scans, so its own needles
    /// would otherwise be counted.
    #[test]
    fn the_seal_has_exactly_two_doors_and_both_gates_sit_below_the_fork() {
        let src = include_str!("arbiter.rs");
        let production = src.split("\n#[cfg(test)]").next().unwrap_or("");
        assert!(
            production.len() > 1000,
            "the production cut read nothing; every count below would be vacuous"
        );

        assert_eq!(
            production.matches("pub fn suspend_for_prompt(").count(),
            1,
            "the stdout door"
        );
        assert_eq!(
            production.matches("pub fn suspend_for_prompt_to(").count(),
            1,
            "the File door (#1959)"
        );
        assert_eq!(
            production
                .matches("Self::suspend_for_prompt_with_output(")
                .count(),
            2,
            "BOTH public doors must delegate to the one private builder — a \
             third door, or a door that built a PromptWindow itself, would \
             bypass the gates below"
        );

        // Call forms, not names: `prompts_permitted` is also DEFINED here and
        // discussed in prose, and counting mentions would move whenever
        // someone edited a comment.
        let veto = "prompts_permitted(super::caps::protocol_mode())";
        let acquire = "StdinToken::acquire()";
        assert_eq!(production.matches(veto).count(), 1, "one veto, one place");
        assert_eq!(
            production.matches(acquire).count(),
            1,
            "one acquire, one place"
        );

        let fork = production
            .find("fn suspend_for_prompt_with_output")
            .expect("the private builder");
        assert!(
            production.find(veto).is_some_and(|at| at > fork),
            "the protocol veto was hoisted ABOVE the fork — it would then \
             cover only the door it sits in, and the other would prompt on a \
             JSON-RPC wire"
        );
        assert!(
            production.find(acquire).is_some_and(|at| at > fork),
            "the stdin token was hoisted ABOVE the fork — one door would then \
             ask without exclusive stdin"
        );
    }

    /// **#1959: the File door takes the same exclusive stdin token.**
    ///
    /// The scan above proves the acquire is shared; this proves what sharing
    /// it buys, through the same `prompt_stdin_active` observable
    /// `watcher_and_prompt_exclude_each_other_on_stdin` uses for door one.
    ///
    /// `/dev/null` is a real fd, which the unit tier otherwise avoids —
    /// `PromptOutput::File` takes a `std::fs::File` and offers no seam. It is
    /// never written to here: the window is constructed and dropped, and the
    /// assertions are all about stdin.
    #[cfg(unix)]
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_file_door_takes_the_same_exclusive_stdin_token() {
        assert!(!prompt_stdin_active(), "stdin must start idle");
        let sink = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/null")
            .expect("/dev/null");

        let window = Terminal::suspend_for_prompt_to(sink);
        assert!(
            prompt_stdin_active(),
            "the File door must take the prompt's stdin token, like the stdout door"
        );
        assert!(
            try_watch_stdin().is_none(),
            "and hold it EXCLUSIVELY — the turn watcher must not read underneath it"
        );

        drop(window);
        assert!(!prompt_stdin_active(), "released on drop");
        assert!(try_watch_stdin().is_some(), "the watcher may read again");
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

    /// Blocked/Unblocked describe reality, not intent: `Blocked` is emitted
    /// only once stdin ownership and suspension have succeeded (observable
    /// here as stdin already being prompt-owned when the observer runs),
    /// `Unblocked` on drop, and the inert test stub emits neither.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn blocked_is_emitted_after_stdin_acquisition_and_unblocked_on_drop() {
        use crate::lifecycle::LifecycleEvent;

        // (event, stdin_owned_at_callback), recorded only for this thread —
        // the lifecycle registry is process-global and sibling tests emit
        // concurrently.
        let log: Arc<Mutex<Vec<(LifecycleEvent, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let me = std::thread::current().id();
        let sub = crate::lifecycle::subscribe(move |event| {
            if std::thread::current().id() == me {
                sink.lock()
                    .unwrap()
                    .push((event.event.clone(), prompt_stdin_active()));
            }
        });

        {
            let _stub = PromptWindow::test_stub();
        }
        assert!(
            log.lock().unwrap().is_empty(),
            "the stub must not emit lifecycle events"
        );

        let w = Terminal::suspend_for_prompt();
        drop(w);
        drop(sub);
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                (LifecycleEvent::Blocked, true),
                (LifecycleEvent::Unblocked, false)
            ],
            "Blocked is emitted exactly once, with stdin already owned \
             (post-acquire, not intent); Unblocked on drop, after stdin is released"
        );
    }

    // ---- #1979: region ownership --------------------------------------

    fn rows(top: u16, height: u16) -> Region {
        Region::Rows { top, height }
    }

    #[test]
    fn regions_intersect_only_when_they_share_a_row() {
        assert!(rows(10, 3).intersects(rows(12, 2)), "overlapping");
        assert!(rows(12, 2).intersects(rows(10, 3)), "and symmetrically");
        assert!(
            !rows(10, 3).intersects(rows(13, 2)),
            "adjacent is not overlapping"
        );
        assert!(!rows(13, 2).intersects(rows(10, 3)), "and symmetrically");
        // The alternate screen is every row, including against itself.
        assert!(Region::WholeScreen.intersects(rows(0, 1)));
        assert!(rows(40, 1).intersects(Region::WholeScreen));
        assert!(Region::WholeScreen.intersects(Region::WholeScreen));
        // A zero-height region owns nothing.
        assert!(!rows(10, 0).intersects(rows(10, 3)));
    }

    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_mint_refuses_rows_another_writer_holds() {
        let held = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("first take");
        assert_eq!(held.region(), rows(18, 6));
        assert!(
            Terminal::lease_region(rows(20, 4), OnCollision::Refuse).is_none(),
            "two writers were granted the same rows — this is #1977"
        );
        // TWIN: the refusal is about the OVERLAP, not about refusing always.
        let elsewhere = Terminal::lease_region(rows(2, 4), OnCollision::Refuse)
            .expect("clear rows are granted");
        assert_eq!(elsewhere.region(), rows(2, 4));
        // And dropping returns them.
        drop(held);
        drop(elsewhere);
        let reclaimed =
            Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("released rows return");
        assert_eq!(reclaimed.region(), rows(18, 6));
    }

    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn shift_opens_above_the_holder_rather_than_through_it() {
        let prompt = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("prompt");
        let panel = Terminal::lease_region(rows(18, 6), OnCollision::Shift).expect("panel shifts");
        assert_eq!(
            panel.region(),
            rows(12, 6),
            "#1977: the panel must open ABOVE the prompt's rows, not over them"
        );
        assert!(
            !panel.region().intersects(prompt.region()),
            "the shifted region still overlaps"
        );
    }

    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn shift_refuses_when_there_is_no_room_above() {
        let _floor = Terminal::lease_region(rows(0, 4), OnCollision::Refuse).expect("floor");
        assert!(
            Terminal::lease_region(rows(2, 6), OnCollision::Shift).is_none(),
            "shifting off the top of the screen must refuse, not wrap"
        );
    }

    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn the_whole_screen_collides_with_everything_and_shifts_nowhere() {
        let _rowsy = Terminal::lease_region(rows(10, 2), OnCollision::Refuse).expect("some rows");
        assert!(
            Terminal::lease_region(Region::WholeScreen, OnCollision::Refuse).is_none(),
            "the alternate screen takes every row and cannot share"
        );
        assert!(
            Terminal::lease_region(Region::WholeScreen, OnCollision::Shift).is_none(),
            "whole-screen has nowhere to shift to"
        );
    }

    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn suspend_holder_takes_the_rows_the_caller_already_quiesced() {
        let _held = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("holder");
        let taken = Terminal::lease_region(rows(18, 6), OnCollision::SuspendHolder)
            .expect("a caller that quiesced the holder takes the rows");
        assert_eq!(taken.region(), rows(18, 6));
    }

    /// The cockpit presenter's block moves and is clamped on resize, so the
    /// lease has to move WITHOUT a release-and-retake window.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_lease_relocates_in_place_and_still_respects_other_holders() {
        let _other = Terminal::lease_region(rows(0, 4), OnCollision::Refuse).expect("other");
        let mut moving = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("moving");
        assert!(
            moving.relocate(rows(10, 6), OnCollision::Refuse),
            "a clear move must succeed"
        );
        assert_eq!(moving.region(), rows(10, 6));
        // TWIN: relocation is checked, not merely recorded.
        assert!(
            !moving.relocate(rows(2, 4), OnCollision::Refuse),
            "relocating onto another holder was allowed"
        );
        assert_eq!(
            moving.region(),
            rows(10, 6),
            "a refused move must leave the lease holding what it held"
        );
        // Moving onto its OWN rows is not a self-collision.
        assert!(
            moving.relocate(rows(10, 8), OnCollision::Refuse),
            "a lease may resize in place"
        );
    }

    /// **Compose proof.** Panel over prompt, close, both restored in order —
    /// the nested-modal property (`a_nested_frame_does_not_restore_the_
    /// terminal_early`) at region scale. The inner holder returning its rows
    /// must not disturb the outer one, which is what makes nesting safe.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn an_inner_region_returns_without_disturbing_the_outer_one() {
        let prompt = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("prompt");
        {
            let panel = Terminal::lease_region(rows(18, 6), OnCollision::Shift).expect("panel");
            assert_eq!(panel.region(), rows(12, 6));
            // While both are up, the rows they hold are BOTH unavailable.
            assert!(
                Terminal::lease_region(rows(12, 6), OnCollision::Refuse).is_none(),
                "the panel's rows were re-let while it held them"
            );
            assert!(
                Terminal::lease_region(rows(18, 6), OnCollision::Refuse).is_none(),
                "the prompt's rows were re-let while it held them"
            );
        }
        // The panel closed. ITS rows are free; the prompt's are NOT — the
        // inner drop must not have released the outer holder's claim.
        let reclaimed = Terminal::lease_region(rows(12, 6), OnCollision::Refuse)
            .expect("the panel's rows return");
        assert_eq!(reclaimed.region(), rows(12, 6));
        assert!(
            Terminal::lease_region(rows(18, 6), OnCollision::Refuse).is_none(),
            "closing the panel released the PROMPT's rows — restoring more than \
             it took is the nested-modal defect"
        );
        drop(prompt);
        drop(reclaimed);
    }

    /// **#2027 (red-first): #2019's shape, at the arbiter.**
    ///
    /// `/settings` acquired its own prompt window while the cockpit had an
    /// editor mounted below it — two live chevrons, a modal with no rows
    /// reserved, and a header repainting through the question every 250 ms.
    /// Every one of those follows from the same fact: this file decides who
    /// owns ROWS and, separately, hands out the TERMINAL, and the second half
    /// never asked the first.
    ///
    /// So: a surface holds rows, and somebody who did not declare that it
    /// would take them asks for a prompt window. Nothing may have been taken.
    /// Asserted on stdin ownership and the suspend flag rather than on bytes,
    /// so the failing run emits nothing onto a sibling test's terminal.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_bare_acquisition_is_refused_while_a_surface_holds_rows() {
        let _held = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("holder");
        let window = Terminal::suspend_for_prompt();
        assert!(
            !prompt_stdin_active(),
            "a refused acquisition must not have taken stdin"
        );
        assert!(
            !suspended(),
            "a refused acquisition must not have quiesced the holder"
        );
        assert!(
            window.ask("question > ").is_err(),
            "a refused window must not report a question it never wrote"
        );
        drop(window);
    }

    /// A relocation is sometimes a REPORT, not a request (#1980).
    ///
    /// The cockpit presenter recomputes its top from the terminal's new size
    /// on a resize. Refusing that move would not un-resize the terminal — it
    /// would leave the lease naming rows the block has already left.
    #[serial_test::serial(tty_arbiter)]
    #[test]
    fn a_forced_relocation_lands_where_a_checked_one_is_refused() {
        let _other = Terminal::lease_region(rows(0, 4), OnCollision::Refuse).expect("other");
        let mut block = Terminal::lease_region(rows(18, 6), OnCollision::Refuse).expect("block");

        // Checked: refused, and the lease is unchanged.
        assert!(!block.relocate(rows(2, 4), OnCollision::Refuse));
        assert_eq!(block.region(), rows(18, 6));

        // Forced: lands, because the move already happened on the terminal.
        assert!(
            block.relocate(rows(2, 4), OnCollision::SuspendHolder),
            "a forced relocation must land, or the lease describes rows the \
             writer has already left"
        );
        assert_eq!(block.region(), rows(2, 4));

        // TWIN: `Shift` is rejected outright rather than quietly landing
        // somewhere else, which would make the lease disagree with the
        // caller's own bookkeeping.
        assert!(
            !block.relocate(rows(0, 4), OnCollision::Shift),
            "a shifting relocation would move the lease somewhere the caller \
             did not ask for"
        );
        assert_eq!(block.region(), rows(2, 4));
    }
}
