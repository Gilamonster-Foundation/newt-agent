//! TTY renderer for the turn-scoped active-tool spill viewport.

use crate::completed_spill::CompletedSpillArchive;
use crate::spill_view::{SpillStream, SpillView};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::queue;
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use newt_core::{LiveToolOutput, ToolOutputStream};
// #1640: CompletedSpillRenderer trait for Rich TUI completed spill rendering
use newt_core::agentic::CompletedSpillRenderer;
use std::io::Write;
#[cfg(any(unix, test))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

const HISTORY_LINES: usize = 4_096;
const LINE_CHARS: usize = 4_096;

struct RenderState {
    geometry: Box<dyn Fn() -> Option<(usize, usize)> + Send + Sync>,
    view: Option<SpillView>,
    columns: usize,
    collapsed_rows: usize,
    max_rows: usize,
    desired_rows: usize,
    drawable: bool,
    color: bool,
    generation: Option<u64>,
}

struct OutputState {
    writer: TerminalWriter,
    /// The exact lines last painted, kept so a later erase can re-wrap them at
    /// whatever width the terminal is NOW and rewind the right number of rows.
    /// Widths alone were not enough — see [`physical_rows`].
    painted_lines: Vec<String>,
    painted_generation: Option<u64>,
    /// Arbiter registration (#1410). Present for the real stdout viewport;
    /// `None` for the `#[cfg(test)]` in-memory renderers, which register
    /// explicitly (`register_for_test`) when a test wants the suspend gate.
    ///
    /// It lives HERE, behind the same mutex `paint_generation` already takes
    /// first, so the gate costs one check on a lock the paint path holds
    /// anyway — no new parameter threaded through `write`, and the handle's
    /// lifetime is exactly the renderer's.
    registration: Option<newt_core::tty::EphemeralRegistration>,
}

enum TerminalWriter {
    Stdout,
    #[cfg(test)]
    Other(Box<dyn Write + Send>),
}

impl TerminalWriter {
    fn write_batch(
        &mut self,
        bytes: &[u8],
        still_valid: impl FnOnce() -> bool,
    ) -> std::io::Result<bool> {
        match self {
            Self::Stdout => {
                let stdout = std::io::stdout();
                let mut stdout = stdout.lock();
                if !still_valid() {
                    return Ok(false);
                }
                stdout.write_all(bytes)?;
                stdout.flush()?;
            }
            #[cfg(test)]
            Self::Other(writer) => {
                if !still_valid() {
                    return Ok(false);
                }
                writer.write_all(bytes)?;
                writer.flush()?;
            }
        }
        Ok(true)
    }
}

/// Single stdout owner for one active tool's redraw region.
///
/// Width-shrink cleanup follows the primary-screen reflow used by mainstream
/// terminal emulators: a painted logical line is rewrapped at the new column
/// count. ANSI exposes no portable reflow capability query, so this is an
/// assumption, not a probe. Normal painting and same-width cleanup use exact
/// row counts.
///
/// **Reflow is a REQUIREMENT of the rich tier, not a caveat** (#1426, decided
/// 2026-07-27 — see `docs/decisions/lean_rich_tui_morphologies.md`). An emulator
/// that keeps old rows un-reflowed is a lean-tier terminal and should run
/// `--lean`, which has no redraw region and therefore no rewind to get wrong.
/// Assuming no-reflow instead would leave stale rows on *every* shrink in the
/// common case in order to be safe in the rare one.
///
/// No height clamp is needed here: `MoveUp` already saturates at row 0, so
/// bounding the count changes the emitted bytes without changing where the
/// cursor lands.
pub(crate) struct LiveSpillRenderer {
    state: Arc<Mutex<RenderState>>,
    output: Arc<Mutex<OutputState>>,
    abandoned_through: Arc<AtomicU64>,
    completed_archive: Option<Arc<CompletedSpillArchive>>,
    #[cfg(any(unix, test))]
    repaint_requested: Arc<AtomicU64>,
    #[cfg(any(unix, test))]
    repaint_running: Arc<AtomicBool>,
}

