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

/// **A surface that may take the WHOLE terminal**, and what it does when
/// another surface already holds rows.
///
/// # Why this is an argument rather than a predicate
///
/// `regions` decides who owns rows; [`Terminal::suspend_for_prompt`] hands out
/// the terminal. Until #2027 the second never asked the first, which is #2019:
/// `/settings` took its own prompt window while the cockpit had an editor
/// mounted below, and the arbiter — holding both halves in this one file — had
/// no opinion. A ratchet on the NUMBER of acquisitions would have passed that
/// PR unchanged, because the call site already existed; what was wrong was the
/// context it ran in.
///
/// So the declaration is **required**, and it is written AT the acquisition,
/// beside the state that justifies it — the same placement rule the Esc
/// ladder's claim accessors follow (`assets/esc_ladder.toml`: *"add the row
/// here AND an accessor beside the state it reads"*), and for the same reason:
/// a "who is asking?" predicate kept in this file goes stale the moment
/// someone adds a consumer.
///
/// Two directions are guarded, and neither is a count:
///
/// * an acquisition that declares nothing **does not compile** — the argument
///   is not optional (`tests/ui/suspend_for_prompt_requires_a_taker.rs`);
/// * a variant with no production acquisition fails
///   `tests/terminal_taker_registry.rs`, so a taker cannot sit here dead.
///
/// # What CANNOT be typed away
///
/// Only the declaration is static. *Whether rows are held* is dynamic and
/// cross-stack by construction — this module's opening paragraph is the
/// argument: "a spinner and a permission prompt sit in different call stacks,
/// so no purely-static scheme can relate them". The collision is therefore a
/// runtime refusal at every one of the call sites, and saying otherwise would
/// be claiming a guarantee we do not have.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TerminalTaker {
    /// The cockpit presenter's `SurfaceRequest::Interact` arm. It **is** the
    /// region holder (`Screen::region`) and has already reserved the modal's
    /// rows and receded the chat chevron, so taking them is the whole point.
    CockpitModal,
    /// The permission gate's authorization prompt (`PromptPermissionGate::ask`).
    /// Runs on the session thread under a live cockpit, whose presenter holds
    /// the block's rows and quiesces itself on this suspension — see
    /// `cockpit/presenter.rs`'s "Threads and stdin": the window takes stdin,
    /// the watcher token is refused, the key loop backs off. A deliberate
    /// row-taker, and one of only two.
    PermissionAuthorization,
    /// `PromptPermissionGate::ask_question`'s fallback, for a gate that has no
    /// surface seam (#1862 C1). A gate that HAS one never reaches here.
    PermissionQuestion,
    /// `RichSurface::present_interaction` — the classic rich surface's own
    /// terminal adapter. Its editor lease is taken per `read_turn` and is not
    /// held while an interaction is presented, so nothing should be holding
    /// rows underneath it.
    RichSurfaceModal,
    /// `LeanSurface::present_interaction`, the plain-scroller twin.
    LeanSurfaceModal,
    /// `ask_on_this_terminal` — a slash form's ask for a caller that owns the
    /// terminal outright (the plain CLI, `newt crew edit`). **This is #2019's
    /// site.** A session passes its surface seam instead; if this is somehow
    /// reached under a mounted surface, refusing is the correct answer.
    SlashForm,
    /// The setup wizard's operator prompt, which runs before any surface is
    /// mounted.
    SetupWizard,
    /// The Codex-compat `OPENAI_*` adoption question, asked at most once per
    /// process on a TTY.
    CodexEnvAdoption,
    /// A `newt <subcommand>` confirmation on the plain CLI — dock, doctor,
    /// ocap, dgx, mcp-probe. No session is mounted on these paths; the process
    /// owns the terminal outright.
    PlainCliConfirm,
}

