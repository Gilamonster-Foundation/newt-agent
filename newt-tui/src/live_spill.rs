//! TTY renderer for the turn-scoped active-tool spill viewport.

use crate::spill_view::{SpillStream, SpillView};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::queue;
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType};
use newt_core::{LiveToolOutput, ToolOutputStream};
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
/// count. ANSI exposes no portable reflow capability query; an emulator that
/// deliberately keeps old rows un-reflowed may therefore over-rewind only on
/// a width shrink. Normal painting and same-width cleanup use exact row counts.
pub(crate) struct LiveSpillRenderer {
    state: Arc<Mutex<RenderState>>,
    output: Arc<Mutex<OutputState>>,
    abandoned_through: Arc<AtomicU64>,
    #[cfg(any(unix, test))]
    repaint_requested: Arc<AtomicU64>,
    #[cfg(any(unix, test))]
    repaint_running: Arc<AtomicBool>,
}

impl LiveSpillRenderer {
    pub(crate) fn stdout(rows: usize, color: bool) -> Option<Self> {
        Self::with_output_and_geometry(TerminalWriter::Stdout, rows, color, || {
            crossterm::terminal::size()
                .ok()
                .map(|(columns, rows)| (usize::from(columns), usize::from(rows)))
        })
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
            geometry,
        )
    }

    fn with_output_and_geometry(
        writer: TerminalWriter,
        desired_rows: usize,
        color: bool,
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
            })),
            abandoned_through: Arc::new(AtomicU64::new(0)),
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
            .map(fixed_frame_lines)
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

fn fixed_frame_lines(view: &SpillView) -> Vec<String> {
    let frame = view.frame();
    let rows = view.visible_rows();
    let mut lines = Vec::with_capacity(rows + 2);
    lines.push(frame.top.line);
    lines.extend(frame.content.into_iter().map(|row| row.line));
    while lines.len() < rows + 1 {
        lines.push("▒".to_string());
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
                        .map(fixed_frame_lines)
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

#[cfg(test)]
mod tests {
    use super::LiveSpillRenderer;
    use crate::spill_view::display_width;
    use newt_core::{LiveToolOutput, ToolOutputStream};
    use std::io::Write;
    use std::sync::{Arc, Condvar, Mutex};

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
                (_, 'A') => self.cursor_row = self.cursor_row.saturating_sub(amount),
                (_, 'G') => self.cursor_col = amount.saturating_sub(1),
                ("2", 'K') => {
                    self.ensure_cursor_row();
                    self.rows[self.cursor_row].clear();
                }
                (_, 'J') => {
                    self.ensure_cursor_row();
                    self.rows.truncate(self.cursor_row + 1);
                    self.rows[self.cursor_row].clear();
                }
                (_, 'm') => {}
                other => panic!("unsupported screen-model CSI: {other:?}"),
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
            Some("▣"),
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

    #[test]
    fn renderer_paints_fixed_rows_and_erases_before_completion() {
        let writer = SharedWriter::default();
        let renderer = LiveSpillRenderer::with_writer(writer.clone(), 80, 3, false);

        renderer.start(1);
        renderer.write(1, ToolOutputStream::Stdout, b"a\nb\nc\nd\n");
        assert_eq!(
            renderer.snapshot_lines(),
            ["▲ 1 more line above", "▒ b", "▒ c", "▓ d", "⧉"]
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
        assert!(
            rendered.contains("\u{1b}[8A"),
            "old reflowed frame was not fully erased before resize: {rendered:?}"
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
        assert_eq!(expanded.first().map(String::as_str), Some("▣"));
        assert_eq!(expanded.last().map(String::as_str), Some("▣"));
        assert_eq!(expanded.len(), 8);
        assert!(expanded.iter().all(|line| !line.starts_with('▓')));

        assert!(renderer.toggle_expanded());
        assert_eq!(renderer.snapshot_lines().len(), 5);
        assert_eq!(
            renderer.snapshot_lines().last().map(String::as_str),
            Some("⧉")
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
            Some("▣")
        );
    }
}