impl LiveSpillRenderer {
    /// The real stdout viewport, registered with the line arbiter (#1410).
    ///
    /// Returns an `Arc` because registration needs `Arc<dyn Ephemeral>`. The
    /// arbiter holds only a `Weak` and the handle stores only a `u64`, so this
    /// is not a reference cycle: the last `Arc` dropping runs `OutputState`'s
    /// drop, which deregisters.
    pub(crate) fn stdout(
        rows: usize,
        color: bool,
        completed_archive: Arc<CompletedSpillArchive>,
    ) -> Option<Arc<Self>> {
        let me = Arc::new(Self::with_output_and_geometry(
            TerminalWriter::Stdout,
            rows,
            color,
            Some(completed_archive),
            || {
                crossterm::terminal::size()
                    .ok()
                    .map(|(columns, rows)| (usize::from(columns), usize::from(rows)))
            },
        )?);
        me.register_with_arbiter();
        Some(me)
    }

    /// Bind this viewport to the line arbiter so `suspend_for_prompt` erases it
    /// before a question and restores it after.
    fn register_with_arbiter(self: &Arc<Self>) {
        let ephemeral: Arc<dyn newt_core::tty::Ephemeral> = self.clone();
        let registration = newt_core::tty::Terminal::register_ephemeral(&ephemeral);
        self.output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .registration = Some(registration);
    }

    /// Test-only registration: the in-memory renderers are constructed bare so
    /// the ~15 existing paint tests keep running with the gate inert. A test
    /// that wants to prove the gate opts in.
    #[cfg(test)]
    fn register_for_test(self: &Arc<Self>) {
        self.register_with_arbiter();
    }

    #[cfg(test)]
    fn with_writer(
        writer: impl Write + Send + 'static,
        columns: usize,
        rows: usize,
        color: bool,
    ) -> Self {
        Self::with_writer_and_geometry(writer, rows, color, move || {
            Some((columns, rows.saturating_add(3)))
        })
        .expect("fixed test geometry is drawable")
    }

    #[cfg(test)]
    fn with_writer_and_geometry(
        writer: impl Write + Send + 'static,
        desired_rows: usize,
        color: bool,
        geometry: impl Fn() -> Option<(usize, usize)> + Send + Sync + 'static,
    ) -> Option<Self> {
        Self::with_output_and_geometry(
            TerminalWriter::Other(Box::new(writer)),
            desired_rows,
            color,
            None,
            geometry,
        )
    }

    fn with_output_and_geometry(
        writer: TerminalWriter,
        desired_rows: usize,
        color: bool,
        completed_archive: Option<Arc<CompletedSpillArchive>>,
        geometry: impl Fn() -> Option<(usize, usize)> + Send + Sync + 'static,
    ) -> Option<Self> {
        let (columns, terminal_rows) = geometry()?;
        let (collapsed_rows, max_rows) = viewport_geometry(desired_rows, columns, terminal_rows)?;
        Some(Self {
            state: Arc::new(Mutex::new(RenderState {
                geometry: Box::new(geometry),
                view: None,
                columns,
                collapsed_rows,
                max_rows,
                desired_rows,
                drawable: true,
                color,
                generation: None,
            })),
            output: Arc::new(Mutex::new(OutputState {
                writer,
                painted_lines: Vec::new(),
                painted_generation: None,
                registration: None,
            })),
            abandoned_through: Arc::new(AtomicU64::new(0)),
            completed_archive,
            #[cfg(any(unix, test))]
            repaint_requested: Arc::new(AtomicU64::new(0)),
            #[cfg(any(unix, test))]
            repaint_running: Arc::new(AtomicBool::new(false)),
        })
    }

    #[cfg(any(unix, test))]
    pub(crate) fn scroll_up(&self) -> bool {
        self.scroll(SpillView::scroll_up)
    }

    #[cfg(any(unix, test))]
    pub(crate) fn scroll_down(&self) -> bool {
        self.scroll(SpillView::scroll_down)
    }

    #[cfg(any(unix, test))]
    pub(crate) fn toggle_expanded(&self) -> bool {
        self.scroll(SpillView::toggle_expanded)
    }

