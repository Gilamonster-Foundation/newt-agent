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
    let stop = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = mpsc::channel();
    let watcher = {
        let renderer = renderer.clone();
        let cancel = cancel.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            crate::turn_input::watch_for_interrupt_fd(
                pipe[0],
                &cancel,
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
// These take both the `prompt_stdin` and `tty_arbiter` serial lanes.
// Region-owning tests use the latter: their held rows would make this
// Refuse-policy prompt inert rather than suspending the viewport.
// `Terminal::suspend_for_prompt` sets a PROCESS-GLOBAL flag, so a window
// held while the ~15 unserialized
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
#[serial_test::serial(tty_arbiter, prompt_stdin)]
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
    assert!(
        after_erase > before,
        "the prompt must acquire the terminal and erase the registered frame"
    );

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
#[serial_test::serial(tty_arbiter, prompt_stdin)]
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
#[serial_test::serial(tty_arbiter, prompt_stdin)]
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
#[serial_test::serial(tty_arbiter, prompt_stdin)]
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
            "⧉ Space to expand · ↑↓ scroll"
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
    // #1263: the boundary rows carry the key legend, so at the shrunken
    // width 8 the OLD frame reflows to 16 physical rows — the erase must
    // cover all of them (was 8 with bare-glyph boundaries).
    //
    // 16, not the 14 this once expected: each 32-cell legend takes FIVE
    // rows at width 8, not four, because `↑` and `↓` are double-width and
    // refuse to straddle the margin. `physical_rows` wraps the way the
    // terminal does; the old `width.div_ceil(columns)` under-counted by one
    // row per legend and stranded two. The ScreenModel in
    // `width_shrink_erases_…` independently reflows the same frame and
    // agrees: 5 + 8 + 1 + 1 + 1 + 1 + 5 = 22 there, 5 + 2 + 1 + 1 + 1 + 1 +
    // 5 = 16 here.
    assert!(
        rendered.contains("\u{1b}[16A"),
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

/// A double-width glyph cannot straddle the right margin, so the terminal
/// breaks the line EARLY and leaves the last cell unused — which makes the
/// line taller than `width.div_ceil(columns)` predicts.
///
/// That arithmetic was what the erase used to rewind a reflowed frame, so
/// it moved `MoveUp` too few rows and stranded the top of the old frame
/// permanently (nothing else clears it; the erase drops its bookkeeping
/// unconditionally). It survived because the boundary legend happened to
/// wrap where the arithmetic agreed; two characters of new legend text
/// pushed it over and the stale rows appeared.
#[test]
fn wide_glyphs_wrap_early_so_rows_exceed_the_width_over_columns_estimate() {
    use super::{physical_rows, rendered_width};

    // 32 cells: `⧉`+`▲`-family glyphs are narrow, but `↑` and `↓` are not.
    let legend = "⧉ Space to expand · ↑↓ scroll";
    assert_eq!(rendered_width(legend), 32);
    // The arithmetic the erase used to trust.
    assert_eq!(32usize.div_ceil(8), 4);
    // What a terminal actually does — one row more, because `↑` will not
    // split across the margin.
    assert_eq!(physical_rows(legend, 8), 5);

    // All-narrow text still agrees with the simple division, so the fix is
    // not a blanket +1.
    assert_eq!(physical_rows("abcdefghijkl", 8), 2);
    assert_eq!(physical_rows("abcdefgh", 8), 1);
    // A zero-width combining mark rides the cell before it, never forcing
    // a break of its own.
    assert_eq!(physical_rows("abcdefgh\u{0301}", 8), 1);
    // An empty line still occupies the row it was printed on.
    assert_eq!(physical_rows("", 8), 1);
    // A single glyph wider than the terminal cannot be split any further.
    assert_eq!(physical_rows("↑↑", 1), 2);
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
    let renderer =
        LiveSpillRenderer::with_writer_and_geometry(SharedWriter::default(), 5, false, move || {
            calls_for_renderer.fetch_add(1, Ordering::Relaxed);
            Some(*geometry_for_renderer.lock().unwrap())
        })
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
        Some("⧉ Space to expand · ↑↓ scroll")
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
