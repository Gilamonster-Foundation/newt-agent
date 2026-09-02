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
    painted_line_widths: Vec<usize>,
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
                painted_line_widths: Vec::new(),
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
    /// `painted_line_widths` and `painted_generation`, and the guard below then
    /// makes every subsequent call write zero bytes — the same shape as
    /// `LineLease::erase`.
    ///
    /// The lock is **blocking**, deliberately. A `try_lock` that gave up would
    /// return having written nothing while `painted_generation` is still set,
    /// and the *next* `erase_output` would then rewind from a cursor now below
    /// the question and the operator's typed answer, deleting both. A wedged
    /// stdout blocks everything anyway; a skipped erase corrupts.
    fn erase(&self) {
        // Re-sync geometry BEFORE reading `columns`. `erase_output` divides
        // `painted_line_widths` by it to recover the physical row count, so a
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
    // reflows it at a narrower width. `painted_line_widths` preserves enough
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
    output.painted_line_widths = lines.iter().map(|line| rendered_width(line)).collect();
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
    if output.painted_line_widths.is_empty() {
        output.painted_generation = None;
        return;
    }
    let physical_rows = output
        .painted_line_widths
        .iter()
        .map(|width| (*width).max(1).div_ceil(columns.max(1)))
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
    output.painted_line_widths.clear();
    output.painted_generation = None;
}

fn discard_generation(output: &mut OutputState, generation: u64) {
    if output.painted_generation == Some(generation) {
        output.painted_line_widths.clear();
        output.painted_generation = None;
    }
}

fn is_abandoned(abandoned_through: &AtomicU64, generation: u64) -> bool {
    generation <= abandoned_through.load(Ordering::Acquire)
}

#[allow(dead_code)]
fn rendered_width(text: &str) -> usize {
    text.chars()
        .map(|ch| {
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
            } else if ch.is_ascii()
                || matches!(ch, '…' | '▲' | '▼' | '▒' | '▓' | '⧉' | '▣' | '\u{fffd}')
            {
                1
            } else {
                2
            }
        })
        .sum()
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
            .painted_line_widths
            .iter()
            .map(|width| (*width).max(1).div_ceil(columns))
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
mod tests {
    use super::LiveSpillRenderer;
    // #1410: the gate tests drive the paint path directly, standing in for the
    // `run_input_repaint` painter that a geometry change wakes.
    use super::paint_generation;
    use crate::spill_view::display_width;
    use newt_core::{LiveToolOutput, ToolOutputStream};
    use std::io::Write;
    #[cfg(unix)]
    use std::sync::Condvar;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct CountingWriter(Arc<std::sync::atomic::AtomicUsize>);

    impl Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct ScreenModel {
        width: usize,
        rows: Vec<String>,
        cursor_row: usize,
        cursor_col: usize,
        wrap: bool,
    }

    impl ScreenModel {
        fn new(width: usize) -> Self {
            Self {
                width,
                rows: vec![String::new()],
                cursor_row: 0,
                cursor_col: 0,
                wrap: true,
            }
        }