    /// #1704 (Ctrl-t): expand to half the console height.
    #[cfg(unix)]
    #[allow(dead_code)]
    pub(crate) fn expand_half(&self) -> bool {
        self.scroll(SpillView::expand_half)
    }

    /// #1704: is the user scrolled back off the tail (explore mode)?
    #[cfg(unix)]
    #[allow(dead_code)]
    pub(crate) fn is_exploring(&self) -> bool {
        let state = self.lock_state();
        state
            .view
            .as_ref()
            .is_some_and(|view| !view.is_following_tail())
    }

    /// #1704 (Esc while exploring): leave explore mode — snap back to the tail.
    #[cfg(unix)]
    #[allow(dead_code)]
    pub(crate) fn exit_explore(&self) -> bool {
        self.scroll(SpillView::scroll_to_bottom)
    }

    // #1303 step 5: editor-mode nav (vi `gg`/`G`/`C-d`/`C-u`, emacs paging),
    // each riding the same single-owner `scroll` write discipline. Reached ONLY
    // through the `#[cfg(unix)]` keyboard-watcher `SpillInput` impl (no test
    // calls them directly, unlike scroll_up/scroll_down/toggle_expanded) — so
    // gate them `unix`, not `any(unix, test)`, or the Windows `test` build
    // compiles them with their sole (unix-only) caller absent → dead_code.
    #[cfg(unix)]
    pub(crate) fn scroll_to_top(&self) -> bool {
        self.scroll(SpillView::scroll_to_top)
    }

    #[cfg(unix)]
    pub(crate) fn scroll_to_bottom(&self) -> bool {
        self.scroll(SpillView::scroll_to_bottom)
    }

    #[cfg(unix)]
    pub(crate) fn half_page_up(&self) -> bool {
        self.scroll(SpillView::half_page_up)
    }

    #[cfg(unix)]
    pub(crate) fn half_page_down(&self) -> bool {
        self.scroll(SpillView::half_page_down)
    }

    // Only reached through the unix-only keyboard watcher (`SpillInput::refresh_geometry`
    // in lib.rs); no test calls this directly, unlike scroll_up/scroll_down/toggle_expanded.
    #[cfg(unix)]
    pub(crate) fn refresh_geometry(&self) -> bool {
        let Some(mut state) = self.try_lock_state() else {
            return false;
        };
        if state.view.is_none() {
            return false;
        }
        let before = (
            state.columns,
            state.collapsed_rows,
            state.max_rows,
            state.drawable,
        );
        let _ = sync_geometry(&mut state);
        let changed = before
            != (
                state.columns,
                state.collapsed_rows,
                state.max_rows,
                state.drawable,
            );
        drop(state);
        if changed {
            self.repaint_async();
        }
        true
    }

    #[cfg(any(unix, test))]
    fn scroll(&self, action: fn(&mut SpillView)) -> bool {
        // A terminal write may hold `output` indefinitely, but every renderer
        // path releases `state` before writing. Waiting for this short model
        // mutation therefore makes a keypress reliable without coupling input
        // responsiveness to terminal I/O.
        let mut state = self.lock_state();
        let Some(view) = state.view.as_mut() else {
            return false;
        };
        action(view);
        drop(state);
        self.repaint_async();
        true
    }