impl TerminalTaker {
    /// Every declared taker. Walked by `tests/terminal_taker_registry.rs`,
    /// which fails when a row here has no production acquisition.
    pub const ALL: &'static [Self] = &[
        Self::CockpitModal,
        Self::PermissionAuthorization,
        Self::PermissionQuestion,
        Self::RichSurfaceModal,
        Self::LeanSurfaceModal,
        Self::SlashForm,
        Self::SetupWizard,
        Self::CodexEnvAdoption,
        Self::PlainCliConfirm,
    ];

    /// The variant's name as it is written at the acquisition — the token the
    /// registry test matches. Exhaustive on purpose: a new variant does not
    /// compile until it is named here, which is where the author meets
    /// [`TerminalTaker::ALL`] and [`TerminalTaker::on_held_rows`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CockpitModal => "CockpitModal",
            Self::PermissionAuthorization => "PermissionAuthorization",
            Self::PermissionQuestion => "PermissionQuestion",
            Self::RichSurfaceModal => "RichSurfaceModal",
            Self::LeanSurfaceModal => "LeanSurfaceModal",
            Self::SlashForm => "SlashForm",
            Self::SetupWizard => "SetupWizard",
            Self::CodexEnvAdoption => "CodexEnvAdoption",
            Self::PlainCliConfirm => "PlainCliConfirm",
        }
    }

    /// What this surface declared about rows another surface holds.
    ///
    /// [`OnCollision`] rather than a fresh vocabulary: "take the rows, the
    /// holder is quiesced" and "refuse, and let the caller degrade" are
    /// already named there, and a second enum for the same two intents is the
    /// sprawl `CLAUDE.md` records five spinners' worth of.
    /// [`OnCollision::Shift`] is unrepresentable here — a prompt window is the
    /// whole terminal and has nowhere to move to — and
    /// `no_taker_declares_a_shift` pins that.
    #[must_use]
    pub const fn on_held_rows(self) -> OnCollision {
        match self {
            // The two deliberate row-takers. Both are quiesced by this very
            // suspension; see each variant's doc.
            Self::CockpitModal | Self::PermissionAuthorization => OnCollision::SuspendHolder,
            _ => OnCollision::Refuse,
        }
    }
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
    /// **And it consults `regions`** (#2027). `taker` is the caller's declared
    /// claim on the terminal; when it declared [`OnCollision::Refuse`] and
    /// another surface holds rows, this hands back a REFUSED window rather
    /// than taking stdin and erasing the screen out from under it. That is
    /// #1952's degrade-don't-die: the caller gets an inert window whose `ask`
    /// and `read_line` error with what happened, and it degrades.
    ///
    /// Everything after this point is guaranteed a clean bottom row, and the
    /// shared ticker paints nothing until the returned window is dropped.
    pub fn suspend_for_prompt(taker: TerminalTaker) -> PromptWindow {
        Self::suspend_for_prompt_with_output(PromptOutput::Stdout, taker)
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
    pub fn suspend_for_prompt_to(output: File, taker: TerminalTaker) -> PromptWindow {
        Self::suspend_for_prompt_with_output(PromptOutput::File(output), taker)
    }

    fn suspend_for_prompt_with_output(output: PromptOutput, taker: TerminalTaker) -> PromptWindow {
        // Counted before the veto ON PURPOSE: a protocol-mode caller reaching
        // this seam is exactly what an operator would want to see in
        // `prompt_windows_constructed`, and silently not counting the attempt
        // would hide the misbehaving caller this veto exists to contain. A
        // row-refused attempt counts for the same reason.
        SUSPENSIONS.fetch_add(1, Ordering::SeqCst);
        if !prompts_permitted(super::caps::protocol_mode()) {
            return PromptWindow::vetoed(output);
        }
        // 0. #2027: is anybody holding rows, and did this caller say it would
        //    take them? Ahead of the stdin token deliberately — refusing after
        //    seizing stdin would block the holder on a window that then
        //    refuses to speak, which is the misrender traded for a hang.
        if taker.on_held_rows() == OnCollision::Refuse {
            if let Some(held) = lock().regions.first().map(|(_, region)| *region) {
                return PromptWindow::refused_rows(output, taker, held);
            }
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
            refusal: None,
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
    /// `Some` when this window may not speak at all — protocol mode (#1866) or
    /// a refused acquisition (#2027). Distinct from `live`: the test stub
    /// arbitrates nothing but is still allowed to speak.
    refusal: Option<Refusal>,
}

/// Why an inert window will not speak. Both arms mean the same thing to the
/// caller — *there is nobody at the other end of this window* — for two
/// different reasons, and each says which in its error.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Refusal {
    /// Protocol mode (#1866): fd 1 may be a JSON-RPC wire.
    Protocol,
    /// #2027: another surface holds terminal rows and this taker declared
    /// [`OnCollision::Refuse`].
    RowsHeld(TerminalTaker, Region),
}

impl Refusal {
    /// The refusal a caller meets. `NotConnected` for both, because that is
    /// what is true either way: no operator is reachable through this window.
    fn error(self, action: &str) -> io::Error {
        let because = match self {
            Self::Protocol => "fd 1 is a machine protocol channel and there is \
                 no operator to answer"
                .to_string(),
            Self::RowsHeld(taker, held) => format!(
                "another surface holds {held:?} and `{}` declared it would not \
                 take rows it does not own — ask through that surface's own \
                 seam instead",
                taker.name()
            ),
        };
        io::Error::new(
            io::ErrorKind::NotConnected,
            format!("refusing to {action}: {because}"),
        )
    }
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
        Self::inert(output, Refusal::Protocol)
    }

    /// #2027: a window refused because another surface holds rows and the
    /// taker declared it would not take them. Structurally identical to the
    /// protocol veto — ONE inert shape, two reasons — so a refusal cannot
    /// accidentally acquire half the capability the veto denies.
    fn refused_rows(output: PromptOutput, taker: TerminalTaker, held: Region) -> Self {
        Self::inert(output, Refusal::RowsHeld(taker, held))
    }

    fn inert(output: PromptOutput, refusal: Refusal) -> Self {
        Self {
            _seal: Seal,
            stdin: None,
            resume: Vec::new(),
            output,
            live: false,
            refusal: Some(refusal),
        }
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
        if let Some(refusal) = self.refusal {
            return Err(refusal.error("ask"));
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
        if let Some(refusal) = self.refusal {
            return Err(refusal.error("read an answer"));
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
        if self.refusal.is_some() {
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
            refusal: None,
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
#[path = "arbiter_tests.rs"]
mod tests;
