//! Reasoning/thinking display + streaming helpers (thinking-mode config).
//!
//! Split out of the agentic loop's `mod.rs` (#2672) so the loop file does not
//! carry the thinking-mode resolver, the spinner-enable gate, the fold
//! commit, the streaming trickle, and the multi-byte-aware wire-decoder —
//! all of which only read the `thinking` setting or route through `display`.
//! The `pub use reasoning::{…}` in `mod.rs` keeps these reachable at
//! `newt_core::agentic::…` for the loop and for the tests that drive it.
//!
//! These symbols are `pub`/`pub(crate)` on purpose: the public surface of the
//! agentic loop (a model's `ThinkingMode`, the spinner-gate query) lives in
//! here, not in the loop itself.

use std::time::Duration;

/// Read the `thinking` setting — `NEWT_THINKING` (set by `/thinking`) beats
/// the `[tui]` config, matching how the TUI editor resolves the same setting.
///
/// The function's sole purpose is to surface what the setting is, in a form
/// the form would show `on` while `[tui] thinking = "off"` quietly won
/// (#1981).
///
/// Public because `/settings thinking` must report what the setting IS, and
/// `/settings` deliberately reads the setting through the same code path the
/// agent uses, so a display bug is a real bug, not a test-only fiction.
#[must_use]
pub fn thinking_mode(report: &mut dyn FnMut(crate::tty::Notice<'static>)) -> crate::ThinkingMode {
    match std::env::var("NEWT_THINKING").ok().as_deref() {
        Some("off") => return crate::ThinkingMode::Off,
        // `stream` asks for the UNBOUNDED cargo-style trickle by name; plain
        // `on` asks to see the reasoning and gets the bounded default.
        Some("stream") => return crate::ThinkingMode::Stream,
        Some("on" | "fold") => return crate::ThinkingMode::Fold,
        _ => {}
    }
    crate::Config::resolve_unpublished(report)
        .ok()
        .and_then(|c| c.tui)
        .map(|t| t.thinking)
        .unwrap_or_default()
}

/// Whether reasoning is displayed AT ALL — the spinner gate.
///
/// Deliberately not `mode == Stream`. Both display modes show the live
/// spinner; they differ only in how much of the body reaches scrollback. When
/// `Fold` was added, leaving this as an equality against `Stream` would have
/// turned the new DEFAULT into "no thinking spinner, ever" with no compile
/// error anywhere — the gate-collapse this split exists to prevent.
#[must_use]
pub fn thinking_stream_enabled() -> bool {
    super::display::with_migration_notices(thinking_mode) != crate::ThinkingMode::Off
}

/// Commit a complete reasoning body as a fold block.
///
/// The non-streaming half of the same treatment `ReasoningTrickle` gives a
/// stream: a whole body arrives at once, so the budget is spent in one pass
/// rather than line by line. Both end at the same closing line, and both route
/// the count through [`super::display::Fold`], so the two wires cannot drift into two
/// different ways of saying the same thing.
pub(super) fn commit_reasoning_fold(
    reasoning: Option<String>,
    elapsed: Duration,
    retain: Option<&dyn super::CompletedSpillRenderer>,
    color: bool,
) {
    if super::display::with_migration_notices(thinking_mode) == crate::ThinkingMode::Off {
        return;
    }
    let Some(reasoning) = reasoning.filter(|r| !r.trim().is_empty()) else {
        return;
    };
    let budget =
        if super::display::with_migration_notices(thinking_mode) == crate::ThinkingMode::Fold {
            super::display::spill_lines()
        } else {
            0
        };
    let mut fold = super::display::ThinkingFold::default();
    let mut shown: Vec<String> = Vec::new();
    for line in reasoning.lines().filter(|l| !l.trim().is_empty()) {
        if fold.offer(line, budget) {
            shown.push(format!("  {line}"));
        }
    }
    // Retain BEFORE printing the handle, so the id names a body that is already
    // there rather than one that is about to be.
    let retained = retain.and_then(|r| r.retain_completed(fold.body()));
    let hint = retained
        .zip(retain)
        .map(|(id, renderer)| renderer.recovery_hint(id));
    let recovery = hint.as_deref().map_or_else(
        super::display::Recovery::default,
        super::display::Recovery::Command,
    );
    if let Some(closing) = fold.closing_line(elapsed, recovery) {
        shown.push(closing);
    }
    if shown.is_empty() {
        return;
    }
    let block = shown.join("\n");
    emit_reasoning(&block, color);
}

fn emit_reasoning(text: &str, color: bool) {
    let notice = crate::tty::Notice::new(crate::tty::Level::Thinking, "", text);
    crate::tty::Terminal::emit_line(crate::tty::Sink::Stdout, notice.writer(color));
}

/// The streaming half of a [`super::display::ThinkingFold`]: it owns the partial-line
/// buffer and the decision of what reaches the spinner.
///
/// The split matters because reasoning arrives in token-sized chunks with no
/// respect for line boundaries, while the budget is counted in LINES. This
/// holds the tail until its newline — the same shape `Spinner::detail` already
/// uses internally, and the reason the two must not both buffer.
#[derive(Default)]
pub(super) struct ReasoningTrickle {
    fold: super::display::ThinkingFold,
    /// Partial line awaiting its newline.
    partial: String,
}

impl ReasoningTrickle {
    /// Feed a chunk. Completed lines within the budget go to the spinner with
    /// the thinking style; the rest are retained by the fold and never printed.
    pub(super) fn feed(&mut self, spinner: &crate::tty::Spinner, chunk: &str, budget: usize) {
        self.partial.push_str(chunk);
        while let Some(nl) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=nl).collect();
            let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
            if trimmed.trim().is_empty() {
                continue;
            }
            if self.fold.offer(&trimmed, budget) {
                // Hand back the newline `detail` splits on, so the spinner's
                // own buffering sees exactly one complete line.
                spinner.detail(&format!("{trimmed}\n"));
            }
        }
    }

    /// Commit the closing line, retaining the body so its handle is real.
    ///
    /// Written straight to stdout rather than through the spinner, because the
    /// spinner is about to be torn down and this line is durable transcript,
    /// not progress.
    pub(super) fn close(
        &mut self,
        elapsed: Duration,
        retain: Option<&dyn super::CompletedSpillRenderer>,
        color: bool,
    ) {
        if !self.partial.trim().is_empty() {
            let tail = std::mem::take(&mut self.partial);
            self.fold.offer(tail.trim_end(), 0);
        }
        if self.fold.is_empty() {
            return;
        }
        // Retain FIRST: the handle the line prints has to name a body that is
        // already there, or the offer is a lie for as long as the race lasts.
        let retained = retain.and_then(|r| r.retain_completed(self.fold.body()));
        let hint = retained
            .zip(retain)
            .map(|(id, renderer)| renderer.recovery_hint(id));
        let recovery = hint.as_deref().map_or_else(
            super::display::Recovery::default,
            super::display::Recovery::Command,
        );
        let Some(line) = self.fold.closing_line(elapsed, recovery) else {
            return;
        };
        emit_reasoning(&line, color);
    }
}