    #[cfg(any(unix, test))]
    fn repaint_async(&self) {
        self.repaint_requested.fetch_add(1, Ordering::Release);
        if self
            .repaint_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let state = self.state.clone();
        let output = self.output.clone();
        let abandoned_through = self.abandoned_through.clone();
        let repaint_requested = self.repaint_requested.clone();
        let repaint_running = self.repaint_running.clone();
        if std::thread::Builder::new()
            .name("newt-live-spill-input".to_string())
            .spawn(move || {
                run_input_repaint(
                    &state,
                    &output,
                    &abandoned_through,
                    &repaint_requested,
                    &repaint_running,
                );
            })
            .is_err()
        {
            self.repaint_running.store(false, Ordering::Release);
        }
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> bool {
        self.lock_state().view.is_some()
    }

    #[cfg(test)]
    fn snapshot_lines(&self) -> Vec<String> {
        let state = self.lock_state();
        state
            .view
            .as_ref()
            .map(|view| fixed_frame_lines(view, state.generation == Some(COMPLETED_GENERATION)))
            .unwrap_or_default()
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, RenderState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn try_lock_state(&self) -> Option<std::sync::MutexGuard<'_, RenderState>> {
        match self.state.try_lock() {
            Ok(state) => Some(state),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    fn is_abandoned(&self, generation: u64) -> bool {
        generation <= self.abandoned_through.load(Ordering::Acquire)
    }
}

/// #1410 — the viewport is the workspace's other cursor owner, so the arbiter
/// has to be able to get it off the screen before a question renders.
///
/// `arbiter.rs`'s own trait doc named this as an unfinished step, and named the
/// hazard: this renderer's `Clear(FromCursorDown)` rewind "can destroy rows it
/// does not own".
impl newt_core::tty::Ephemeral for LiveSpillRenderer {
    /// Erase whatever generation is currently painted.
    ///
    /// Idempotent by construction: `erase_output` clears both
    /// `painted_lines` and `painted_generation`, and the guard below then
    /// makes every subsequent call write zero bytes — the same shape as
    /// `LineLease::erase`.
    ///
    /// The lock is **blocking**, deliberately. A `try_lock` that gave up would
    /// return having written nothing while `painted_generation` is still set,
    /// and the *next* `erase_output` would then rewind from a cursor now below
    /// the question and the operator's typed answer, deleting both. A wedged
    /// stdout blocks everything anyway; a skipped erase corrupts.
    fn erase(&self) {
        // Re-sync geometry BEFORE reading `columns`. `erase_output` re-wraps
        // `painted_lines` at it to recover the physical row count, so a
        // stale width makes `MoveUp` land *inside* the frame and strands the
        // rows above it permanently (nothing else clears them — the erase
        // discards its own bookkeeping unconditionally). `finish` takes exactly
        // this precaution, and
        // `finish_rechecks_geometry_even_without_another_output_chunk` is the
        // test pinning it.
        //
        // Scoped so `state` is released before `output` is taken: every other
        // path here locks state-then-output and drops state in between.
        let columns = {
            let mut state = self.lock_state();
            let _ = sync_geometry(&mut state);
            state.columns
        };
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(generation) = output.painted_generation {
            erase_output(&mut output, columns, &self.abandoned_through, generation);
        }
    }

    /// Repaint the frame the prompt displaced.
    ///
    /// **Synchronous**, not `repaint_async`. `PromptWindow::drop` documents
    /// that the terminal mode goes back "only after the screen is whole
    /// again"; an async restore returns before the frame exists, and the
    /// spawned repaint would then race the caller's canonical output — landing
    /// the frame *after* a denial message, recording rows it does not own, and
    /// leaving the next erase to rewind through that message.
    ///
    /// Unwind-guarded because a panic here escapes through
    /// `suspend_for_prompt`, which would leave the arbiter's `suspended` flag
    /// set with no `PromptWindow` ever constructed — silencing every spinner in
    /// the process for good. A viewport that fails to repaint is a cosmetic
    /// loss; that is not.
    fn restore(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let generation = self.lock_state().generation;
            if let Some(generation) = generation {
                paint_generation(
                    &self.state,
                    &self.output,
                    &self.abandoned_through,
                    generation,
                );
            }
        }));
    }
}

impl LiveToolOutput for LiveSpillRenderer {
    fn start(&self, generation: u64) {
        if self.is_abandoned(generation) {
            return;
        }
        let mut state = self.lock_state();
        let _ = sync_geometry(&mut state);
        let mut view = SpillView::with_limits(
            state.columns,
            state.collapsed_rows,
            HISTORY_LINES,
            LINE_CHARS,
        );
        view.resize(state.columns, state.collapsed_rows, state.max_rows);
        state.view = Some(view);
        state.generation = Some(generation);
    }

