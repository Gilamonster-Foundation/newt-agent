//! Output budget, live/completed tool output, and completed-spill rendering.
//!
//! Types and helpers for tracking per-round token observation, delivering
//! incremental tool output to a renderer-neutral sink, and rendering a
//! scrollable completed-spill viewport in the Rich TUI.
//!
//! The types here are deliberately renderer-neutral: the loop holds only
//! `&dyn` sinks (`LiveToolOutput`, `CompletedSpillRenderer`) and the Rich TUI
//! supplies the implementations. Headless and Lean TUI callers pass `None`
//! and retain completion-only output exactly.

use crate::agentic::FileChangePresentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundObservation {
    /// Backend evaluated `prompt_tokens` and the round produced a usable
    /// response (tool calls or non-empty content). `estimated_tokens` is the
    /// loop's chars/4 estimate of the same request, for calibration.
    /// Quality-gated AND truncation-gated at the emission sites: never
    /// emitted when the prompt was within 5% of the request's `num_ctx`
    /// (Ollama may have silently dropped the head of the prompt).
    Accepted {
        prompt_tokens: u32,
        estimated_tokens: usize,
    },
    /// Persistent empty responses at `prompt_tokens` after retries (the
    /// 85%-of-safe-context silent-overflow exit).
    SuspectedOverflow { prompt_tokens: u32 },
    /// A hard HTTP error reported the endpoint's full context window. The TUI
    /// applies this through the same in-memory capability entry as subsequent
    /// accepted-round evidence, avoiding stale whole-cache overwrites.
    ContextWindow400 { context_window: u32 },
    /// Response carried only non-content fields (thinking/reasoning) with
    /// empty content.
    ThinkingOnly,
}

/// Source stream for one incremental shell-output chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutputStream {
    /// Bytes captured from the tool's standard output stream.
    Stdout,
    /// Bytes captured from the tool's standard error stream.
    Stderr,
}

/// Renderer-neutral, turn-scoped consumer for live tool output.
///
/// The interactive TUI supplies this only when both stdio handles are TTYs.
/// Headless callers pass `None` and retain completion-only output exactly.
/// Newt delivers accepted chunks serially on a bounded presentation worker;
/// slow rendering cannot backpressure a child process or its timeout. Live-only
/// chunks may be dropped when that queue is full. The independently captured
/// result envelope remains authoritative and complete.
pub trait LiveToolOutput: Send + Sync {
    /// Begin a new active-tool frame. No terminal output is required until the
    /// first chunk arrives. This runs on the bounded presentation worker, not
    /// on the tool-execution task. An implementation must ignore a generation
    /// that [`LiveToolOutput::abandon`] has already invalidated.
    fn start(&self, generation: u64);

    /// Consume one raw byte chunk. Display sanitization belongs to the sink;
    /// the authoritative result envelope is captured independently. A
    /// generation can be abandoned after a bounded completion wait, so a
    /// post-finish, abandoned, or stale-generation write must be a no-op.
    fn write(&self, generation: u64, stream: ToolOutputStream, chunk: &[u8]);

    /// Erase/close the active frame before the canonical result is printed.
    /// This is ordered after every accepted write on normal completion. Panics
    /// are contained by the dispatcher; implementations should still leave the
    /// terminal in a canonical state whenever possible.
    fn finish(&self, generation: u64);

    /// Invalidate a generation without producing terminal output.
    ///
    /// Cancellation and bounded teardown call this synchronously before the
    /// canonical result may be rendered. It must be fast, idempotent, and must
    /// make queued callbacks for `generation` no-ops. A later [`Self::start`]
    /// must discard abandoned frame bookkeeping without trying to erase it
    /// from the terminal's then-current cursor position.
    fn abandon(&self, generation: u64);
}

