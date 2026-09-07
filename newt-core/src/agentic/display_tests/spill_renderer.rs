use super::*;

// ====================================================================
// CompletedSpillRenderer routing (#1640 wiring): the committed excerpt
// stays canonical; the viewport is an addition, dismissed before the
// next tool header.
// ====================================================================

/// A writer the renderer double can also observe, so the tests can assert
/// ORDER — what had already reached the "terminal" when a trait call
/// fired — not merely that both things happened.
#[derive(Clone, Default)]

struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuf {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Records trait calls with a snapshot of the terminal at each call;
/// `active` mimics a real viewport's lifecycle.
#[derive(Default)]
struct RecordingRenderer {
    terminal: SharedBuf,
    retained: std::sync::Mutex<Vec<String>>,
    rendered: std::sync::Mutex<Vec<String>>,
    seen_at_render: std::sync::Mutex<Vec<String>>,
    seen_at_erase: std::sync::Mutex<Vec<String>>,
    erased: std::sync::atomic::AtomicUsize,
    active: std::sync::atomic::AtomicBool,
}

impl RecordingRenderer {
    fn watching(terminal: SharedBuf) -> Self {
        Self {
            terminal,
            ..Self::default()
        }
    }
}

impl crate::agentic::CompletedSpillRenderer for RecordingRenderer {
    fn retain_completed(&self, output: &str) -> Option<u64> {
        self.retained.lock().unwrap().push(output.to_string());
        Some(7)
    }

    fn render_completed(&self, output: &str, _width: usize, _max_height: usize) -> usize {
        self.rendered.lock().unwrap().push(output.to_string());
        self.seen_at_render
            .lock()
            .unwrap()
            .push(self.terminal.contents());
        self.active.store(true, std::sync::atomic::Ordering::SeqCst);
        3
    }
    fn is_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn erase(&self) {
        self.seen_at_erase
            .lock()
            .unwrap()
            .push(self.terminal.contents());
        self.erased
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.active
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The committed excerpt is NEVER replaced by the viewport — and the
/// ORDER is asserted: at the moment the renderer paints, the excerpt's
/// bytes have already reached the terminal (the flush-before-render
/// contract the cursor-relative rewind depends on).
#[test]
fn result_commits_the_excerpt_before_the_viewport_renders() {
    let terminal = SharedBuf::default();
    let renderer = std::sync::Arc::new(RecordingRenderer::watching(terminal.clone()));
    let mut display = super::ToolDisplay::new(terminal.clone(), false, 80, 3, false);
    display.set_completed_spill_renderer(renderer.clone());

    display.result("line-1\nline-2\n");

    assert!(
        terminal.contents().contains("line-2"),
        "the static excerpt committed"
    );
    assert_eq!(
        renderer.rendered.lock().unwrap().as_slice(),
        ["line-1\nline-2\n"],
        "the viewport rendered the full output"
    );
    let seen = renderer.seen_at_render.lock().unwrap();
    assert!(
        seen[0].contains("line-2"),
        "the excerpt had ALREADY reached the terminal when the viewport \
         painted — render-before-commit would put the frame above its own \
         record: {seen:?}"
    );
}

/// #1663 review F13: in SUMMARY mode the committed record is the one-line
/// marker, but the completed viewport must still receive the FULL output —
/// the marker's whole justification is that the viewport recovers detail.
#[test]
fn summary_mode_commits_the_marker_but_the_viewport_gets_full_output() {
    let terminal = SharedBuf::default();
    let renderer = std::sync::Arc::new(RecordingRenderer::watching(terminal.clone()));
    let mut display = super::ToolDisplay::new(terminal.clone(), false, 80, 3, true);
    display.set_completed_spill_renderer(renderer.clone());

    let output = "l1\nl2\nl3\nl4\nl5\nERROR: tail\n";
    display.result(output);

    let committed = terminal.contents();
    assert!(
        committed.contains("▲ 6 lines"),
        "the committed record is the collapsed marker: {committed}"
    );
    assert!(
        committed.contains("/spill open 7"),
        "the marker names the retained result it can actually reopen: {committed}"
    );
    assert!(
        !committed.contains("l1"),
        "the hidden body is NOT in the committed record: {committed}"
    );
    assert_eq!(
        renderer.rendered.lock().unwrap().as_slice(),
        [output],
        "the viewport rendered the FULL output, not the marker"
    );
    assert_eq!(
        renderer.retained.lock().unwrap().as_slice(),
        [output],
        "the completed result is retained exactly once"
    );
}

/// #1663 review F14: summary mode is confined to result() — the
/// in-progress presentation events (preview/document) keep their full
/// behavior with a summary=true ToolDisplay.
#[test]
fn summary_mode_leaves_preview_and_document_untouched() {
    use super::ToolPresentation as _;
    let terminal = SharedBuf::default();
    let mut display = super::ToolDisplay::new(terminal.clone(), false, 80, 3, true);
    let body = "p1\np2\np3\np4\np5\n";
    display.preview(body, 10);
    display.document(body);
    let out = terminal.contents();
    for l in ["p1", "p2", "p3", "p4", "p5"] {
        assert!(out.contains(l), "preview/document keep full lines: {out}");
    }
    assert!(
        !out.contains("▲ 5 lines"),
        "no collapse marker outside result(): {out}"
    );
}

/// The NEXT tool's header dismisses a still-active viewport BEFORE any
/// header byte lands — asserted by snapshot: at erase time the terminal
/// does not yet contain the header.
#[test]
fn the_next_tool_header_dismisses_an_active_viewport_first() {
    let terminal = SharedBuf::default();
    let renderer = std::sync::Arc::new(RecordingRenderer::watching(terminal.clone()));
    let mut display = super::ToolDisplay::new(terminal.clone(), false, 80, 3, false);
    display.set_completed_spill_renderer(renderer.clone());

    display.result("first tool output\n");
    assert!(crate::agentic::CompletedSpillRenderer::is_active(
        renderer.as_ref()
    ));

    display.call("run_command", "echo second");
    assert_eq!(
        renderer.erased.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the header erased the previous viewport"
    );
    assert!(!crate::agentic::CompletedSpillRenderer::is_active(
        renderer.as_ref()
    ));
    let seen = renderer.seen_at_erase.lock().unwrap();
    assert!(
        !seen[0].contains("run_command"),
        "the erase ran BEFORE the header bytes — a header-then-erase order \
         is exactly the rewind-through-canonical-rows bug: {seen:?}"
    );
    assert!(
        terminal.contents().contains("run_command"),
        "the header still printed after the erase"
    );
}

/// The cancel teardown path drops the renderer: the synthetic
/// interrupted-result must not paint a viewport that would outlive every
/// dismiss hook.
#[test]
fn a_dropped_renderer_paints_no_viewport() {
    let terminal = SharedBuf::default();
    let renderer = std::sync::Arc::new(RecordingRenderer::watching(terminal.clone()));
    let mut display = super::ToolDisplay::new(terminal.clone(), false, 80, 3, false);
    display.set_completed_spill_renderer(renderer.clone());

    display.drop_completed_spill_renderer();
    display.result("error: run_command interrupted\n");

    assert!(renderer.rendered.lock().unwrap().is_empty());
    assert!(
        terminal.contents().contains("interrupted"),
        "the static excerpt still committed"
    );
}

/// Without a renderer, the static path is BYTE-FOR-BYTE unchanged — the
/// lean / headless tiers cannot be affected by the wiring.
#[test]
fn no_renderer_means_the_static_path_alone() {
    let mut with_none = super::ToolDisplay::new(Vec::new(), false, 80, 3, false);
    with_none.result("solo output\n");
    let committed = String::from_utf8(with_none.into_inner()).unwrap();
    let expected = format!("{}\n", spill_view_lines("solo output\n", 3, 80).join("\n"));
    assert_eq!(
        committed, expected,
        "the no-renderer bytes are exactly the pre-wiring static path"
    );
}