    fn write(&self, generation: u64, stream: ToolOutputStream, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        if self.is_abandoned(generation) {
            return;
        }
        let mut state = self.lock_state();
        if state.generation != Some(generation) {
            return;
        }
        let Some(view) = state.view.as_mut() else {
            return;
        };
        let stream = match stream {
            ToolOutputStream::Stdout => SpillStream::Stdout,
            ToolOutputStream::Stderr => SpillStream::Stderr,
        };
        view.push_stream_bytes(stream, chunk);
        let _ = sync_geometry(&mut state);
        drop(state);
        paint_generation(
            &self.state,
            &self.output,
            &self.abandoned_through,
            generation,
        );
    }

    fn finish(&self, generation: u64) {
        if self.is_abandoned(generation) {
            return;
        }
        let mut state = self.lock_state();
        if state.generation != Some(generation) {
            return;
        }
        if let Some(view) = state.view.as_mut() {
            view.finish();
        }
        let _ = sync_geometry(&mut state);
        let columns = state.columns;
        // #1303 step 6 (DEFERRED — clean seam): the post-completion overlay
        // attaches HERE. Instead of dropping the finished `SpillView`, a
        // retain-overlay would move it (or its `lines` + `dropped_lines`) into a
        // generation-keyed slot on `RenderState`, beside `view`, reusing
        // `frame()`/`fixed_frame_lines` for a bounded reopenable viewer anchored
        // at the cursor (decision clause 3, grounding §4). The committed block is
        // still re-rendered from the authoritative envelope (`display.rs`), never
        // the live buffer; an abandoned generation is NOT retainable. Kept out of
        // v1 to preserve the single-owner hand-off below unchanged.
        state.view = None;
        state.generation = None;
        drop(state);
        erase_generation(&self.output, &self.abandoned_through, generation, columns);
    }

    fn abandon(&self, generation: u64) {
        // Leave the already-painted frame where it is: canonical output may
        // begin as soon as this fast invalidation returns. The next generation
        // discards this bookkeeping instead of rewinding from that new cursor.
        self.abandoned_through
            .fetch_max(generation, Ordering::AcqRel);
        if let Some(mut state) = self.try_lock_state() {
            if state.generation == Some(generation) {
                state.view = None;
                state.generation = None;
            }
        }
    }
}

fn fixed_frame_lines(view: &SpillView, completed: bool) -> Vec<String> {
    let frame = if completed {
        view.completed_frame()
    } else {
        view.frame()
    };
    let rows = view.visible_rows();
    let mut lines = Vec::with_capacity(rows + 2);
    lines.push(frame.top.line);
    lines.extend(frame.content.into_iter().map(|row| row.line));
    while lines.len() < rows + 1 {
        lines.push(if completed { "⎴" } else { "▒" }.to_string());
    }
    lines.push(frame.bottom.line);
    lines
}

fn viewport_geometry(
    desired_rows: usize,
    columns: usize,
    terminal_rows: usize,
) -> Option<(usize, usize)> {
    // Two boundary rows plus the cursor row below the frame must fit. Very
    // small terminals stay on the canonical completion-only path.
    (desired_rows > 0 && columns >= 2 && terminal_rows >= 4).then(|| {
        let max_rows = terminal_rows - 3;
        (desired_rows.min(max_rows), max_rows)
    })
}

fn sync_geometry(state: &mut RenderState) -> bool {
    let Some((columns, terminal_rows)) = (state.geometry)() else {
        state.drawable = false;
        return false;
    };
    let Some((collapsed_rows, max_rows)) =
        viewport_geometry(state.desired_rows, columns, terminal_rows)
    else {
        state.columns = columns;
        state.drawable = false;
        return false;
    };

    if !state.drawable
        || state.columns != columns
        || state.collapsed_rows != collapsed_rows
        || state.max_rows != max_rows
    {
        state.columns = columns;
        state.collapsed_rows = collapsed_rows;
        state.max_rows = max_rows;
        if let Some(view) = state.view.as_mut() {
            view.resize(columns, collapsed_rows, max_rows);
        }
    }
    state.drawable = true;
    true
}