/// Renderer-neutral, turn-scoped consumer for COMPLETED tool output spill view.
///
/// This is the Rich TUI's completed spill renderer — an interactive, scrollable
/// viewport painted BELOW the committed `spill_view_lines` excerpt after a tool
/// result. The excerpt stays the canonical transcript record on every tier; the
/// viewport is a scroll affordance that lives only until the turn's next
/// canonical write dismisses it (`dismiss_completed_spill` at round dispatch, or
/// the next tool header). Only the Rich TUI (feature `rich-tui` + `live-spill`)
/// implements this; the Lean TUI and headless callers pass `None`.
///
/// The dismissal discipline is load-bearing: the viewport holds rows the tty
/// arbiter placed under the transcript and erases exactly those rows, so a
/// committed line that landed on them would go with the frame. Dismissing
/// first is what keeps the transcript whole. A viewport that must SURVIVE
/// canonical output (scroll during the next thinking round, or at the prompt)
/// is the #1303 step 6 retain-overlay, not this trait.
///
/// Key differences from `LiveToolOutput`:
/// - Not incremental — renders a complete, finished tool result
/// - Interactive — supports scrolling, expanding/collapsing, editor-mode nav
/// - Bounded — max 50% of screen height so a single spill can't flood a tmux
/// - Delimited — visual boundaries (⎵/⎶/⎴) mark scrollable region extents
pub trait CompletedSpillRenderer: Send + Sync {
    /// Retain a completed result for an on-demand, session-scoped viewer.
    ///
    /// Returns the content-derived handle that committed collapse markers
    /// should name (same bytes in a session, same handle). The default preserves renderers that only provide
    /// the transient inline viewport.
    fn retain_completed(&self, _output: &str) -> Option<super::SpillCid> {
        None
    }

    /// Host-supported inspection affordance, with the command fallback.
    fn recovery_hint(&self, id: &super::SpillCid) -> String {
        format!("/spill open {}", id.tail())
    }

    /// Render a completed tool result as an interactive spill viewport.
    ///
    /// `output` is the complete (stdout+stderr) tool output. `width` is the
    /// terminal column count. `max_height` is the maximum rows this viewport
    /// may occupy (enforced at 50% of terminal height by the caller).
    ///
    /// Returns the number of physical rows painted — 0 when the viewport could
    /// not take the screen (no TTY room, or a LIVE viewport is still up).
    fn render_completed(&self, output: &str, width: usize, max_height: usize) -> usize;

    /// Render a captured file change when the host supports safe styled cells.
    /// Existing hosts retain the completed text fallback and its lifecycle.
    fn render_file_change(
        &self,
        _raw_output: &str,
        output: &str,
        _change: std::sync::Arc<FileChangePresentation>,
        width: usize,
        max_height: usize,
    ) -> usize {
        self.render_completed(output, width, max_height)
    }

    /// Whether a COMPLETED viewport is currently on screen (a live viewport
    /// does not count).
    fn is_active(&self) -> bool;

    /// Erase the completed spill viewport from the terminal — a pure rewind;
    /// the committed excerpt above the frame is the durable record. No-op
    /// when no completed viewport is up.
    fn erase(&self);

    /// Drop the viewport's bookkeeping WITHOUT touching the terminal — the
    /// completed twin of the live path's `abandon`. Used at turn-exit
    /// boundaries where canonical lines may already have landed below a
    /// still-painted frame (an error `?`, a cancel return): rewinding from
    /// there would destroy rows the frame does not own, so the frame is left
    /// as inert residue instead, and — critically — no LATER erase can replay
    /// a stale rewind over the next turn's prompt. No-op when nothing is up.
    fn discard(&self) {}
}

/// Turn-scoped guard: discards any still-tracked completed viewport when the
/// provider function exits by ANY path (normal return, `?`, cancel, panic).
/// This makes the worst failure — a stale `MoveUp` rewind executing on a later
/// turn, over the prompt and the user's typed input — unrepresentable: the
/// bookkeeping cannot outlive the turn that painted the frame.
pub(super) struct CompletedSpillDismissGuard<'a>(
    pub(super) &'a Option<std::sync::Arc<dyn CompletedSpillRenderer>>,
);

impl Drop for CompletedSpillDismissGuard<'_> {
    fn drop(&mut self) {
        if let Some(renderer) = self.0 {
            renderer.discard();
        }
    }
}
