//! Reasoning and tool-round narration display: the thinking-mode switch, the
//! reasoning fold/trickle, and the narration rows a model sends with a tool
//! batch.
//!
//! Moved verbatim from `agentic/mod.rs`.

use super::{display, CompletedSpillRenderer};

/// Whether the reasoning spinner is enabled: `NEWT_THINKING` (set by
/// `/thinking`) overrides `[tui] thinking`; default on.
///
/// Public because `/settings thinking` must report what the setting IS, and
/// this is the one function that owns the precedence. A second reading of
/// `NEWT_THINKING` in the form would show `on` while `[tui] thinking = "off"`
/// quietly won (#1981).
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
    display::with_migration_notices(thinking_mode) != crate::ThinkingMode::Off
}

/// Commit a complete reasoning body as a fold block.
///
/// The non-streaming half of the same treatment `ReasoningTrickle` gives a
/// stream: a whole body arrives at once, so the budget is spent in one pass
/// rather than line by line. Both end at the same closing line, and both route
/// the count through [`display::Fold`], so the two wires cannot drift into two
/// different ways of saying the same thing.
pub(super) fn commit_reasoning_fold(
    reasoning: Option<String>,
    elapsed: std::time::Duration,
    retain: Option<&dyn CompletedSpillRenderer>,
    color: bool,
) {
    if display::with_migration_notices(thinking_mode) == crate::ThinkingMode::Off {
        return;
    }
    let Some(reasoning) = reasoning.filter(|r| !r.trim().is_empty()) else {
        return;
    };
    let budget = if display::with_migration_notices(thinking_mode) == crate::ThinkingMode::Fold {
        display::spill_lines()
    } else {
        0
    };
    let mut fold = display::ThinkingFold::default();
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
    let recovery = hint
        .as_deref()
        .map_or_else(display::Recovery::default, display::Recovery::Command);
    if let Some(closing) = fold.closing_line(elapsed, recovery) {
        shown.push(closing);
    }
    if shown.is_empty() {
        return;
    }
    let block = shown.join("\n");
    emit_notice_line(crate::tty::Level::Thinking, "", &block, color);
}

/// One permanent, arbiter-cooperating stdout line in a named register: live
/// ephemerals (a tool or thinking spinner) are erased first, and nothing
/// gates on capability, so a piped or headless run receives the same bytes
/// the `⚙` header does. Reasoning folds, and the tool-round narration below,
/// share this one writer so the two cannot drift into two ways of saying it.
pub(super) fn emit_notice_line(level: crate::tty::Level, glyph: &str, text: &str, color: bool) {
    let notice = crate::tty::Notice::new(level, glyph, text).gap(2);
    crate::tty::Terminal::emit_line(crate::tty::Sink::Stdout, notice.writer(color));
}

/// The prose a model sent WITH this tool batch — what every loop used to
/// replay into history and show to nobody — bounded the way a tool result
/// and a reasoning block are: the first `[tui] spill_lines` rows commit, the
/// rest are retained behind the one fold marker so `/spill open <id>` raises
/// the whole of it, and the conversation spine is never buried. Rows are
/// `▹`-glyphed, the hollow sibling of the `▸` reply marker, in the theme's
/// `narration` colour, so the model's interim voice is told apart from its
/// reply, its reasoning, and the operator's own text. Skipped when the wire
/// already streamed the text live (Anthropic) and when nothing readable
/// remains once the call text itself is dropped.
pub(super) fn commit_tool_round_narration(
    content: &str,
    already_printed: bool,
    retain: Option<&dyn CompletedSpillRenderer>,
    color: bool,
) {
    emit_tool_round_narration(
        content,
        already_printed,
        retain,
        display::reply_cols(crate::tty::term_cols()),
        display::spill_lines(),
        |level, glyph, text| emit_notice_line(level, glyph, text, color),
    );
}

/// The production narration path with layout and output injected for testing.
pub(super) fn emit_tool_round_narration(
    content: &str,
    already_printed: bool,
    retain: Option<&dyn CompletedSpillRenderer>,
    width: usize,
    budget: usize,
    mut emit: impl FnMut(crate::tty::Level, &str, &str),
) {
    if already_printed {
        return;
    }
    let rows = display::tool_round_prose(content);
    if rows.is_empty() {
        return;
    }
    let rendered: Vec<_> = rows
        .iter()
        .flat_map(|row| crate::tty::wrap_line(row, width))
        .collect();
    let shown = if budget == 0 {
        rendered.len()
    } else {
        rendered.len().min(budget)
    };
    for (i, line) in rendered.iter().take(shown).enumerate() {
        let glyph = if i == 0 { "▹" } else { " " };
        emit(crate::tty::Level::Narration, glyph, line);
    }
    let hidden = rendered.len() - shown;
    if hidden == 0 {
        return;
    }
    // Retain BEFORE printing the marker, so the id names a body that is
    // already there — the same order the reasoning fold keeps.
    let body = rows.join("\n");
    let retained = retain.and_then(|r| r.retain_completed(&body));
    let hint = retained
        .zip(retain)
        .map(|(id, renderer)| renderer.recovery_hint(id));
    let recovery = hint
        .as_deref()
        .map_or_else(display::Recovery::default, display::Recovery::Command);
    emit(
        crate::tty::Level::Dim,
        "▲",
        &display::Fold::lines(hidden, recovery).marker(),
    );
}

/// The streaming half of a [`ThinkingFold`]: it owns the partial-line buffer
/// and the decision of what reaches the spinner.
///
/// The split matters because reasoning arrives in token-sized chunks with no
/// respect for line boundaries, while the budget is counted in LINES. This
/// holds the tail until its newline — the same shape `Spinner::detail` already
/// uses internally, and the reason the two must not both buffer.
#[derive(Default)]
pub(super) struct ReasoningTrickle {
    fold: display::ThinkingFold,
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
        elapsed: std::time::Duration,
        retain: Option<&dyn CompletedSpillRenderer>,
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
        let recovery = hint
            .as_deref()
            .map_or_else(display::Recovery::default, display::Recovery::Command);
        let Some(line) = self.fold.closing_line(elapsed, recovery) else {
            return;
        };
        emit_notice_line(crate::tty::Level::Thinking, "", &line, color);
    }
}