fn paint_generation(
    state: &Mutex<RenderState>,
    output: &Mutex<OutputState>,
    abandoned_through: &AtomicU64,
    generation: u64,
) {
    if is_abandoned(abandoned_through, generation) {
        return;
    }
    let mut output = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // #1410 — THE PAINT GATE. Registration alone is not enough: it guarantees
    // the frame is erased *before* a question renders, not that nothing paints
    // *over* it a moment later. Two painters would:
    //
    //   * the `newt-live-output-{gen}` worker, on the next tool chunk; and
    //   * `run_input_repaint`, because `watch_for_interrupt_fd` calls
    //     `refresh_geometry()` every 10 ms while a prompt owns stdin
    //     (lib.rs) — so a terminal RESIZE during a permission question
    //     repaints on top of it, with no tool call-ordering involved.
    //
    // Worse, `suspend_for_prompt`'s erase clears `painted_generation`, so a
    // paint that slipped through would skip the erase-previous branch below and
    // land at the cursor — i.e. directly under the question — and the following
    // `restore()` would rewind `MoveUp + Clear(FromCursorDown)` straight
    // through it. That is the 8x/second overwrite bug with a 4-row frame.
    //
    // Checked under the `output` lock that the whole paint holds, so a paint
    // that beat the flag is still undone by the erase that follows it.
    if output
        .registration
        .as_ref()
        .is_some_and(newt_core::tty::EphemeralRegistration::suspended)
    {
        return;
    }
    if is_abandoned(abandoned_through, generation) {
        discard_generation(&mut output, generation);
        return;
    }
    let snapshot = {
        let state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.generation == Some(generation)).then(|| {
            (
                state.drawable.then(|| {
                    state
                        .view
                        .as_ref()
                        .map(|view| fixed_frame_lines(view, generation == COMPLETED_GENERATION))
                        .unwrap_or_default()
                }),
                state.color,
                state.columns,
            )
        })
    };
    let Some((lines, color, columns)) = snapshot else {
        return;
    };
    if is_abandoned(abandoned_through, generation) {
        discard_generation(&mut output, generation);
        return;
    }

    if let Some(previous) = output.painted_generation {
        if is_abandoned(abandoned_through, previous) {
            discard_generation(&mut output, previous);
        } else {
            erase_output(&mut output, columns, abandoned_through, previous);
        }
    }
    let Some(lines) = lines else {
        return;
    };

    // Each explicit line may occupy more physical rows after the terminal
    // reflows it at a narrower width. `painted_lines` preserves enough
    // information for the next erase to rewind that resized footprint.
    let mut batch = Vec::new();
    if color {
        let _ = queue!(&mut batch, SetForegroundColor(Color::DarkGrey));
    }
    for line in &lines {
        let _ = queue!(
            &mut batch,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Print(line),
            Print("\r\n")
        );
    }
    if color {
        let _ = queue!(&mut batch, ResetColor);
    }
    let wrote = output
        .writer
        .write_batch(&batch, || !is_abandoned(abandoned_through, generation))
        .unwrap_or(false);
    if !wrote || is_abandoned(abandoned_through, generation) {
        discard_generation(&mut output, generation);
        return;
    }
    output.painted_lines.clone_from(&lines);
    output.painted_generation = Some(generation);
}

#[cfg(any(unix, test))]
fn run_input_repaint(
    state: &Mutex<RenderState>,
    output: &Mutex<OutputState>,
    abandoned_through: &AtomicU64,
    repaint_requested: &AtomicU64,
    repaint_running: &AtomicBool,
) {
    loop {
        let observed = repaint_requested.load(Ordering::Acquire);
        let generation = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation;
        if let Some(generation) = generation {
            paint_generation(state, output, abandoned_through, generation);
        }
        if repaint_requested.load(Ordering::Acquire) != observed {
            continue;
        }

        repaint_running.store(false, Ordering::Release);
        if repaint_requested.load(Ordering::Acquire) == observed
            || repaint_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            break;
        }
    }
}

#[allow(dead_code)]
fn erase_generation(
    output: &Mutex<OutputState>,
    abandoned_through: &AtomicU64,
    generation: u64,
    columns: usize,
) {
    let mut output = output
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if is_abandoned(abandoned_through, generation) {
        discard_generation(&mut output, generation);
        return;
    }
    if output.painted_generation == Some(generation) {
        erase_output(&mut output, columns, abandoned_through, generation);
    }
}