/// Decode one wire chunk, holding an INCOMPLETE trailing character back for
/// the next one.
///
/// It replaces a per-chunk `String::from_utf8_lossy`, which was wrong for
/// exactly the reason a per-chunk `lines()` split is wrong: `reqwest` splits
/// where the socket did, not where the protocol did. A multi-byte character
/// therefore straddles a chunk boundary whenever the boundary happens to fall
/// inside it — and lossy decoding replaces the half that arrived with U+FFFD.
/// That corruption is silent and permanent: the mangled text is what gets
/// printed, returned, persisted, and re-sent to the model. Where the boundary
/// falls is a function of machine load, so the same reply is clean on an idle
/// box and mangled on a busy one; `café` arrives as `caf\u{FFFD}\u{FFFD}`.
///
/// Only a TRUNCATED tail is carried (`Utf8Error::error_len() == None`).
/// Genuinely invalid bytes are consumed lossily and the loop continues, so a
/// server emitting garbage cannot grow `carry` without bound or stall the
/// stream waiting for a continuation that will never come.
pub(super) fn decode_chunk(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    carry.extend_from_slice(chunk);
    let mut out = String::new();
    loop {
        let err = match std::str::from_utf8(carry) {
            Ok(s) => {
                out.push_str(s);
                carry.clear();
                return out;
            }
            Err(e) => e,
        };
        let good = err.valid_up_to();
        // Valid by construction, so this is a decode and never a replacement.
        out.push_str(&String::from_utf8_lossy(&carry[..good]));
        match err.error_len() {
            // Cut at the boundary: the rest is in the next chunk.
            None => {
                carry.drain(..good);
                return out;
            }
            // Not a cut — actually invalid. Spend it and keep going.
            Some(n) => {
                carry.drain(..good + n);
                out.push(char::REPLACEMENT_CHARACTER);
            }
        }
    }
}