        fn apply(&mut self, bytes: &[u8]) {
            let text = std::str::from_utf8(bytes).unwrap();
            let mut chars = text.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '\x1b' => {
                        assert_eq!(chars.next(), Some('['));
                        let mut body = String::new();
                        let final_byte = loop {
                            let next = chars.next().expect("complete CSI sequence");
                            if ('@'..='~').contains(&next) {
                                break next;
                            }
                            body.push(next);
                        };
                        self.apply_csi(&body, final_byte);
                    }
                    '\r' => self.cursor_col = 0,
                    '\n' => {
                        self.cursor_row += 1;
                        self.ensure_cursor_row();
                    }
                    ch if !ch.is_control() => self.print(ch),
                    _ => {}
                }
            }
        }

        fn resize(&mut self, width: usize) {
            let old_rows = std::mem::take(&mut self.rows);
            self.width = width.max(1);
            for row in old_rows {
                let mut chunk = String::new();
                let mut chunk_width = 0;
                for ch in row.chars() {
                    let char_width = display_width(&ch.to_string()).max(1);
                    if chunk_width > 0 && chunk_width + char_width > self.width {
                        self.rows.push(std::mem::take(&mut chunk));
                        chunk_width = 0;
                    }
                    chunk.push(ch);
                    chunk_width += char_width;
                }
                self.rows.push(chunk);
            }
            if self.rows.is_empty() {
                self.rows.push(String::new());
            }
            self.cursor_row = self.rows.len() - 1;
            self.cursor_col = display_width(&self.rows[self.cursor_row]);
        }

        fn nonempty_rows(&self) -> Vec<String> {
            self.rows
                .iter()
                .filter(|row| !row.is_empty())
                .cloned()
                .collect()
        }

        fn apply_csi(&mut self, body: &str, final_byte: char) {
            let amount = body.parse::<usize>().unwrap_or(1);
            match (body, final_byte) {
                ("?7", 'l') => self.wrap = false,
                ("?7", 'h') => self.wrap = true,
                // #1303 (§8.4): mouse-mode private sequences (set/reset) alter
                // input reporting, not the visible grid — no-op them so a frame
                // captured while mouse capture toggles doesn't panic.
                ("?1000" | "?1002" | "?1003" | "?1006" | "?1015", 'h' | 'l') => {}
                (_, 'A') => self.cursor_row = self.cursor_row.saturating_sub(amount),
                (_, 'G') => self.cursor_col = amount.saturating_sub(1),
                ("2", 'K') => {
                    self.ensure_cursor_row();
                    self.rows[self.cursor_row].clear();
                }
                // #1427: bare `ESC[K` (== `ESC[0K`) erases from the cursor to
                // end of line. This is what `Clear(UntilNewLine)` emits, and
                // therefore what `LineLease::erase` puts on the wire — so the
                // model needs it to observe the ARBITER, not just this
                // renderer. Distinct from `ESC[2K` above, which clears the whole
                // row regardless of cursor position.
                ("" | "0", 'K') => {
                    self.ensure_cursor_row();
                    let col = self.cursor_col;
                    let row = &mut self.rows[self.cursor_row];
                    // `cursor_col` is a DISPLAY column, not a byte offset —
                    // walk to the matching boundary so a wide or multibyte
                    // glyph is never split (these frames carry ▒/▓/▲ and CJK).
                    let mut width = 0usize;
                    let mut cut = row.len();
                    for (i, ch) in row.char_indices() {
                        if width >= col {
                            cut = i;
                            break;
                        }
                        width += display_width(&ch.to_string()).max(1);
                    }
                    row.truncate(cut);
                }
                (_, 'J') => {
                    self.ensure_cursor_row();
                    self.rows.truncate(self.cursor_row + 1);
                    self.rows[self.cursor_row].clear();
                }
                (_, 'm') => {}
                // #1427 asked whether this should record instead of panic.
                // It should NOT. This is a test double: a model that silently
                // ignores a sequence it does not understand keeps returning
                // green while diverging from the real terminal, which is the
                // one failure a screen model exists to prevent. Aborting loudly
                // is the feature — add an arm above when a new sequence is
                // legitimately in play, and pin its semantics with a test.
                other => panic!(
                    "unsupported screen-model CSI: {other:?} — add an arm above \
                     rather than widening the model silently"
                ),
            }
        }

        fn print(&mut self, ch: char) {
            let char_width = display_width(&ch.to_string()).max(1);
            if self.cursor_col + char_width > self.width {
                if !self.wrap {
                    return;
                }
                self.cursor_row += 1;
                self.cursor_col = 0;
                self.ensure_cursor_row();
            }
            self.ensure_cursor_row();
            self.rows[self.cursor_row].push(ch);
            self.cursor_col += char_width;
        }

        fn ensure_cursor_row(&mut self) {
            while self.rows.len() <= self.cursor_row {
                self.rows.push(String::new());
            }
        }
    }

    // Only exercised by the unix-only `blocked_terminal_write_does_not_block_*`
    // regression below (it drives `crate::watch_for_interrupt_fd`, itself unix-only).
    #[cfg(unix)]
    #[derive(Clone, Default)]
    struct BlockingWriter {
        gate: Arc<(Mutex<(bool, bool)>, Condvar)>,
    }

    #[cfg(unix)]
    impl BlockingWriter {
        fn wait_until_blocked(&self) {
            let (state, wake) = &*self.gate;
            let mut state = state.lock().unwrap();
            while !state.0 {
                state = wake.wait(state).unwrap();
            }
        }

        fn release(&self) {
            let (state, wake) = &*self.gate;
            state.lock().unwrap().1 = true;
            wake.notify_all();
        }
    }

    #[cfg(unix)]
    impl Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let (state, wake) = &*self.gate;
            let mut state = state.lock().unwrap();
            state.0 = true;
            wake.notify_all();
            while !state.1 {
                state = wake.wait(state).unwrap();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[serial_test::serial(prompt_stdin)]
    #[test]
    fn blocked_terminal_write_does_not_block_interrupt_or_watcher_shutdown() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::time::Duration;

        let writer = BlockingWriter::default();
        let geometry = Arc::new(Mutex::new((80usize, 6usize)));
        let geometry_for_renderer = geometry.clone();
        let renderer = Arc::new(
            LiveSpillRenderer::with_writer_and_geometry(writer.clone(), 3, false, move || {
                Some(*geometry_for_renderer.lock().unwrap())
            })
            .unwrap(),
        );
        renderer.start(1);

        let render_thread = {
            let renderer = renderer.clone();
            std::thread::spawn(move || {
                renderer.write(1, ToolOutputStream::Stdout, b"blocked\n");
            })
        };
        writer.wait_until_blocked();
        *geometry.lock().unwrap() = (40, 6);

        let (controls_tx, controls_rx) = mpsc::channel();
        let controls_thread = {
            let renderer = renderer.clone();
            std::thread::spawn(move || {
                assert!(renderer.scroll_up());
                assert!(renderer.toggle_expanded());
                controls_tx.send(()).unwrap();
            })
        };
        controls_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("spill controls waited for terminal I/O");
        controls_thread.join().unwrap();
        assert_eq!(
            renderer.snapshot_lines().last().map(String::as_str),
            Some("▣ Space collapses · ↑↓ scroll"),
            "the toggle must update model state while terminal output is blocked"
        );

        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let cancel = Arc::new(AtomicBool::new(false));
        let hard = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel();
        let watcher = {
            let renderer = renderer.clone();
            let cancel = cancel.clone();
            let hard = hard.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                crate::watch_for_interrupt_fd(
                    pipe[0],
                    &cancel,
                    &hard,
                    &stop,
                    Some(renderer.as_ref()),
                    newt_core::EditMode::Nano,
                    false, // mode_nav: base keys only
                    10,
                    100,
                );
                done_tx.send(()).unwrap();
            })
        };

        assert_eq!(
            unsafe { libc::write(pipe[1], [0x03].as_ptr().cast(), 1) },
            1
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !cancel.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(cancel.load(Ordering::Relaxed), "Ctrl-C was not polled");

        let (abandon_tx, abandon_rx) = mpsc::channel();
        let abandon_thread = {
            let renderer = renderer.clone();
            std::thread::spawn(move || {
                renderer.abandon(1);
                abandon_tx.send(()).unwrap();
            })
        };
        abandon_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("generation invalidation waited for terminal I/O");
        abandon_thread.join().unwrap();

        stop.store(true, Ordering::Relaxed);
        assert_eq!(unsafe { libc::write(pipe[1], b"x".as_ptr().cast(), 1) }, 1);
        done_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("watcher shutdown waited for the blocked terminal writer");

        writer.release();
        render_thread.join().unwrap();
        watcher.join().unwrap();
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
    }

    // -----------------------------------------------------------------------
    // #1410 — arbiter registration + the paint gate
    //
    // These take the `prompt_stdin` serial lane. `Terminal::suspend_for_prompt`
    // sets a PROCESS-GLOBAL flag, so a window held while the ~15 unserialized
    // paint tests above run in parallel would make them fail intermittently.
    // That global reach is also exactly why the gate hangs off the per-renderer
    // registration handle rather than reading the flag unconditionally: the
    // in-memory test renderers are unregistered, so the gate is inert for them
    // unless a test opts in with `register_for_test`.
    // -----------------------------------------------------------------------

    /// A registered viewport must not paint while a question is on screen.
    ///
    /// This is the whole point of #1410. `suspend_for_prompt` erases the frame,
    /// but *nothing* stopped the next paint from putting it straight back —
    /// under the question — and `restore()` would then rewind through the
    /// question to erase it.
    #[serial_test::serial(prompt_stdin)]
    #[test]
    fn a_registered_viewport_paints_nothing_while_a_prompt_is_up() {
        let writer = SharedWriter::default();
        let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
        renderer.register_for_test();

        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"a\nb\nc\n");
        let before = writer.0.lock().unwrap().len();
        assert!(before > 0, "the frame painted before the prompt");

        let window = newt_core::tty::Terminal::suspend_for_prompt(
            newt_core::tty::TerminalTaker::RichSurfaceModal,
        );
        // The arbiter erased us on the way in; that write is expected.
        let after_erase = writer.0.lock().unwrap().len();

        // Now the two real painters try again, exactly as they would in
        // production: a further tool chunk, and a geometry-driven repaint.
        renderer.write(1, ToolOutputStream::Stdout, b"d\ne\nf\n");
        paint_generation(
            &renderer.state,
            &renderer.output,
            &renderer.abandoned_through,
            1,
        );

        assert_eq!(
            writer.0.lock().unwrap().len(),
            after_erase,
            "a registered viewport wrote bytes while a question was on screen — \
             this is the overwrite bug #1410 exists to close"
        );

        drop(window);
    }

    /// Negative control: the same sequence with NO registration paints happily
    /// over the question. Without this, the test above could pass for the wrong
    /// reason (e.g. the writes were dropped for some unrelated cause).
    #[serial_test::serial(prompt_stdin)]
    #[test]
    fn an_unregistered_viewport_is_what_the_bug_looked_like() {
        let writer = SharedWriter::default();
        let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
        // deliberately NOT registered

        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"a\nb\nc\n");

        let window = newt_core::tty::Terminal::suspend_for_prompt(
            newt_core::tty::TerminalTaker::RichSurfaceModal,
        );
        let after_prompt = writer.0.lock().unwrap().len();
        renderer.write(1, ToolOutputStream::Stdout, b"d\ne\nf\n");

        assert!(
            writer.0.lock().unwrap().len() > after_prompt,
            "an unregistered viewport should still paint — if it does not, the \
             gate test above proves nothing"
        );

        drop(window);
    }

    /// `Ephemeral::erase` must be idempotent: the trait doc requires it, and
    /// `Terminal::emit_line` relies on it (it erases every registered ephemeral
    /// with no matching restore, so a second erase must write nothing).
    #[serial_test::serial(prompt_stdin)]
    #[test]
    fn erase_is_idempotent_and_writes_nothing_when_nothing_is_painted() {
        use newt_core::tty::Ephemeral as _;

        let writer = SharedWriter::default();
        let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
        renderer.register_for_test();

        // Nothing painted yet: erase must be a no-op, not a blind rewind.
        renderer.erase();
        assert!(
            writer.0.lock().unwrap().is_empty(),
            "erase wrote a rewind with no frame on screen — that would delete \
             rows the viewport does not own"
        );

        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"a\nb\nc\n");
        renderer.erase();
        let after_first = writer.0.lock().unwrap().len();
        renderer.erase();
        assert_eq!(
            writer.0.lock().unwrap().len(),
            after_first,
            "the second erase wrote bytes; Ephemeral::erase must be idempotent"
        );
    }

    /// Dropping the renderer must deregister it, or the arbiter accumulates
    /// dead entries and `suspend_for_prompt` walks them on every prompt.
    #[serial_test::serial(prompt_stdin)]
    #[test]
    fn dropping_the_renderer_deregisters_it() {
        let writer = SharedWriter::default();
        {
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.register_for_test();
            renderer.start(1);
            renderer.write(1, ToolOutputStream::Stdout, b"a\n");
        }
        // The renderer is gone. A prompt must not touch it — if the weak
        // registration were a strong one, or the handle leaked, this would
        // paint into a dropped writer's buffer or panic.
        let before = writer.0.lock().unwrap().len();
        let window = newt_core::tty::Terminal::suspend_for_prompt(
            newt_core::tty::TerminalTaker::RichSurfaceModal,
        );
        drop(window);
        assert_eq!(
            writer.0.lock().unwrap().len(),
            before,
            "a dropped renderer was still driven by the arbiter"
        );
    }

    #[test]
    fn renderer_paints_fixed_rows_and_erases_before_completion() {
        let writer = SharedWriter::default();
        let renderer = LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false);

        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"a\nb\nc\nd\n");
        assert_eq!(
            renderer.snapshot_lines(),
            [
                "▲ 1 more line above",
                "▒ b",
                "▒ c",
                "▓ d",
                "⧉ Space expands · ↑↓ scroll"
            ]
        );

        renderer.finish(1);
        assert!(!renderer.is_active());
        let bytes = writer.0.lock().unwrap().clone();
        let rendered = String::from_utf8_lossy(&bytes);
        assert!(rendered.contains("▲ 1 more line above"));
        assert!(
            rendered.contains("\u{1b}[5A"),
            "frame was not rewound: {rendered:?}"
        );
        assert!(
            rendered.contains("\u{1b}[J"),
            "frame was not erased: {rendered:?}"
        );
    }

    #[test]
    fn each_paint_and_erase_is_one_writer_batch() {
        let writer = CountingWriter::default();
        let renderer = LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false);
        renderer.start(1);

        renderer.write(1, ToolOutputStream::Stdout, b"visible\n");
        assert_eq!(
            writer.0.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "a frame must not interleave with canonical stdout"
        );

        renderer.finish(1);
        assert_eq!(
            writer.0.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "erase must be one stdout-locked batch"
        );
    }

    #[test]
    fn same_width_finish_erases_only_the_live_frame_not_the_audit_line() {
        let writer = SharedWriter::default();
        let renderer = LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false);
        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"visible\n");
        renderer.finish(1);

        let mut screen = ScreenModel::new(80);
        screen.apply(b"audit line\r\n");
        screen.apply(&writer.0.lock().unwrap());
        assert_eq!(screen.nonempty_rows(), ["audit line"]);
    }

    #[test]
    fn arrows_are_consumed_only_during_an_active_frame() {
        let renderer = LiveSpillRenderer::with_writer(SharedWriter::default(), 80, 3, false);
        assert!(!renderer.scroll_up());

        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"1\n2\n3\n4\n5\n");
        assert!(renderer.scroll_up());
        assert_eq!(renderer.snapshot_lines()[1], "▒ 2");
        assert!(renderer.scroll_down());
        renderer.finish(1);

        assert!(!renderer.scroll_down());
    }

    #[test]
    fn writes_after_finish_cannot_reopen_or_repaint_the_frame() {
        let writer = SharedWriter::default();
        let renderer = LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false);
        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"visible\n");
        renderer.finish(1);
        let finished_len = writer.0.lock().unwrap().len();

        renderer.write(1, ToolOutputStream::Stderr, b"late\n");

        assert_eq!(writer.0.lock().unwrap().len(), finished_len);
        assert!(!renderer.is_active());
    }

    #[test]
    fn abandoned_frame_is_not_erased_after_canonical_output_can_resume() {
        let writer = SharedWriter::default();
        let renderer = LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false);
        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"old frame\n");
        let before_abandon = writer.0.lock().unwrap().len();

        renderer.abandon(1);
        renderer.finish(1);
        assert_eq!(
            writer.0.lock().unwrap().len(),
            before_abandon,
            "abandon and a delayed finish must perform no terminal I/O"
        );

        renderer.start(2);
        renderer.write(2, ToolOutputStream::Stdout, b"new frame\n");
        let bytes = writer.0.lock().unwrap();
        let next_frame = String::from_utf8_lossy(&bytes[before_abandon..]);
        assert!(next_frame.contains("new frame"));
        assert!(
            !next_frame.contains("\u{1b}[5A") && !next_frame.contains("\u{1b}[J"),
            "a new generation must not erase an abandoned frame from the new cursor: {next_frame:?}"
        );
    }

    // #1303 acceptance 2 (clause B / rule 7): the abandon/teardown-miss path
    // releases mouse capture with NO renderer I/O. Because `abandon` emits
    // nothing through the renderer, the release is asserted on the GUARD's own
    // Drop side-effect handle — a sink independent of the renderer writer.
    // Mouse is a unix-only tier (`mod mouse` is `#[cfg(all(unix, …))]`), so this
    // proof only compiles/runs there.
    #[cfg(unix)]
    #[test]
    fn rule7_abandon_releases_mouse_capture_without_renderer_io() {
        use crate::mouse::{MouseCaptureGuard, MouseSink};
        use std::sync::{Arc, Mutex};

        let renderer_writer = SharedWriter::default();
        let renderer = LiveSpillRenderer::with_writer(renderer_writer.clone(), 80, 3, false);
        let mouse_sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        let released =
            || String::from_utf8_lossy(&mouse_sink.lock().unwrap()).contains("\u{1b}[?1006l");

        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"stalled frame\n");
        let before_abandon = renderer_writer.0.lock().unwrap().len();

        {
            // Capture is live for the turn.
            let _capture = MouseCaptureGuard::enable(MouseSink::Shared(mouse_sink.clone()));
            assert!(
                String::from_utf8_lossy(&mouse_sink.lock().unwrap()).contains("\u{1b}[?1006h"),
                "capture enabled on the mouse tier"
            );

            // The rule-7 teardown-miss: atomic, I/O-free abandon then a delayed
            // finish. Neither may touch the renderer writer...
            renderer.abandon(1);
            renderer.finish(1);
            assert_eq!(
                renderer_writer.0.lock().unwrap().len(),
                before_abandon,
                "abandon + delayed finish performed renderer I/O"
            );
            // ...and capture stays held until the turn scope unwinds.
            assert!(!released(), "capture must stay held inside the turn scope");
        }

        // Scope exit dropped the guard → capture released via the guard's OWN
        // handle, with the renderer writer still untouched.
        assert!(
            released(),
            "mouse capture released on the abandon/teardown path"
        );
        assert_eq!(
            renderer_writer.0.lock().unwrap().len(),
            before_abandon,
            "release must not have ridden the renderer writer"
        );
    }

    // #1303 (§8.4): the hand-rolled CSI interpreter must tolerate mouse-capture
    // enable/disable sequences so a golden/frame test never panics on them.
    #[test]
    fn screen_model_tolerates_mouse_capture_sequences() {
        let mut screen = ScreenModel::new(20);
        screen.apply(b"hi");
        let mut enable = Vec::new();
        let _ = crossterm::queue!(enable, crossterm::event::EnableMouseCapture);
        let mut disable = Vec::new();
        let _ = crossterm::queue!(disable, crossterm::event::DisableMouseCapture);
        screen.apply(&enable);
        screen.apply(&disable);
        assert_eq!(screen.nonempty_rows(), vec!["hi".to_string()]);
    }

    #[test]
    fn stale_generation_cannot_touch_a_retry_frame() {
        let renderer = LiveSpillRenderer::with_writer(SharedWriter::default(), 80, 3, false);
        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"first\n");
        renderer.finish(1);

        renderer.start(2);
        renderer.write(1, ToolOutputStream::Stdout, b"stale\n");
        renderer.finish(1);
        assert!(renderer.is_active(), "stale finish erased the retry frame");
        renderer.write(2, ToolOutputStream::Stdout, b"second\n");

        assert!(renderer
            .snapshot_lines()
            .iter()
            .any(|line| line.contains("second")));
        assert!(!renderer
            .snapshot_lines()
            .iter()
            .any(|line| line.contains("stale")));
    }

    #[test]
    fn terminal_resize_erases_old_geometry_and_reclips_the_frame() {
        let writer = SharedWriter::default();
        let geometry = Arc::new(Mutex::new((80usize, 8usize)));
        let geometry_for_renderer = geometry.clone();
        let renderer =
            LiveSpillRenderer::with_writer_and_geometry(writer.clone(), 5, false, move || {
                Some(*geometry_for_renderer.lock().unwrap())
            })
            .unwrap();
        renderer.start(1);
        renderer.write(
            1,
            ToolOutputStream::Stdout,
            b"abcdefghij\nsecond\nthird\nfourth\nfifth\n",
        );
        assert_eq!(renderer.snapshot_lines().len(), 7);

        *geometry.lock().unwrap() = (8, 5);
        renderer.write(1, ToolOutputStream::Stdout, b"sixth\n");

        let lines = renderer.snapshot_lines();
        assert_eq!(lines.len(), 4);
        for line in lines {
            assert!(display_width(&line) < 8, "row escaped width: {line:?}");
        }
        let rendered = String::from_utf8_lossy(&writer.0.lock().unwrap()).into_owned();
        // #1263: the boundary rows now carry the key legend (~27 cols), so at
        // the shrunken width 8 the OLD frame reflows to 14 physical rows —
        // the erase must cover all of them (was 8 with bare-glyph boundaries).
        assert!(
            rendered.contains("\u{1b}[14A"),
            "old reflowed frame was not fully erased before resize: {rendered:?}"
        );
    }

    /// #1427: the screen model must survive the bytes the ARBITER emits, not
    /// only the ones this renderer emits.
    ///
    /// `LineLease::erase` writes `\r` + `Clear(UntilNewLine)`, and crossterm
    /// renders that as a **parameterless** `ESC[K`. The model only had a
    /// `("2", 'K')` arm, so that sequence fell through to `panic!` and aborted
    /// the whole test binary. Any future test that drives a leased spinner and
    /// this viewport through one byte stream — exactly what #1408's
    /// consolidation needs — would have hit it.
    ///
    /// Semantics pinned here: bare `ESC[K` == `ESC[0K` == erase from the cursor
    /// to end of line, which is NOT `ESC[2K` (erase the entire line).
    #[test]
    fn screen_model_handles_the_parameterless_erase_the_arbiter_emits() {
        let mut screen = ScreenModel::new(40);
        screen.apply(b"keep this|and drop this");

        // Park the cursor after "keep this|" (column 11, 1-based) and clear to
        // end of line — what `LineLease::erase` puts on the wire after its
        // leading carriage return.
        screen.apply(b"\x1b[11G\x1b[K");
        assert_eq!(
            screen.nonempty_rows(),
            vec!["keep this|"],
            "bare ESC[K must erase from the cursor to end of line"
        );

        // The whole-line form must still mean the whole line.
        screen.apply(b"\x1b[2K");
        assert!(
            screen.nonempty_rows().is_empty(),
            "ESC[2K must still clear the entire row"
        );
    }

    #[test]
    fn width_shrink_erases_the_reflowed_physical_frame_without_stale_rows() {
        let writer = SharedWriter::default();
        let geometry = Arc::new(Mutex::new((80usize, 8usize)));
        let geometry_for_renderer = geometry.clone();
        let renderer =
            LiveSpillRenderer::with_writer_and_geometry(writer.clone(), 5, false, move || {
                Some(*geometry_for_renderer.lock().unwrap())
            })
            .unwrap();
        renderer.start(1);
        renderer.write(
            1,
            ToolOutputStream::Stdout,
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\nsecond\nthird\nfourth\nfifth\n",
        );
        let first_paint_len = writer.0.lock().unwrap().len();

        let mut screen = ScreenModel::new(80);
        screen.apply(&writer.0.lock().unwrap()[..first_paint_len]);
        screen.resize(8);
        assert!(
            screen.nonempty_rows().len() > 7,
            "the model must exercise physical reflow, not only inspect escapes"
        );

        *geometry.lock().unwrap() = (8, 5);
        renderer.write(1, ToolOutputStream::Stdout, b"sixth\n");
        screen.apply(&writer.0.lock().unwrap()[first_paint_len..]);

        assert_eq!(screen.nonempty_rows(), renderer.snapshot_lines());
    }

    #[test]
    fn finish_rechecks_geometry_even_without_another_output_chunk() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let geometry = Arc::new(Mutex::new((80usize, 8usize)));
        let calls = Arc::new(AtomicUsize::new(0));
        let geometry_for_renderer = geometry.clone();
        let calls_for_renderer = calls.clone();
        let renderer = LiveSpillRenderer::with_writer_and_geometry(
            SharedWriter::default(),
            5,
            false,
            move || {
                calls_for_renderer.fetch_add(1, Ordering::Relaxed);
                Some(*geometry_for_renderer.lock().unwrap())
            },
        )
        .unwrap();
        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"a\nb\nc\nd\ne\n");
        let before_finish = calls.load(Ordering::Relaxed);
        *geometry.lock().unwrap() = (8, 5);

        renderer.finish(1);

        assert!(
            calls.load(Ordering::Relaxed) > before_finish,
            "finish must observe a resize that arrived after the final chunk"
        );
    }

    #[test]
    fn boundary_control_expands_to_available_rows_and_collapses_again() {
        let renderer =
            LiveSpillRenderer::with_writer_and_geometry(SharedWriter::default(), 3, false, || {
                Some((80, 20))
            })
            .unwrap();
        renderer.start(1);
        renderer.write(
            1,
            ToolOutputStream::Stdout,
            b"first\nsecond\nthird\nfourth\nfifth\nsixth\n",
        );

        assert!(renderer.toggle_expanded());
        let expanded = renderer.snapshot_lines();
        assert_eq!(
            expanded.first().map(String::as_str),
            Some("▣ Space collapses · ↑↓ scroll")
        );
        assert_eq!(
            expanded.last().map(String::as_str),
            Some("▣ Space collapses · ↑↓ scroll")
        );
        assert_eq!(expanded.len(), 8);
        assert!(expanded.iter().all(|line| !line.starts_with('▓')));

        assert!(renderer.toggle_expanded());
        assert_eq!(renderer.snapshot_lines().len(), 5);
        assert_eq!(
            renderer.snapshot_lines().last().map(String::as_str),
            Some("⧉ Space expands · ↑↓ scroll")
        );
    }

    #[test]
    fn toggle_survives_transient_model_lock_contention() {
        use std::sync::mpsc;
        use std::time::Duration;

        let renderer = Arc::new(
            LiveSpillRenderer::with_writer_and_geometry(SharedWriter::default(), 3, false, || {
                Some((80, 20))
            })
            .unwrap(),
        );
        renderer.start(1);
        renderer.write(
            1,
            ToolOutputStream::Stdout,
            b"first\nsecond\nthird\nfourth\n",
        );

        let state = renderer.lock_state();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let control = {
            let renderer = renderer.clone();
            std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                done_tx.send(renderer.toggle_expanded()).unwrap();
            })
        };
        started_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "the test must exercise model-lock contention"
        );
        drop(state);

        assert!(done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the toggle stayed blocked after model state was released"));
        control.join().unwrap();
        assert_eq!(
            renderer.snapshot_lines().last().map(String::as_str),
            Some("▣ Space collapses · ↑↓ scroll")
        );
    }

    // ====================================================================
    // CompletedSpillRenderer (#1640 wiring): the completed viewport paints
    // under a REAL generation, scrolls, and erases as a pure rewind.
    // Nested so the trait import cannot make the parent module's
    // `Ephemeral::erase` calls ambiguous.
    // ====================================================================
    mod completed {
        use super::*;
        use newt_core::agentic::CompletedSpillRenderer;

        /// The regression #1640 shipped: generation 0 sat below the abandonment
        /// floor, so every completed paint silently no-opped. A completed render
        /// must actually reach the terminal and report its physical rows.
        #[test]
        fn completed_viewport_paints_and_reports_rows() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));

            let rows = renderer.render_completed("l1\nl2\nl3\nl4\nl5\n", 80, 3);
            assert!(rows > 0, "a completed render paints physical rows");
            assert!(CompletedSpillRenderer::is_active(renderer.as_ref()));

            let painted = String::from_utf8_lossy(&writer.0.lock().unwrap()).to_string();
            assert!(
                painted.contains("Completed output"),
                "the completed header row painted: {painted:?}"
            );
            assert!(painted.contains("l5"), "the tail content painted");
        }

        /// Scrolling a completed viewport works — the completed view IS
        /// `state.view`, so the existing `SpillInput` routing drives it; the
        /// repaint must survive the abandonment gate (the shipped bug killed it).
        #[test]
        fn completed_viewport_scrolls_and_repaints() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.render_completed("l1\nl2\nl3\nl4\nl5\nl6\n", 80, 3);
            assert!(renderer.scroll_up(), "a completed viewport accepts scroll");

            let before = writer.0.lock().unwrap().len();
            paint_generation(
                &renderer.state,
                &renderer.output,
                &renderer.abandoned_through,
                crate::live_spill::COMPLETED_GENERATION,
            );
            assert!(
                writer.0.lock().unwrap().len() > before,
                "the scroll repaint reached the terminal (gen-0 regression)"
            );
            let older = renderer.snapshot_lines();
            assert!(
                older.iter().any(|line| line.contains("l3")),
                "scrolled content is older lines: {older:?}"
            );
        }

        /// A live abandon (any live generation) must never gag a completed
        /// viewport: completed frames paint under `COMPLETED_GENERATION`, which
        /// sits above every possible abandonment floor.
        #[test]
        fn live_abandonment_cannot_gag_a_completed_viewport() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.abandon(7);

            let rows = renderer.render_completed("after-abandon\n", 80, 3);
            assert!(rows > 0, "completed paint survives a prior live abandon");
            assert!(CompletedSpillRenderer::is_active(renderer.as_ref()));
        }

        /// Erase is a pure rewind and releases the model state; it is idempotent
        /// and `is_active` flips false. (The shipped erase PAINTED instead.)
        #[test]
        fn completed_erase_rewinds_once_and_deactivates() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.render_completed("l1\nl2\nl3\n", 80, 3);
            let painted = writer.0.lock().unwrap().len();

            CompletedSpillRenderer::erase(renderer.as_ref());
            assert!(!CompletedSpillRenderer::is_active(renderer.as_ref()));
            let after_erase = writer.0.lock().unwrap().len();
            assert!(after_erase > painted, "the rewind wrote erase bytes");

            CompletedSpillRenderer::erase(renderer.as_ref());
            assert_eq!(
                writer.0.lock().unwrap().len(),
                after_erase,
                "a second erase writes zero bytes"
            );
        }

        /// `erase` applied through the screen model leaves no frame rows behind —
        /// the rewind math is exact.
        #[test]
        fn completed_erase_leaves_a_clean_screen() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.render_completed("alpha\nbeta\ngamma\n", 80, 3);
            CompletedSpillRenderer::erase(renderer.as_ref());

            let mut screen = ScreenModel::new(80);
            screen.apply(&writer.0.lock().unwrap());
            assert!(
                screen.rows.iter().all(|row| row.trim().is_empty()),
                "no frame residue after erase: {:?}",
                screen.rows
            );
        }

        /// A LIVE viewport is never stomped: completed rendering yields (returns
        /// 0) while a live generation owns the screen, and `is_active` stays
        /// false — a live frame is not the dismissal hook's business.
        #[test]
        fn a_live_viewport_is_never_stomped_by_completed_rendering() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.start(3);
            renderer.write(3, ToolOutputStream::Stdout, b"live-line\n");

            assert_eq!(renderer.render_completed("intruder\n", 80, 3), 0);
            assert!(
                !CompletedSpillRenderer::is_active(renderer.as_ref()),
                "a live frame is not 'completed'"
            );
            assert!(
                renderer
                    .snapshot_lines()
                    .iter()
                    .any(|line| line.contains("live-line")),
                "the live view is untouched"
            );

            // Erase must not touch the live generation either.
            CompletedSpillRenderer::erase(renderer.as_ref());
            assert!(renderer
                .snapshot_lines()
                .iter()
                .any(|line| line.contains("live-line")));
        }

        /// `discard` drops the bookkeeping with ZERO terminal writes — the
        /// turn-exit guard's contract: a stale rewind can never replay later,
        /// and the erase that would have replayed it becomes a no-op.
        #[test]
        fn discard_clears_bookkeeping_without_touching_the_terminal() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.render_completed("l1\nl2\n", 80, 3);
            let painted = writer.0.lock().unwrap().len();

            renderer.discard();
            assert!(!CompletedSpillRenderer::is_active(renderer.as_ref()));
            assert_eq!(
                writer.0.lock().unwrap().len(),
                painted,
                "discard wrote zero bytes"
            );
            CompletedSpillRenderer::erase(renderer.as_ref());
            assert_eq!(
                writer.0.lock().unwrap().len(),
                painted,
                "an erase after discard cannot replay a stale rewind"
            );
        }

        /// `discard` never touches a LIVE generation — mirroring `erase`.
        #[test]
        fn discard_leaves_a_live_viewport_alone() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.start(5);
            renderer.write(5, ToolOutputStream::Stdout, b"live-line\n");

            renderer.discard();
            assert!(
                renderer
                    .snapshot_lines()
                    .iter()
                    .any(|line| line.contains("live-line")),
                "the live view survives a completed discard"
            );
        }

        /// The returned row count matches the frame the terminal actually
        /// shows — the caller positions subsequent output with it.
        #[test]
        fn reported_rows_match_the_screen_model() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            let rows = renderer.render_completed("l1\nl2\nl3\nl4\nl5\n", 80, 3);

            let mut screen = ScreenModel::new(80);
            screen.apply(&writer.0.lock().unwrap());
            let visible = screen
                .rows
                .iter()
                .filter(|row| !row.trim().is_empty())
                .count();
            assert_eq!(rows, visible, "reported rows == painted rows");
        }

        /// After the live hand-off (`finish`), completed rendering takes the
        /// screen normally — the wired sequence display.rs actually runs.
        #[test]
        fn completed_rendering_takes_over_after_the_live_handoff() {
            let writer = SharedWriter::default();
            let renderer = Arc::new(LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false));
            renderer.start(4);
            renderer.write(4, ToolOutputStream::Stdout, b"live\n");
            renderer.finish(4);

            assert!(renderer.render_completed("done-1\ndone-2\n", 80, 3) > 0);
            assert!(CompletedSpillRenderer::is_active(renderer.as_ref()));
        }
    }
}