#[allow(dead_code)]
fn erase_output(
    output: &mut OutputState,
    columns: usize,
    abandoned_through: &AtomicU64,
    generation: u64,
) {
    if output.painted_lines.is_empty() {
        output.painted_generation = None;
        return;
    }
    let physical_rows = output
        .painted_lines
        .iter()
        .map(|line| physical_rows(line, columns))
        .sum::<usize>();
    let mut batch = Vec::new();
    let _ = queue!(
        &mut batch,
        MoveUp(u16::try_from(physical_rows).unwrap_or(u16::MAX)),
        MoveToColumn(0),
        Clear(ClearType::FromCursorDown)
    );
    let _ = output
        .writer
        .write_batch(&batch, || !is_abandoned(abandoned_through, generation));
    output.painted_lines.clear();
    output.painted_generation = None;
}

fn discard_generation(output: &mut OutputState, generation: u64) {
    if output.painted_generation == Some(generation) {
        output.painted_lines.clear();
        output.painted_generation = None;
    }
}

fn is_abandoned(abandoned_through: &AtomicU64, generation: u64) -> bool {
    generation <= abandoned_through.load(Ordering::Acquire)
}

/// Terminal cells one character occupies. Combining marks attach to the
/// previous cell (0); ASCII and the frame's own glyph vocabulary are narrow;
/// everything else is assumed double-width.
fn char_cells(ch: char) -> usize {
    if matches!(
        ch,
        '\u{0300}'..='\u{036f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{fe20}'..='\u{fe2f}'
            | '\u{e0100}'..='\u{e01ef}'
    ) {
        0
    } else if ch.is_ascii() || matches!(ch, '…' | '▲' | '▼' | '▒' | '▓' | '⧉' | '▣' | '\u{fffd}')
    {
        1
    } else {
        2
    }
}

#[allow(dead_code)]
fn rendered_width(text: &str) -> usize {
    text.chars().map(char_cells).sum()
}

/// Physical rows one painted line occupies at `columns`, **by the terminal's
/// own wrapping rule**.
///
/// This replaced `width.div_ceil(columns)`, which is wrong for any line
/// carrying a double-width glyph: such a glyph cannot straddle the right
/// margin, so the terminal breaks the line EARLY and leaves a cell unused. The
/// arithmetic then under-counts, `MoveUp` lands inside the old frame, and the
/// rows above it are stranded forever — nothing else clears them, because the
/// erase discards its own bookkeeping unconditionally.
///
/// Found by the boundary legend: `⧉ Space to expand · ↑↓ scroll` is 32 cells
/// and reflows to FIVE rows at width 8, not the four `div_ceil` predicts,
/// because `↑` and `↓` each refuse to split. Two such lines in a frame stranded
/// exactly two rows. The previous legend was two cells shorter and happened to
/// wrap where the arithmetic agreed, so the defect sat behind a passing test.
fn physical_rows(text: &str, columns: usize) -> usize {
    let columns = columns.max(1);
    let mut rows = 1usize;
    let mut used = 0usize;
    for cells in text.chars().map(char_cells) {
        // A zero-width mark never forces a break; it rides the cell before it.
        // Nor does anything break an ALREADY-empty row: a glyph wider than the
        // whole terminal has nowhere further to go, and breaking before it
        // would count a row that never gets written.
        if cells > 0 && used > 0 && used + cells > columns {
            rows += 1;
            used = 0;
        }
        used += cells;
    }
    rows
}

// ========================================================================
// #1640: CompletedSpillRenderer implementation for Rich TUI completed spill
// ========================================================================

/// The generation completed viewports paint under. Live generations count up
/// from 1 and `abandon` only ever raises `abandoned_through` to a live number,
/// so `u64::MAX` can never satisfy `generation <= abandoned_through` — the
/// abandonment gate stays open for completed frames without any bypass. (The
/// prior sentinel, 0, sat BELOW the floor and was abandoned by definition:
/// every completed paint and scroll repaint silently no-opped.)
const COMPLETED_GENERATION: u64 = u64::MAX;

impl CompletedSpillRenderer for LiveSpillRenderer {
    fn retain_completed(&self, output: &str) -> Option<u64> {
        self.completed_archive
            .as_ref()
            .map(|archive| archive.retain(output))
    }

    /// Render a completed tool result as an interactive spill viewport.
    ///
    /// Reuses the live SpillView frame logic — scrolling, expanding, and
    /// editor-mode navigation all ride the existing `SpillInput` routing,
    /// because the completed view IS `state.view`. Bounded to max 50% of the
    /// terminal height so a single spill can't flood a tmux.
    fn render_completed(&self, output: &str, width: usize, max_height: usize) -> usize {
        {
            let mut state = self.lock_state();
            // Never stomp a LIVE viewport: the live hand-off (`finish`) clears
            // `generation` before completed rendering may take the screen. A
            // previous COMPLETED frame is ours to replace.
            if state
                .generation
                .is_some_and(|generation| generation != COMPLETED_GENERATION)
            {
                return 0;
            }
            if !sync_geometry(&mut state) {
                return 0;
            }
            let mut view =
                SpillView::with_limits(state.columns, max_height.max(1), HISTORY_LINES, LINE_CHARS);
            view.push_stream_bytes(SpillStream::Stdout, output.as_bytes());
            view.finish();
            // Bounded by the caller's budget AND 50% of the terminal height.
            let (_, terminal_rows) = (state.geometry)().unwrap_or((width, 24));
            let max_allowed = (terminal_rows / 2).max(3).min(state.max_rows.max(1));
            let rows_to_show = view
                .retained_line_count()
                .clamp(1, max_allowed.min(max_height.max(1)));
            view.resize(state.columns, rows_to_show, max_allowed);
            state.view = Some(view);
            state.generation = Some(COMPLETED_GENERATION);
        }
        paint_generation(
            &self.state,
            &self.output,
            &self.abandoned_through,
            COMPLETED_GENERATION,
        );

        // Physical rows painted, for the caller's cursor accounting.
        let columns = self.lock_state().columns.max(1);
        let output_state = self
            .output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if output_state.painted_generation != Some(COMPLETED_GENERATION) {
            return 0;
        }
        output_state
            .painted_lines
            .iter()
            .map(|line| physical_rows(line, columns))
            .sum()
    }

    /// Whether a COMPLETED viewport is on screen. A live viewport does not
    /// count — its lifecycle belongs to `LiveToolOutput`, not to dismissal.
    fn is_active(&self) -> bool {
        self.lock_state().generation == Some(COMPLETED_GENERATION)
    }

    /// Drop the completed viewport's bookkeeping without terminal writes —
    /// the completed twin of live `abandon`. The painted frame (if any) stays
    /// as inert residue; what this guarantees is that no LATER erase can
    /// replay a stale rewind from a cursor that has since moved.
    fn discard(&self) {
        {
            let mut state = self.lock_state();
            if state.generation != Some(COMPLETED_GENERATION) {
                return;
            }
            state.view = None;
            state.generation = None;
        }
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        discard_generation(&mut output, COMPLETED_GENERATION);
    }

    /// Erase the completed viewport — a pure rewind, releasing the model
    /// state so the next `start`/`render_completed` begins clean. The
    /// committed excerpt above the frame is the durable record. No-op when
    /// no completed viewport is up (never touches a live generation).
    fn erase(&self) {
        let columns = {
            let mut state = self.lock_state();
            if state.generation != Some(COMPLETED_GENERATION) {
                return;
            }
            // Re-sync so the rewind divides by the terminal's CURRENT width —
            // the same stale-width hazard `Ephemeral::erase` documents.
            let _ = sync_geometry(&mut state);
            state.view = None;
            state.generation = None;
            state.columns
        };
        erase_generation(
            &self.output,
            &self.abandoned_through,
            COMPLETED_GENERATION,
            columns,
        );
    }
}

#[cfg(test)]
#[path = "live_spill_tests.rs"]
mod tests;
