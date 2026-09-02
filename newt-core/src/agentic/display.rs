//! Terminal output helpers for the agentic loop.
//!
//! Moved verbatim from `newt-tui` in Step 9.7 so the loop and the inline
//! progress it prints (tool calls, retries, trim notices) stay together.
//! Everything here writes straight to stdout — headless callers (Step 9.8's
//! ACP worker) run with `color: false` and capture/ignore the stream.

use crossterm::{
    execute,
    style::{Color as CtColor, Print, ResetColor, SetForegroundColor},
};
use std::io::{self, Write};

// The terminal-line primitives (palette, width, single-line fitting) moved to
// the public `newt_core::tty` module — `agentic::display` is private with a
// curated re-export list, and *that privacy was the mechanical cause* of the
// duplicate frame sets and open-coded erase escapes elsewhere in the workspace.
// Re-exported here so every call site in `agentic` is unchanged.
pub(crate) use crate::tty::term_cols;
pub use crate::tty::NEWT_ORANGE_CT;
// `FADE_CT`'s only consumer is `agentic::markdown::emitter`, which is itself
// `markdown`-gated — so under `--no-default-features` this re-export is unused
// and `-D warnings` refuses it (#1890). Gated with its consumer rather than
// deleted, because the curated re-export list above is the deliberate design:
// every `agentic` call site names `display`, not `tty`.
#[cfg(feature = "markdown")]
pub(crate) use crate::tty::FADE_CT;

// The multi-line wrapper moved up to `tty::width::wrap_line` with the rest of
// the width model (`docs/decisions/tty_widget_suite.md` §3.0). Aliased under its
// old name so every call site and every test in this module is unchanged — the
// promotion is a move, not a behavior change.
pub(crate) use crate::tty::width::wrap_line as wrap_to_width;

/// Print a newt narrator line.
///
/// The `▸` marker stays the **default text color**: a colored sigil on every
/// narrator line reads as noise, and the saturated logo orange is exactly the
/// hue that's hard to parse on this operator's display (accessibility note —
/// never lean on a deep saturated color for anything readable). No-color: `>`.
pub fn print_newt(msg: &str, color: bool, verbose: bool) {
    println!("{}", newt_line(msg, color, verbose));
}

/// The narrator line [`print_newt`] prints, as a string.
///
/// Split out so a caller holding a [`crate::tty::PromptWindow`] can route the
/// SAME bytes through `PromptWindow::notice` instead of `println!` — a notice
/// emitted while a question is on screen must go through the arbiter, or it
/// races the very ticker it was meant to be protected from.
pub fn newt_line(msg: &str, color: bool, verbose: bool) -> String {
    let prefix = if color {
        if verbose {
            "newt ▸  "
        } else {
            "▸  "
        }
    } else if verbose {
        "newt >  "
    } else {
        ">  "
    };
    format!("{prefix}{msg}")
}

/// Print one row of a selectable list in newt's default list style.
///
/// The **active** row is flagged with a red `▸` margin sigil and a green
/// `◀ active` tag; inactive rows align under it with two leading spaces (the
/// `▸ ` sigil consumes one of those two columns, so labels line up). The label
/// itself is always default-colored — only the small arrow sigils carry color,
/// and the words `▸`/`active` carry the meaning too, so nothing depends on
/// color alone.
pub fn print_list_item(label: &str, active: bool, color: bool) {
    if !active {
        println!("  {label}");
        return;
    }
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::Red),
            Print("▸ "),
            ResetColor,
            Print(label),
            Print("  "),
            SetForegroundColor(CtColor::Red),
            Print("◀ "),
            SetForegroundColor(CtColor::Green),
            Print("active"),
            ResetColor,
            Print("\n"),
        )
        .ok();
    } else {
        println!("> {label}  <- active");
    }
}

/// Print a harness-originated notice — an adaptation/diagnostic message from
/// newt *itself* (context-budget fail-open, compression latch, …), NOT model
/// output and NOT a plain narrator line. Rendered in amber with a `newt:` label
/// so it reads as the harness speaking and doesn't blend into the conversation
/// (the failure mode the operator flagged). Multi-line text stays amber; the
/// marker leads the first line.
pub fn print_harness_notice(msg: &str, color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkYellow),
            Print(format!("⚠  newt: {msg}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("⚠  newt: {msg}");
    }
    io::stdout().flush().ok();
}

/// Print a single-line debug diagnostic (dimmed, prefix `[debug]`).
/// Only called when `ChatCtx.debug` is true — guard at the call site.
pub(crate) fn print_debug(msg: &str, color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("[debug] {msg}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("[debug] {msg}");
    }
    io::stdout().flush().ok();
}

/// Print a deeper diagnostic intended for backend compatibility issue reports.
pub(crate) fn print_trace(msg: &str, color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("[trace] {msg}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("[trace] {msg}");
    }
    io::stdout().flush().ok();
}

/// Insert thousands separators into a token count for display.
pub(crate) fn fmt_tokens(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

// --- Context-budget gauge formatting (Step 24.5, #559) ---------------------
//
// The token gauge shows how full the context window is BEFORE compression
// fires. Two display registers: a `used/budget` fraction in `k` (thousands) for
// the live header — `899k/1024k` — and a single compact figure that rolls a
// round window up to `M` (where **1M = 1024k**) for summary contexts.

/// Tokens as a rounded `k` (thousands) figure, e.g. `899_000 → "899k"`. The
/// fraction register used by the live gauge.
pub(crate) fn fmt_tokens_k(n: u32) -> String {
    format!("{}k", (n + 500) / 1000)
}

/// Tokens as a compact figure: `"Nk"` below 1024k, otherwise `"N[.N]M"` with
/// **1M = 1024k** (so a 1,024,000-token window reads `1M`, 1,536,000 → `1.5M`).
pub fn fmt_tokens_compact(n: u32) -> String {
    let k = (n + 500) / 1000;
    if k >= 1024 {
        let m = k as f64 / 1024.0;
        if (m - m.round()).abs() < 0.05 {
            format!("{}M", m.round() as u64)
        } else {
            format!("{m:.1}M")
        }
    } else {
        format!("{k}k")
    }
}

/// `used/budget` gauge in `k`, e.g. `"899k/1024k"`.
pub fn fmt_token_gauge(used: u32, budget: u32) -> String {
    format!("{}/{}", fmt_tokens_k(used), fmt_tokens_k(budget))
}

/// Fill-level band for the gauge — color-type-agnostic so each caller maps it to
/// its own palette (crossterm for the scroller, ratatui for the rich header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GaugeLevel {
    /// Under 75% — comfortable.
    Ok,
    /// 75–90% — approaching the send budget.
    Warn,
    /// 90%+ — compression is imminent.
    Critical,
}

/// Classify a `used/budget` fill into a [`GaugeLevel`] (green / amber / red).
pub fn gauge_level(used: u32, budget: u32) -> GaugeLevel {
    let pct = if budget == 0 {
        0
    } else {
        (used as u64 * 100 / budget as u64) as u32
    };
    if pct >= 90 {
        GaugeLevel::Critical
    } else if pct >= 75 {
        GaugeLevel::Warn
    } else {
        GaugeLevel::Ok
    }
}

/// Print a context-overflow adaptation notice to the TUI stream.
pub(crate) fn emit_overflow_notice(
    color: bool,
    usage: Option<&crate::TokenUsage>,
    safe_context: Option<u32>,
    model: &str,
    attempt: u32,
) {
    let token_str = usage
        .map(|u| format!("{} tokens", fmt_tokens(u.input_tokens)))
        .unwrap_or_else(|| "unknown tokens".to_string());
    let safe_str = safe_context
        .map(|s| format!(" > {} safe window for {model}", fmt_tokens(s)))
        .unwrap_or_default();
    let msg = format!(
        "⚠  context overflow likely ({token_str}{safe_str})\n⟳  trimming context and retrying (attempt {attempt}/2)…"
    );
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkYellow),
            Print(format!("{msg}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("{msg}");
    }
    io::stdout().flush().ok();
}

/// Print a one-line compression notice (Step 18.4, #247). Always visible —
/// the B6 baseline's failure mode was context loss with *no event anywhere*;
/// "visibly degrades" is the acceptance bar.
/// The compression-notice text + whether it is the **loud static-marker last
/// resort** (Step 24.7, #559). Pure → testable; `emit_compression_notice`
/// prints it. Distinct registers per outcome: `✓` summarized, `⧉` pruned, and a
/// loud `⛔` for the static marker — the #548 "silent context loss" fix.
pub(crate) fn compression_notice_text(
    action: super::compress::CompressAction,
    before: usize,
    after: usize,
    suffix: &str,
) -> (String, bool) {
    use super::compress::CompressAction;
    let b = fmt_tokens(before.min(u32::MAX as usize) as u32);
    let a = fmt_tokens(after.min(u32::MAX as usize) as u32);
    match action {
        CompressAction::StaticFallback => (
            format!(
                "⛔  summary unavailable — context compacted to a marker \
                 (~{b} → ~{a} est. tokens{suffix}). Re-read files if needed."
            ),
            true,
        ),
        CompressAction::Summarized => (
            format!("✓  context summarized: ~{b} → ~{a} est. tokens{suffix}"),
            false,
        ),
        other => (
            format!(
                "⧉  context compressed: ~{b} → ~{a} est. tokens ({}{suffix})",
                other.describe()
            ),
            false,
        ),
    }
}

pub(crate) fn emit_compression_notice(
    color: bool,
    before: usize,
    after: usize,
    action: super::compress::CompressAction,
    suffix: &str,
) {
    let (msg, loud) = compression_notice_text(action, before, after, suffix);
    // The static-marker last resort is RED + loud (24.7) so it can't be missed;
    // other outcomes stay amber.
    let hue = if loud {
        CtColor::Red
    } else {
        CtColor::DarkYellow
    };
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(hue),
            Print(format!("{msg}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("{msg}");
    }
    io::stdout().flush().ok();
}

/// Print a visible retry indicator to the TUI so the user knows why there's
/// a pause rather than seeing a silent hang.
pub(crate) fn retry_indicator_text(
    attempt: u32,
    max_retries: u32,
    delay: std::time::Duration,
    class: Option<super::observability::ErrorClass>,
) -> String {
    use super::observability::ErrorClass;

    let reason = match class {
        Some(ErrorClass::Timeout) => "request timed out",
        Some(ErrorClass::Transport) => "connection lost",
        Some(ErrorClass::Model) => "backend returned a retryable error",
        Some(ErrorClass::Harness) => "request failed in the harness",
        None => "connection lost",
    };
    let delay_s = delay.as_secs_f32();
    format!("  ↻ {reason} — retrying in {delay_s:.1}s (retry {attempt}/{max_retries})…")
}

pub(crate) fn print_retry_indicator(
    attempt: u32,
    max_retries: u32,
    delay: std::time::Duration,
    error: &anyhow::Error,
    color: bool,
) {
    let msg = retry_indicator_text(
        attempt,
        max_retries,
        delay,
        super::observability::error_class(error),
    );
    crate::tty::Notice::new(crate::tty::Level::Warn, "", msg).emit(
        crate::tty::LineCaps::Own,
        crate::tty::Sink::Stdout,
        color,
    );
}

fn tool_call_lines(name: &str, detail: &str, cols: usize) -> Vec<String> {
    // #1153: WORD-WRAP the full detail across as many lines as it needs — the
    // operator must be able to audit exactly what command/path ran, so the
    // command is never truncated with `…`. Continuation lines are indented to
    // align under the detail. Keep the "⚙  {name}: " prefix whole (it's short).
    let prefix_w = 3 + name.chars().count() + 2; // "⚙  " + name + ": "
    let detail_w = cols.saturating_sub(prefix_w).max(8);
    let wrapped = wrap_to_width(detail, detail_w);
    let indent = " ".repeat(prefix_w);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("⚙  {name}: {line}")
            } else {
                format!("{indent}{line}")
            }
        })
        .collect()
}

/// #1235/#1973: the SPILL VIEW — a bounded rendering of completed tool
/// output. Pure: returns the exact lines to print (gutter glyphs included)
/// so the unit tier tests the geometry without a terminal.
///
/// **#1973 — why this is head+tail, not tail-only.** Before this, an
/// overflowing block showed only its LAST `view` lines, on the reasoning
/// that "the tail is where grep hits and errors live" — true for
/// cargo-style output, where the compiler emits diagnostics and a final
/// summary line at the end. It is FALSE for "print results, then something
/// in cleanup crashes" — a shape at least as common (any script/test
/// harness whose success path finishes before an unrelated teardown
/// exception). The live incident: an MCP integration test printed
/// `Response 0`..`Response N` confirming the protocol worked, then an
/// unrelated asyncio cleanup raised `ProcessLookupError`; tail-only showed
/// ONLY the traceback, so the one visible artifact at the exact moment a
/// report claimed "verified working end-to-end" was a crash — the
/// confirming responses were entirely inside the hidden head. A fold that
/// must guess which end holds the decisive content is exactly that: a
/// guess. Showing both ends removes the guess. This is a display-only
/// fix — it does not change what gets recorded (`#1947` is the separate,
/// ledger-side rule that an agent's own claims must be backed by evidence
/// on record); this only changes what the OPERATOR sees without taking
/// action, since `/spill N` is a recovery step available only AFTER the
/// misleading view has already been read.
///
/// Considered and rejected: content-sniffing for "looks like a traceback /
/// looks like a result line" (the issue's other proposed alternative). It
/// would need a pattern per language/harness (Python tracebacks, Rust
/// panics, JS unhandled rejections, ad-hoc `PASS`/`FAIL`/JSON-RPC shapes,
/// …) and fails exactly the shapes nobody anticipated — the failure mode
/// this whole issue is about. A structural head+tail split needs no
/// language knowledge and degrades gracefully: it can't perfectly center
/// on the true head/tail boundary in content it hasn't classified, but it
/// can never fully hide either end the way a single-ended fold can.
///
/// Shape: when the output fits in `view` lines it is shown whole with the
/// `▒` gutter and the `…` end-of-output marker (unchanged from before).
/// When it overflows, the **first** and **last** portions are shown — a
/// head (proves what happened before any later failure) and a tail (still
/// where cargo-style errors live) — with one `▲` marker in between naming
/// the hidden count, and the `▓` thumb still marks the true tail position.
/// The head/tail split is roughly even, tail getting any odd remainder row
/// (`content_budget - content_budget / 2`): either end could hold the
/// decisive content depending on the output's shape, and the small tail
/// bias preserves today's historical prior that errors conventionally sit
/// at the very end. `view == 0` means unbounded (no gutter — the raw
/// historical behavior).
///
/// **The reserve fix, checked specifically (#1973's small-render finding —
/// a 12-line block clipped 2 MORE lines than its `view` budget implied it
/// should).** The pre-fix marker line was an unaccounted-for EXTRA row: a
/// `view`-line budget rendered `view` content lines plus the boundary
/// marker plus the trailing `…` — `view + 2` total rows, not `view`. The
/// reserve for that marker was 0 when it should have been at least 1 — off
/// by exactly the marker's own height. Fixed here by reserving 1 row for
/// the marker OUT OF `view` before splitting head/tail
/// (`content_budget = view.saturating_sub(1)`), tightening the overshoot
/// to `view + 1` (the pre-existing trailing `…` is the one row left
/// unreserved — see the note on it below). This is the completion-time foundation;
/// live tail-follow and interactive scrolling are gated on a superseding
/// decision doc (plain_scroller_tui.md bans multi-line redraws) plus a streaming
/// dispatch seam — see #1235 for the ladder.
/// #1235: the resolved spill-view height — a process-wide knob following the
/// `output_budget` atomics precedent (set at per-turn config resolve, read at
/// the display site) so the value reaches the shell echo without threading a
/// parallel param through every tool signature. Default 3 (`[tui] spill_lines`).
static SPILL_LINES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(3);

/// Set the spill-view height (per-turn, from the resolved config).
pub fn set_spill_lines(n: usize) {
    SPILL_LINES.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// The current spill-view height.
pub(crate) fn spill_lines() -> usize {
    SPILL_LINES.load(std::sync::atomic::Ordering::Relaxed)
}

/// #1640 Layer 1 (meta-scroller): whether committed tool results COLLAPSE to a
/// one-line summary instead of the multi-row excerpt. The conversation spine
/// (the operator's prompts and the model's replies) is what the operator needs
/// to keep in view; a wall of grey per tool is what buries it. Same
/// process-wide-knob precedent as `SPILL_LINES` above: seeded per turn by the
/// active surface (rich = on, lean = off — lean shows FULL output, #1640), read
/// at the display site. Default off so headless/CLI paths keep today's excerpt.
static SPILL_SUMMARY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set summary-collapse mode (per-turn, from the active surface).
pub fn set_spill_summary(on: bool) {
    SPILL_SUMMARY.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether committed tool results collapse to a one-line summary.
pub(crate) fn spill_summary() -> bool {
    SPILL_SUMMARY.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether a mouse click reaches the live viewport — the `[tui] mouse_viewport`
/// opt-in AND the capability gate, resolved once by the surface that mounts the
/// frame. Same process-wide-knob precedent as `SPILL_LINES` / `SPILL_SUMMARY`
/// above: seeded per turn where the guard is taken, read at the marker site.
///
/// It exists so a fold marker cannot promise a click on a surface where nothing
/// is listening for one. Default off, which is also the config default.
static MOUSE_RECOVERY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record whether mouse capture is live, from the surface that took the guard.
pub fn set_mouse_recovery(on: bool) {
    MOUSE_RECOVERY.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The recovery a MOUNTED live viewport may honestly advertise: a click only
/// when capture is actually on, otherwise the keys that always work.
pub fn interactive_recovery() -> Recovery<'static> {
    if MOUSE_RECOVERY.load(std::sync::atomic::Ordering::Relaxed) {
        Recovery::Click
    } else {
        Recovery::Keys
    }
}

/// The one place that names the recovery path out of a truncated view (#1433).
///
/// Every truncation marker interpolates THIS, so a third one cannot silently
/// ship without it — which is exactly how `:{HIDDEN_TAIL}` below drifted from
/// the boundary marker and left the operator stuck. codex solves it the same
/// way with a single `TRANSCRIPT_HINT` constant; pi derives the text from its
/// live binding table so a rebind can never desync a hint.
pub(crate) const SPILL_RECOVERY_HINT: &str = "/spill N raises this view";

/// How the operator gets hidden content back.
///
/// **Derived from the surface, never chosen at a call site.** A marker must
/// not advertise an affordance the surface does not have, and the two surfaces
/// genuinely differ: the committed excerpt is durable scrollback that OUTLIVES
/// the viewport which could answer a keypress, so it can only name a command.
/// A mounted live viewport is the one place `space` is a true statement.
///
/// This is the fix for a real operator report (#1263): the inert committed
/// excerpt shares the ▲/▒/▓ glyphs with the live scroller, so it masqueraded as
/// interactive and the operator sat pressing keys at printed text. Encoding the
/// answer in a type means the lie cannot be re-typed at a sixth call site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Recovery<'a> {
    /// Durable scrollback, a pipe, or a headless run — nothing here listens,
    /// so name the command that raises the view. Carries its own text because
    /// a Rich surface that retained the body substitutes the EXACT recovery
    /// (`/spill open 7`) for the generic [`SPILL_RECOVERY_HINT`].
    Command(&'a str),
    /// A live viewport is mounted: Space expands it.
    Keys,
    /// …and mouse capture is on (`[tui] mouse_viewport`), so a click lands too.
    Click,
}

impl Default for Recovery<'_> {
    fn default() -> Self {
        Self::Command(SPILL_RECOVERY_HINT)
    }
}

impl Recovery<'_> {
    /// The handle, as the operator would perform it.
    pub fn handle(&self) -> &str {
        match self {
            Self::Command(cmd) => cmd,
            Self::Keys => "space to expand",
            Self::Click => "click or space to expand",
        }
    }
}

/// What is hidden, in the units it was hidden by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hidden {
    /// Logical lines of the output.
    Lines(usize),
    /// Wrapped display rows of ONE very long line — the #1433 pathological
    /// case, where "lines" would be a lie (there is only one).
    Rows(usize),
}

/// The ONE rendering of "there is more, and here is how to get it."
///
/// Before this type, `display.rs` carried FIVE hand-written phrasings of that
/// one sentence — `{n} lines omitted`, `{n} more lines above`, `{n} wrapped
/// rows omitted`, `▲ {n} lines · {tail}`, and `… ({n} more lines hidden)` —
/// each with its own separator, its own verb, and its own idea of the noun. A
/// sixth was always one edit away, and the count and the recovery hint could
/// drift apart independently. Per the repo's reuse discipline, the fix is not
/// to correct five sites but to leave one.
///
/// Renders as `{count} {noun} hidden  [{handle}]`: what happened, then the
/// handle you can reach for, visually separated. The bracket is the promise —
/// it is only ever filled with something the surface can actually do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fold<'a> {
    hidden: Hidden,
    recovery: Recovery<'a>,
}

impl<'a> Fold<'a> {
    pub fn lines(hidden: usize, recovery: Recovery<'a>) -> Self {
        Self {
            hidden: Hidden::Lines(hidden),
            recovery,
        }
    }

    pub fn rows(hidden: usize, recovery: Recovery<'a>) -> Self {
        Self {
            hidden: Hidden::Rows(hidden),
            recovery,
        }
    }

    /// The marker text, WITHOUT a leading glyph — each call site owns its own
    /// (`▲` for hidden-above, `…` for a preview tail), because the glyph
    /// carries direction and only the site knows the direction.
    pub fn marker(&self) -> String {
        let (count, noun) = match self.hidden {
            Hidden::Lines(n) => (n, if n == 1 { "line" } else { "lines" }),
            Hidden::Rows(n) => (
                n,
                if n == 1 {
                    "wrapped row"
                } else {
                    "wrapped rows"
                },
            ),
        };
        format!("{count} {noun} hidden  [{}]", self.recovery.handle())
    }
}

impl std::fmt::Display for Fold<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.marker())
    }
}

pub(crate) fn spill_view_lines(output: &str, view: usize, columns: usize) -> Vec<String> {
    spill_view_lines_with_hint(output, view, columns, Recovery::default())
}

fn spill_view_lines_with_hint(
    output: &str,
    view: usize,
    columns: usize,
    recovery: Recovery<'_>,
) -> Vec<String> {
    // #1433: the budget is spent in RENDERED rows, not logical lines — counting
    // lines let one 4000-char diagnostic consume an unbounded number of them.
    //
    // But the emitted text stays UNWRAPPED. This excerpt is the canonical
    // committed block, and `plain_scroller_tui.md` names scrollback as
    // "searchable, copy-pasteable, and capturable with script/asciinema".
    // Hard-wrapping would insert a newline and a gutter mid-sentence: visually
    // identical to the terminal's own soft-wrap, but it breaks copy-paste and
    // breaks search across the wrap point. So we MEASURE with the wrapper and
    // EMIT the original.
    //
    // The gutter glyph costs two columns ("▒ "), so content wraps at
    // `columns - 2`.
    let content_width = columns.saturating_sub(2).max(1);
    let rows_of = |l: &str| wrap_to_width(l, content_width).len().max(1);

    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    if view == 0 {
        return lines.iter().map(|l| (*l).to_string()).collect();
    }

    // Does everything fit? Walk from the tail with the FULL (unreserved)
    // budget — the cheapest way to answer "does it fit" without a separate
    // full-content row sum, and it reproduces the pre-#1973 behavior exactly
    // when nothing needs to move (this walk alone decided the whole render
    // before #1973; it now only decides fits-vs-overflow).
    let mut kept = 0usize;
    let mut used = 0usize;
    for l in lines.iter().rev() {
        let r = rows_of(l);
        if kept > 0 && used + r > view {
            break;
        }
        kept += 1;
        used += r;
        if used >= view {
            break;
        }
    }
    let start = lines.len() - kept;

    // A single line wider than the whole budget is the pathological case
    // #1433 was about. #1973: it gets the SAME head+tail treatment as the
    // multi-line case for the same reason — the decisive part of one huge
    // line (a JSON blob, a base64 payload) is not reliably at its end either.
    if used > view && kept == 1 {
        return spill_wide_line_head_and_tail(lines[0], view, content_width, recovery);
    }

    if start == 0 {
        // Fits: unchanged from pre-#1973.
        let mut out = Vec::with_capacity(lines.len() + 1);
        for l in &lines {
            out.push(format!("▒ {l}"));
        }
        out.push("…".to_string());
        return out;
    }

    // #1973 OVERFLOW: show both ends rather than tail-only — see the module
    // doc above for the full reasoning (evidence inversion + the considered
    // and rejected content-sniffing alternative) and the reserve fix.
    //
    // Reserve 1 row for the boundary marker OUT OF `view` (the reserve-off-
    // by-the-marker's-own-height fix) before splitting; roughly even between
    // head and tail, tail taking any odd remainder row.
    let content_budget = view.saturating_sub(1);
    if content_budget == 0 {
        // view == 1: no room to reserve for a marker AND show content from
        // both ends. Not a realistic operator setting — fall back to the
        // pre-#1973 pure-tail shape using the already-computed full walk.
        let tail = &lines[start..];
        let mut out = vec![format!("▲ {}", Fold::lines(start, recovery))];
        for (i, l) in tail.iter().enumerate() {
            let glyph = if i + 1 == tail.len() { '▓' } else { '▒' };
            out.push(format!("{glyph} {l}"));
        }
        out.push("…".to_string());
        return out;
    }
    let head_budget = content_budget / 2;
    let tail_budget = content_budget - head_budget;

    let mut head_kept = 0usize;
    let mut head_used = 0usize;
    for l in &lines {
        let r = rows_of(l);
        if head_kept > 0 && head_used + r > head_budget {
            break;
        }
        if r > head_budget {
            break; // can't fit even one line in the head budget
        }
        head_kept += 1;
        head_used += r;
        if head_used >= head_budget {
            break;
        }
    }

    let max_tail_lines = lines.len() - head_kept;
    let mut tail_kept = 0usize;
    let mut tail_used = 0usize;
    for l in lines.iter().rev() {
        if tail_kept >= max_tail_lines {
            break;
        }
        let r = rows_of(l);
        if tail_kept > 0 && tail_used + r > tail_budget {
            break;
        }
        tail_kept += 1;
        tail_used += r;
        if tail_used >= tail_budget {
            break;
        }
    }

    let hidden = lines.len() - head_kept - tail_kept;
    if hidden == 0 {
        // The reserved (smaller) split still covered everything after all.
        let mut out = Vec::with_capacity(lines.len() + 1);
        for l in &lines {
            out.push(format!("▒ {l}"));
        }
        out.push("…".to_string());
        return out;
    }

    let mut out = Vec::with_capacity(head_kept + tail_kept + 2);
    for l in &lines[..head_kept] {
        out.push(format!("▒ {l}"));
    }
    // #1263: this excerpt is PLAIN PRINTED TEXT — it deliberately shares the
    // ▲/▒/▓ glyphs with the live viewport, so without this hint it masqueraded
    // as the interactive scroller (the diagnosed operator tried to expand it in
    // scrollback). Name the real recovery path at the point of use.
    out.push(format!("▲ {}", Fold::lines(hidden, recovery)));
    let tail_start = lines.len() - tail_kept;
    for (i, l) in lines[tail_start..].iter().enumerate() {
        let glyph = if i + 1 == tail_kept { '▓' } else { '▒' };
        out.push(format!("{glyph} {l}"));
    }
    out.push("…".to_string());
    out
}

/// The #1433 pathological case (one line wider than the whole row budget),
/// given the SAME head+tail treatment as the multi-line path (#1973) — see
/// [`spill_view_lines_with_hint`]'s module doc. Operates on already-wrapped
/// rows, so every row costs exactly 1 (no per-row wrap-width accounting
/// needed, unlike the multi-line split).
fn spill_wide_line_head_and_tail(
    line: &str,
    view: usize,
    content_width: usize,
    recovery: Recovery<'_>,
) -> Vec<String> {
    let wrapped = wrap_to_width(line, content_width);
    let content_budget = view.saturating_sub(1).max(1);
    let head_budget = (content_budget / 2).min(wrapped.len());
    let tail_budget = (content_budget - content_budget / 2).min(wrapped.len() - head_budget);
    let tail_start = wrapped.len() - tail_budget;
    let hidden = tail_start - head_budget;

    let mut out = Vec::new();
    for row in &wrapped[..head_budget] {
        out.push(format!("▒ {row}"));
    }
    if hidden > 0 {
        out.push(format!("▲ {}", Fold::rows(hidden, recovery)));
    }
    for (i, row) in wrapped[tail_start..].iter().enumerate() {
        let glyph = if i + 1 == tail_budget { '▓' } else { '▒' };
        out.push(format!("{glyph} {row}"));
    }
    out.push("…".to_string());
    out
}

/// Whether `output` spills past a `view`-row budget at `columns` — the SAME
/// wrapped-rows accounting [`spill_view_lines`] spends (#1433), so the
/// collapse decision and the excerpt truncation can never disagree. (Review
/// fix on #1663: the collapse previously counted LOGICAL lines, so a result
/// of a few heavily-wrapped lines was truncated by the excerpt path yet
/// refused to collapse in summary mode.) `view == 0` never spills (unbounded).
pub(crate) fn spills_past(output: &str, view: usize, columns: usize) -> bool {
    if view == 0 {
        return false;
    }
    let content_width = columns.saturating_sub(2).max(1);
    let mut used = 0usize;
    for l in output.lines() {
        used += wrap_to_width(l, content_width).len().max(1);
        if used > view {
            return true;
        }
    }
    false
}

/// #1640 Layer 1 (meta-scroller): the ONE-LINE collapse of a committed tool
/// result — `▲ {n} lines · {tail} · {SPILL_RECOVERY_HINT}` — used in summary
/// mode when the output spills past the `view` budget, measured in WRAPPED
/// rows via [`spills_past`] (the excerpt path's own accounting). Returns
/// `None` when the output fits or `view` is 0 (unbounded): those keep the
/// normal render, so a short result never collapses into pointless
/// indirection and `/spill 0` still means "show everything".
///
/// The tail (the last non-empty line — where errors and results live) is
/// truncated so the whole marker stays within `columns`; it is dropped
/// entirely when fewer than 8 columns remain for it. Interpolates
/// Uses [`SPILL_RECOVERY_HINT`] when no retained result ID is available; Rich
/// renderers replace it with the exact `/spill open <id>` recovery command.
pub(crate) fn spill_summary_line(output: &str, view: usize, columns: usize) -> Option<String> {
    spill_summary_line_with_hint(output, view, columns, Recovery::default())
}

fn spill_summary_line_with_hint(
    output: &str,
    view: usize,
    columns: usize,
    recovery: Recovery<'_>,
) -> Option<String> {
    if !spills_past(output, view, columns) {
        return None;
    }
    let total = output.lines().count();
    let tail = output
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let head = format!("▲ {}", Fold::lines(total, recovery));
    // Space left for the tail, in chars (the excerpt path also emits unwrapped
    // text and lets the terminal soft-wrap; here we just keep the marker
    // visually one row in the common case). The 4 covers the " · " separator
    // and a possible `…` cut marker.
    let avail = columns.saturating_sub(head.chars().count() + 4);
    let tail_len = tail.chars().count();
    // Drop the tail only when the row can't fit a MEANINGFUL piece of it —
    // a tail that fits outright is always shown, however short.
    if avail < 8 && avail < tail_len {
        return Some(head);
    }
    let shown: String = tail.chars().take(avail).collect();
    let ellipsis = if shown.chars().count() < tail_len {
        "…"
    } else {
        ""
    };
    Some(format!("{head} · {shown}{ellipsis}"))
}

/// Injected writer for one tool's operator-facing audit block. Production uses
/// stdout; tests use a `Vec<u8>` so dispatcher routing can be verified without
/// process-wide fd redirection.
pub(crate) struct ToolDisplay<W: Write> {
    writer: W,
    color: bool,
    cols: usize,
    spill_lines: usize,
    /// #1640 Layer 1: collapse a spilled result to a one-line summary marker
    /// instead of the multi-row excerpt (rich surface; keeps the conversation
    /// spine dominant). A required constructor parameter — not read from the
    /// global here — so a call site cannot silently get the wrong mode.
    summary: bool,
    result_override: Option<String>,
    /// Optional completed spill renderer for Rich TUI interactive viewport (#1640).
    /// When present, completed tool output ADDITIONALLY renders as an interactive
    /// spill viewport below the committed `spill_view_lines` excerpt — the excerpt
    /// stays the canonical transcript record on every tier.
    completed_spill_renderer: Option<std::sync::Arc<dyn crate::agentic::CompletedSpillRenderer>>,
}

impl<W: Write> ToolDisplay<W> {
    pub(crate) fn new(
        writer: W,
        color: bool,
        cols: usize,
        spill_lines: usize,
        summary: bool,
    ) -> Self {
        Self {
            writer,
            color,
            cols,
            spill_lines,
            summary,
            result_override: None,
            completed_spill_renderer: None,
        }
    }

    /// Set the completed spill renderer for Rich TUI interactive viewport.
    pub(crate) fn set_completed_spill_renderer(
        &mut self,
        renderer: std::sync::Arc<dyn crate::agentic::CompletedSpillRenderer>,
    ) {
        self.completed_spill_renderer = Some(renderer);
    }

    /// Drop the renderer for the rest of this display's life — the cancel
    /// teardown path, where painting a new interactive viewport would strand
    /// a dead frame past every dismiss hook.
    pub(crate) fn drop_completed_spill_renderer(&mut self) {
        self.completed_spill_renderer = None;
    }

    pub(crate) fn call(&mut self, name: &str, detail: &str) {
        // A completed-spill viewport from the PREVIOUS tool must come down
        // before this header lands below it — its erase rewinds relative to
        // the cursor, and a committed line underneath breaks that math. The
        // excerpt already committed above the frame is the durable record.
        if let Some(renderer) = &self.completed_spill_renderer {
            renderer.erase();
        }
        let lines = tool_call_lines(name, detail, self.cols);
        for (i, line) in lines.iter().enumerate() {
            if self.color {
                if i == 0 {
                    let prefix = format!("⚙  {name}");
                    let suffix = line.strip_prefix(&prefix).unwrap_or(line);
                    execute!(
                        &mut self.writer,
                        SetForegroundColor(NEWT_ORANGE_CT),
                        Print(prefix),
                        ResetColor,
                        SetForegroundColor(CtColor::DarkGrey),
                        Print(suffix),
                        ResetColor,
                        Print("\n"),
                    )
                    .ok();
                } else {
                    execute!(
                        &mut self.writer,
                        SetForegroundColor(CtColor::DarkGrey),
                        Print(line),
                        ResetColor,
                        Print("\n"),
                    )
                    .ok();
                }
            } else {
                writeln!(&mut self.writer, "{line}").ok();
            }
        }
        self.writer.flush().ok();
    }

    pub(crate) fn result(&mut self, output: &str) {
        let overridden = self.result_override.take();
        let output = overridden.as_deref().unwrap_or(output);
        let output = if output.trim().is_empty() {
            "(no output)"
        } else {
            output
        };
        let retained_id = self
            .completed_spill_renderer
            .as_ref()
            .and_then(|renderer| renderer.retain_completed(output));
        // Just the command — it lands inside the fold marker's `[...]`, which
        // already frames it as the handle to reach for.
        let recovery_hint = retained_id.map(|id| format!("/spill open {id}"));

        // The static excerpt is ALWAYS committed first — it is the canonical
        // transcript record on every tier, and it must never depend on an
        // ephemeral viewport that is erased moments later.
        //
        // #1640 Layer 1: in summary mode a SPILLED result commits as a single
        // collapse marker instead of the multi-row excerpt, so the
        // conversation spine (green prompts/replies) stays dominant. A result
        // that fits the budget, and `/spill 0` (unbounded), keep the normal
        // render — `spill_summary_line` returns `None` for both.
        let rendered = if let Some(recovery_hint) = recovery_hint.as_deref() {
            // Committed scrollback outlives the viewport, so this text may only
            // ever name a COMMAND — never `space`, which would stop being true
            // the moment the next canonical write dismisses the frame.
            let recovery = Recovery::Command(recovery_hint);
            self.summary
                .then(|| {
                    spill_summary_line_with_hint(output, self.spill_lines, self.cols, recovery)
                })
                .flatten()
                .unwrap_or_else(|| {
                    spill_view_lines_with_hint(output, self.spill_lines, self.cols, recovery)
                        .join("\n")
                })
        } else {
            self.summary
                .then(|| spill_summary_line(output, self.spill_lines, self.cols))
                .flatten()
                .unwrap_or_else(|| spill_view_lines(output, self.spill_lines, self.cols).join("\n"))
        };
        if self.color {
            execute!(
                &mut self.writer,
                SetForegroundColor(CtColor::DarkGrey),
                Print(format!("{rendered}\n")),
                ResetColor,
            )
            .ok();
        } else {
            writeln!(&mut self.writer, "{rendered}").ok();
        }
        self.writer.flush().ok();

        // Rich TUI (#1640): additionally paint an interactive viewport BELOW
        // the committed excerpt — scrollable/expandable until the turn's next
        // canonical write dismisses it (round dispatch, or the next tool
        // header via `call`). Its erase is a pure rewind: the excerpt above
        // is the durable record, so dismissal loses nothing. The flush above
        // guarantees the excerpt's bytes reach the terminal before the frame
        // paints below them.
        if let Some(renderer) = &self.completed_spill_renderer {
            let _ = renderer.render_completed(output, self.cols, self.spill_lines);
        }
    }

    #[cfg(test)]
    pub(crate) fn into_inner(self) -> W {
        self.writer
    }
}

/// Non-final presentation events available to the execution layer. Header and
/// completed-result rendering are intentionally absent: the outer dispatcher
/// owns those exactly once for every return path.
pub(crate) trait ToolPresentation: Send {
    fn preview(&mut self, output: &str, max_lines: usize);
    fn document(&mut self, output: &str);
    fn override_result(&mut self, output: String);
}

impl<W: Write + Send> ToolPresentation for ToolDisplay<W> {
    fn preview(&mut self, output: &str, max_lines: usize) {
        let lines: Vec<&str> = output.lines().collect();
        let shown = if max_lines == 0 {
            lines.len()
        } else {
            lines.len().min(max_lines)
        };
        let mut rendered = lines[..shown].join("\n");
        let hidden = lines.len().saturating_sub(shown);
        if hidden > 0 {
            if !rendered.is_empty() {
                rendered.push('\n');
            }
            rendered.push_str(&format!("  … {}", Fold::lines(hidden, Recovery::default())));
        }
        if rendered.is_empty() {
            return;
        }
        if self.color {
            execute!(
                &mut self.writer,
                SetForegroundColor(CtColor::DarkGrey),
                Print(format!("{rendered}\n")),
                ResetColor,
            )
            .ok();
        } else {
            writeln!(&mut self.writer, "{rendered}").ok();
        }
        self.writer.flush().ok();
    }

    fn document(&mut self, output: &str) {
        writeln!(&mut self.writer, "{output}").ok();
        self.writer.flush().ok();
    }

    fn override_result(&mut self, output: String) {
        self.result_override = Some(output);
    }
}

/// Print a tool-call header so the user can see what the agent is doing.
#[cfg(test)]
pub(crate) fn print_tool_call(name: &str, detail: &str, color: bool) {
    ToolDisplay::new(
        io::stdout(),
        color,
        term_cols(),
        spill_lines(),
        spill_summary(),
    )
    .call(name, detail);
}

/// Print completed tool output using the universal #1235 spill height. The
/// legacy `tool_output_lines` argument remains in compatibility signatures but
/// no longer overrides `[tui].spill_lines`.
#[cfg(test)]
pub(crate) fn print_tool_output(output: &str, _tool_output_lines: usize, color: bool) {
    ToolDisplay::new(
        io::stdout(),
        color,
        term_cols(),
        spill_lines(),
        spill_summary(),
    )
    .result(output);
}

#[cfg(test)]
mod tests {
    use super::super::observability::ErrorClass;
    use super::{
        fmt_tokens, print_harness_notice, print_list_item, print_newt, retry_indicator_text,
        spill_summary_line, spill_view_lines, tool_call_lines, wrap_to_width, SPILL_RECOVERY_HINT,
    };

    #[test]
    fn retry_indicator_names_timeout_and_retry_budget() {
        assert_eq!(
            retry_indicator_text(
                1,
                1,
                std::time::Duration::from_secs(2),
                Some(ErrorClass::Timeout),
            ),
            "  ↻ request timed out — retrying in 2.0s (retry 1/1)…"
        );
    }

    #[test]
    fn retry_indicator_distinguishes_transport_from_model_errors() {
        assert!(retry_indicator_text(
            2,
            4,
            std::time::Duration::from_millis(750),
            Some(ErrorClass::Transport),
        )
        .contains("connection lost"));
        assert!(retry_indicator_text(
            2,
            4,
            std::time::Duration::from_millis(750),
            Some(ErrorClass::Model),
        )
        .contains("backend returned a retryable error"));
    }

    /// #1433: the excerpt is capped by LOGICAL lines, so its "N rows" promise
    /// only holds when the output happens to be narrow. One long line — a
    /// compiler diagnostic, a minified JSON blob, a base64 payload — autowraps
    /// past the budget and floods the scrollback.
    ///
    /// codex names this rule explicitly (`exec_cell/render.rs`): "Wrap first so
    /// that truncation is applied to on-screen lines rather than logical lines.
    /// This ensures that a small number of very long lines cannot flood the
    /// viewport."
    ///
    /// Note the asymmetry this closes: the LIVE viewport already clips to width
    /// (`docs/decisions/live_spill_viewport.md` §3). Only the committed excerpt
    /// did not — so the same output was bounded while running and unbounded
    /// once finished, which is backwards from what an operator expects.
    #[test]
    fn one_long_line_cannot_flood_the_row_budget() {
        const COLS: usize = 80;
        let long = "x".repeat(400);
        let out = spill_view_lines(&long, 3, COLS);

        let rendered_rows: usize = out.iter().map(|l| wrap_to_width(l, COLS).len()).sum();
        // header + 3 body rows + the trailing ellipsis
        assert!(
            rendered_rows <= 5,
            "a single {}-char line rendered {rendered_rows} rows at {COLS} columns \
             against a 3-row budget:\n{out:#?}",
            long.len()
        );
    }

    /// #1433: the budget is measured in rendered rows, but the TEXT is not
    /// rewrapped. This excerpt is the canonical committed block, and
    /// `plain_scroller_tui.md` names scrollback as "searchable, copy-pasteable,
    /// and capturable with script/asciinema" — hard-wrapping would insert a
    /// newline and a gutter mid-sentence, which looks identical to the
    /// terminal's own soft-wrap but breaks copy-paste and breaks search across
    /// the wrap point.
    ///
    /// This is the regression that first showed up as
    /// `artifact_read_central_display_never_echoes_recovered_body` failing: a
    /// 103-character line split "…44 of 44 body / characters…" and a `contains`
    /// assertion stopped matching. The assertion was right and the wrap was
    /// wrong.
    #[test]
    fn a_line_wider_than_the_terminal_is_measured_but_not_rewrapped() {
        const COLS: usize = 40;
        let wide = "artifact:abc: returned 44 of 44 body characters at offset 0 (complete)";
        assert!(wide.len() > COLS, "the fixture must exceed the width");

        let out = spill_view_lines(&format!("first\n{wide}"), 3, COLS);
        let joined = out.join("\n");
        assert!(
            joined.contains("returned 44 of 44 body characters"),
            "the phrase was split across a wrap — copy-paste and search are \
             broken by rewrapping the canonical block:\n{joined}"
        );
    }

    /// #1433: measuring in rows must still SPEND the budget in rows — a wide
    /// line costs what it actually occupies, so fewer lines are shown, not more
    /// rows than asked for.
    ///
    /// **#1973 declared amendment: this golden MOVED.** Pre-#1973 the wide
    /// line alone consumed the whole 3-row budget, leaving zero room for
    /// `a`/`b`/`c` — tail-only by construction never shows anything before
    /// the tail item it kept. Post-#1973 the budget is split head+tail
    /// (content_budget=2 → head=1/tail=1), so `a` (the head) now shows
    /// alongside the wide line (the tail) — TWO body rows, not one. The
    /// property this test still proves is unchanged and is what it is
    /// actually named for: the wide line's row cost is still measured
    /// correctly, so `b`/`c` are still excluded (only `a` fits the 1-row
    /// head budget).
    #[test]
    fn a_wide_line_spends_the_row_budget_it_actually_occupies() {
        const COLS: usize = 20;
        // ~3 rows at width 18 (20 minus the "▒ " gutter).
        let wide = "w".repeat(50);
        let out = spill_view_lines(&format!("a\nb\nc\n{wide}"), 3, COLS);

        let body: Vec<&String> = out
            .iter()
            .filter(|l| l.starts_with('▒') || l.starts_with('▓'))
            .collect();
        assert_eq!(
            body,
            vec!["▒ a", &format!("▓ {wide}")],
            "the wide line's 3-row cost must still exclude b/c — only the \
             1-row head (a) and the wide tail fit the split budget:\n{out:#?}"
        );
        assert!(out.iter().any(|l| l.contains("lines hidden")), "{out:#?}");
        assert!(
            out.iter().any(|l| l.contains(SPILL_RECOVERY_HINT)),
            "every truncation marker names the way out:\n{out:#?}"
        );
    }

    /// #1235/#1973: the spill view shows BOTH ends with the issue's gutter
    /// glyphs — small outputs show whole (▒ gutter + … end marker), overflow
    /// shows a head and a tail with the ▲ hidden-count boundary between them
    /// and the ▓ thumb on the true tail line. view=0 = unbounded raw
    /// (historical behavior).
    ///
    /// **#1973 declared amendment: the overflow golden MOVED**, from
    /// tail-only (`l3,l4,l5`) to head+tail (`l1` .. `l5`) — see the module
    /// doc on [`spill_view_lines`] for why tail-only is a defect, not a
    /// style choice.
    #[test]
    fn spill_view_shows_both_ends_with_gutter_glyphs() {
        // Fits: whole output, ▒ gutter, end marker — unchanged by #1973.
        let small = spill_view_lines("a\nb\nc", 3, 80);
        assert_eq!(small, vec!["▒ a", "▒ b", "▒ c", "…"]);

        // Overflows: a head AND a tail (#1973 — neither end is fully
        // hidden), ▲ carries the hidden count, ▓ thumbs the true tail.
        let big = spill_view_lines("l1\nl2\nl3\nl4\nl5", 3, 80);
        assert_eq!(
            big,
            vec![
                "▒ l1",
                "▲ 3 lines hidden  [/spill N raises this view]",
                "▓ l5",
                "…"
            ]
        );

        // Unbounded: raw lines, no gutter.
        assert_eq!(spill_view_lines("x\ny", 0, 80), vec!["x", "y"]);
        // Empty: nothing.
        assert!(spill_view_lines("", 3, 80).is_empty());
    }

    /// **#1973 declared amendment: this golden MOVED** — same head+tail
    /// reasoning as `spill_view_shows_both_ends_with_gutter_glyphs`.
    #[test]
    fn completed_tool_output_uses_the_spill_view() {
        let output = "l1\nl2\nl3\nl4\nl5";

        assert_eq!(
            spill_view_lines(output, 3, 80),
            vec![
                "▒ l1",
                "▲ 3 lines hidden  [/spill N raises this view]",
                "▓ l5",
                "…"
            ]
        );
        let raw: Vec<String> = output.lines().map(str::to_string).collect();
        assert_eq!(spill_view_lines(output, 0, 80), raw);
    }

    /// #1263: the COMPLETED excerpt names its real recovery path at the point
    /// of use — it is plain printed text sharing the live viewport's glyphs, so
    /// without the hint it masqueraded as the interactive scroller (the
    /// diagnosed operator tried to expand it in scrollback and could not).
    ///
    /// **#1973 declared amendment:** the hint no longer lives at a fixed
    /// index — with a head shown before it, the boundary marker (and its
    /// hint) sits wherever the head ends, not always at `lines[0]`. The
    /// property this test proves — the hint appears somewhere, exactly
    /// once, whenever truncation occurs — is unchanged.
    #[test]
    fn completed_excerpt_names_its_recovery_path() {
        let lines = spill_view_lines("l1\nl2\nl3\nl4\nl5", 3, 80);
        let hint_lines: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("/spill N raises this view"))
            .collect();
        assert_eq!(
            hint_lines.len(),
            1,
            "the boundary marker must carry the recovery hint exactly once: {lines:?}"
        );
        // #1263 fingerprint pin (the other half lives in the spill_view tests):
        // the completed excerpt's last row is the INERT `…` — never the live
        // frame's ⧉/▣ boundary.
        assert_eq!(lines.last().map(String::as_str), Some("…"));
        // The fits-entirely form is inert-terminated too.
        let small = spill_view_lines("a\nb", 3, 80);
        assert_eq!(small.last().map(String::as_str), Some("…"));
    }

    /// #1973 anti-vacuous pair, replaying the incident's own shape: a script
    /// prints real results, then something unrelated in cleanup crashes.
    /// Tail-only showed ONLY the traceback — the confirming responses were
    /// entirely inside the hidden middle. Both the LAST result line and the
    /// TRACEBACK'S OWN HEAD must be visible in one default render.
    ///
    /// `view=8` (not the bare `[tui] spill_lines` default of 3) is chosen
    /// deliberately and stated so, not smuggled in: no fold of a 3-row
    /// budget can show 2 result lines AND any part of a 12-line traceback —
    /// there is not enough room at any split, head+tail or otherwise. 8 is
    /// what "the default view" (as the issue frames it, contrasted with an
    /// operator's `/spill N` AFTER already reading a misleading render)
    /// looks like for a moderately verbose tool result; the property under
    /// test is the SPLIT's behavior, not a specific numeric default.
    #[test]
    fn results_then_a_crash_shows_the_last_result_and_the_traceback_head() {
        let output = [
            "Response 0: initialize ok",
            "Response 1: tools/list ok",
            "Traceback (most recent call last):",
            "File \"cleanup.py\", line 40, in shutdown",
            "File \"cleanup.py\", line 30, in _terminate",
            "File \"cleanup.py\", line 20, in _kill",
            "File \"cleanup.py\", line 15, in _signal",
            "File \"cleanup.py\", line 10, in _reap",
            "File \"cleanup.py\", line 8, in _wait",
            "File \"cleanup.py\", line 6, in _proc",
            "File \"cleanup.py\", line 4, in _pid",
            "ProcessLookupError: [Errno 3] No such process",
        ]
        .join("\n");
        let output = output.as_str();
        let out = spill_view_lines(output, 8, 80);
        let joined = out.join("\n");

        assert!(
            joined.contains("Response 1: tools/list ok"),
            "the last result line — the confirming evidence — must not be \
             fully hidden inside the fold:\n{out:#?}"
        );
        assert!(
            joined.contains("Traceback (most recent call last):"),
            "the traceback's own head must not be fully hidden either — \
             showing only its tail (the pre-#1973 behavior) is exactly the \
             evidence-inversion this issue is about:\n{out:#?}"
        );
        // The true tail — the actual exception — is still the tail's last
        // line (▓-thumbed): the fix ADDS a head, it does not sacrifice the
        // tail cargo-style output already depended on.
        assert!(
            out.last().is_some_and(|l| l == "…"),
            "inert-terminated as always:\n{out:#?}"
        );
        assert!(
            joined.contains("ProcessLookupError"),
            "the actual exception must still be visible — this is not a \
             head-only regression of the tail:\n{out:#?}"
        );
    }

    /// #1973 anti-vacuous TWIN: cargo-style output (the decisive content
    /// really is last) must not regress. Same `view` as the sibling test
    /// above for a fair comparison.
    #[test]
    fn cargo_style_output_with_errors_last_still_shows_the_errors() {
        let output = [
            "Compiling foo v0.1.0",
            "Compiling bar v0.1.0",
            "Compiling baz v0.1.0",
            "Compiling qux v0.1.0",
            "Compiling quux v0.1.0",
            "Compiling corge v0.1.0",
            "Compiling grault v0.1.0",
            "error[E0499]: cannot borrow `x` as mutable more than once",
            "  --> src/lib.rs:42:5",
            "error[E0502]: cannot borrow `x` as immutable",
            "  --> src/lib.rs:43:5",
            "error: could not compile `foo` (bin \"foo\") due to 2 previous errors",
        ]
        .join("\n");
        let output = output.as_str();
        let out = spill_view_lines(output, 8, 80);
        let joined = out.join("\n");

        assert!(
            joined
                .contains("error: could not compile `foo` (bin \"foo\") due to 2 previous errors"),
            "the final cargo summary line — the decisive content for THIS \
             shape — must still be visible:\n{out:#?}"
        );
        assert!(
            joined.contains("E0499") || joined.contains("E0502"),
            "at least one of the actual diagnostics should still be in the \
             tail window:\n{out:#?}"
        );
    }

    /// #1973's small-render finding, checked directly: the reserve for the
    /// boundary marker was previously 0 (an unaccounted extra row on top of
    /// `view` content rows), so a `view`-row budget rendered `view + 2`
    /// total rows (content + marker + the pre-existing trailing `…`), not
    /// `view`. Reserving 1 row for the marker tightens this to `view + 1` —
    /// the one row still unaccounted for is the trailing `…`, which predates
    /// #1973 and is not part of "the marker's own height".
    #[test]
    fn the_marker_reserve_is_tightened_by_its_own_height() {
        for view in [2usize, 3, 5, 8, 12] {
            // Enough lines to guarantee overflow at every tested view.
            let lines: Vec<String> = (0..view + 20).map(|i| format!("l{i}")).collect();
            let out = spill_view_lines(&lines.join("\n"), view, 80);
            assert!(
                out.len() <= view + 1,
                "view={view}: rendered {} total rows, expected at most view+1 \
                 ({}) — the marker's reserve regressed:\n{out:#?}",
                out.len(),
                view + 1
            );
        }
    }

    /// #1640 Layer 1: a spilled result collapses to ONE line in summary mode —
    /// total count, the tail (where errors live), and the #1433 recovery hint.
    /// Fitting results and `/spill 0` (unbounded) return `None` (normal render).
    #[test]
    fn summary_line_collapses_only_spilled_results() {
        // Spilled: one line with count + tail + hint.
        let line = spill_summary_line("l1\nl2\nl3\nl4\nl5", 3, 80).expect("5 > 3 collapses");
        assert_eq!(line, "▲ 5 lines hidden  [/spill N raises this view] · l5");

        // Tail skips trailing blank lines — the last NON-EMPTY line informs.
        let line = spill_summary_line("l1\nl2\nl3\nerror: boom\n\n", 3, 80).unwrap();
        assert!(line.contains("error: boom"), "{line}");

        // Fits the budget → None (normal render, no pointless indirection).
        assert_eq!(spill_summary_line("a\nb\nc", 3, 80), None);
        // `/spill 0` = unbounded → None (full text always wins).
        assert_eq!(spill_summary_line("a\nb\nc\nd\ne", 0, 80), None);
    }

    /// The one-line promise holds on narrow terminals: the tail is truncated
    /// (with `…`) to keep the marker within the column budget, and dropped
    /// entirely when almost no room remains — but the hint always survives.
    #[test]
    fn summary_line_fits_narrow_terminals() {
        let wide = format!("l1\nl2\nl3\n{}", "x".repeat(300));
        let line = spill_summary_line(&wide, 3, 60).unwrap();
        assert!(
            line.chars().count() <= 60,
            "one visual row on an 60-col terminal: {} chars",
            line.chars().count()
        );
        assert!(line.contains('…'), "a cut tail is marked: {line}");
        assert!(line.contains("/spill N raises this view"), "{line}");

        // Pathologically narrow: tail dropped, count + hint intact.
        let line = spill_summary_line(&wide, 3, 20).unwrap();
        assert_eq!(line, "▲ 4 lines hidden  [/spill N raises this view]");
    }

    /// `ToolDisplay` in summary mode commits the collapse marker INSTEAD of the
    /// excerpt for a spilled result — and keeps the excerpt for a fitting one.
    /// (Excerpt mode `false` is pinned by every other test in this module.)
    #[test]
    fn summary_mode_commits_the_marker_not_the_excerpt() {
        let mut display = super::ToolDisplay::new(Vec::new(), false, 80, 3, true);
        display.result("l1\nl2\nl3\nl4\nl5");
        let out = String::from_utf8(display.writer).unwrap();
        assert!(
            out.contains("▲ 5 lines hidden  [/spill N raises this view] · l5"),
            "the marker committed: {out:?}"
        );
        assert!(
            !out.contains("▒ l3"),
            "no excerpt rows in summary mode: {out:?}"
        );

        // A fitting result renders exactly as excerpt mode would.
        let mut display = super::ToolDisplay::new(Vec::new(), false, 80, 3, true);
        display.result("a\nb");
        let out = String::from_utf8(display.writer).unwrap();
        assert!(
            out.contains("▒ a"),
            "fitting results keep the full render: {out:?}"
        );
    }

    #[test]
    fn tool_call_lines_wrap_without_losing_the_command() {
        assert_eq!(
            tool_call_lines("find", ". (name=*.rs, type=f)", 80),
            vec!["⚙  find: . (name=*.rs, type=f)"]
        );
    }

    #[test]
    fn wrap_to_width_never_drops_and_hard_splits_long_tokens() {
        // #1153: the full command must survive — reassembling the wrap yields
        // the original (spaces preserved via split_inclusive).
        let cmd = "grep -rn \"NudgerProfile|resolve.*knob|KNOWN_KNOBS\" --include=*.rs newt-core/src newt-cli/src";
        let wrapped = super::wrap_to_width(cmd, 20);
        assert_eq!(wrapped.join(""), cmd, "no characters lost");
        assert!(
            wrapped.iter().all(|l| l.chars().count() <= 20),
            "each line fits"
        );
        assert!(wrapped.len() > 1, "long command actually wraps");

        // A single token longer than the width is hard-split, not dropped.
        let long = "a".repeat(50);
        let w = super::wrap_to_width(&long, 10);
        assert_eq!(w.join(""), long);
        assert_eq!(w.len(), 5);

        // Short input stays one line.
        assert_eq!(super::wrap_to_width("cargo build", 40), vec!["cargo build"]);
        // Embedded newlines split into separate logical lines.
        assert_eq!(super::wrap_to_width("a\nb", 40), vec!["a", "b"]);
    }

    #[test]
    fn fmt_tokens_inserts_thousands_separators() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1,000");
        assert_eq!(fmt_tokens(1_234_567), "1,234,567");
    }

    #[test]
    fn gauge_formatting_k_compact_fraction_and_level() {
        use super::{fmt_token_gauge, fmt_tokens_compact, fmt_tokens_k, gauge_level, GaugeLevel};
        // k (rounded thousands)
        assert_eq!(fmt_tokens_k(899_000), "899k");
        assert_eq!(fmt_tokens_k(1_024_000), "1024k");
        assert_eq!(fmt_tokens_k(512_400), "512k");
        // compact: 1M = 1024k; round M drops the .0
        assert_eq!(fmt_tokens_compact(899_000), "899k");
        assert_eq!(fmt_tokens_compact(1_024_000), "1M");
        assert_eq!(fmt_tokens_compact(2_048_000), "2M");
        assert_eq!(fmt_tokens_compact(1_536_000), "1.5M");
        // fraction
        assert_eq!(fmt_token_gauge(899_000, 1_024_000), "899k/1024k");
        // level bands: <75 Ok, 75–90 Warn, ≥90 Critical
        assert_eq!(gauge_level(100, 1000), GaugeLevel::Ok);
        assert_eq!(gauge_level(740, 1000), GaugeLevel::Ok);
        assert_eq!(gauge_level(750, 1000), GaugeLevel::Warn);
        assert_eq!(gauge_level(890, 1000), GaugeLevel::Warn);
        assert_eq!(gauge_level(900, 1000), GaugeLevel::Critical);
        assert_eq!(gauge_level(0, 0), GaugeLevel::Ok); // no budget → no panic
    }

    #[test]
    fn compression_notice_text_registers_per_outcome() {
        use super::compression_notice_text;
        use crate::agentic::compress::CompressAction;
        // The static-marker last resort is LOUD and degraded-sounding (24.7).
        let (msg, loud) =
            compression_notice_text(CompressAction::StaticFallback, 10_000, 6_000, "");
        assert!(loud, "static marker is the loud last resort");
        assert!(msg.starts_with("⛔"), "{msg}");
        assert!(msg.contains("summary unavailable"), "{msg}");
        assert!(msg.contains("Re-read files"), "{msg}");
        // Success and prune are calm, distinct glyphs.
        let (msg, loud) = compression_notice_text(CompressAction::Summarized, 10_000, 6_000, "");
        assert!(!loud);
        assert!(msg.starts_with("✓") && msg.contains("summarized"), "{msg}");
        let (msg, loud) = compression_notice_text(CompressAction::Pruned, 10_000, 6_000, "");
        assert!(!loud);
        assert!(
            msg.starts_with("⧉") && msg.contains("structural prune"),
            "{msg}"
        );
        // The over-budget suffix rides along.
        let (msg, _) = compression_notice_text(
            CompressAction::Summarized,
            10_000,
            6_000,
            ", still over budget",
        );
        assert!(msg.contains(", still over budget"), "{msg}");
    }

    /// `docs/decisions/tty_widget_suite.md` §5 row 3: `Notice` must reproduce
    /// this builder's bytes **exactly** before any call site is migrated onto
    /// it. Every one of the three registers is a glyph + the two-space gutter +
    /// text, so the widget's composition is the right shape and the migration
    /// in step 5 is a deletion rather than a rewrite.
    ///
    /// Byte-for-byte, not "starts with" — the assertions above are the loose
    /// ones this deliberately is not.
    #[test]
    fn notice_reproduces_the_compression_notice_bytes() {
        use super::compression_notice_text;
        use crate::agentic::compress::CompressAction;
        use crate::tty::{Level, Notice};

        let cases = [
            (CompressAction::StaticFallback, Level::Loud, "⛔"),
            (CompressAction::Summarized, Level::Ok, "✓"),
            (CompressAction::Pruned, Level::Info, "⧉"),
        ];
        for (action, level, glyph) in cases {
            let (msg, _) = compression_notice_text(action, 10_000, 6_000, "");
            let body = msg
                .strip_prefix(glyph)
                .and_then(|r| r.strip_prefix("  "))
                .unwrap_or_else(|| panic!("{action:?} is not `{glyph}` + two spaces: {msg:?}"));
            assert_eq!(
                Notice::new(level, glyph, body).gap(2).line(),
                msg,
                "Notice must reproduce {action:?}'s bytes exactly"
            );
        }
    }

    /// Visual preview for UX review (run with `--ignored --nocapture`): the three
    /// compression-notice registers with their colors.
    #[test]
    #[ignore = "visual preview; run with --ignored --nocapture"]
    fn compression_notice_visual_preview() {
        use super::compression_notice_text;
        use crate::agentic::compress::CompressAction;
        let paint = |loud: bool, s: &str| {
            if loud {
                format!("\x1b[31m{s}\x1b[0m") // red, loud
            } else {
                format!("\x1b[33m{s}\x1b[0m") // amber
            }
        };
        println!("\n  compression-notice registers (24.7):");
        for action in [
            CompressAction::Pruned,
            CompressAction::Summarized,
            CompressAction::StaticFallback,
        ] {
            let (msg, loud) = compression_notice_text(action, 1_024_000, 600_000, "");
            println!("    {}", paint(loud, &msg));
        }
        println!();
    }

    /// Visual preview for UX review (run with `--nocapture`). Not an assertion —
    /// prints the gauge at several fills with colors so the format/thresholds
    /// can be eyeballed. Ignored by default so it never adds CI noise.
    #[test]
    #[ignore = "visual preview; run with --ignored --nocapture"]
    fn gauge_visual_preview() {
        use super::{fmt_token_gauge, fmt_tokens_compact, gauge_level, GaugeLevel};
        use crossterm::style::Color;
        let color = |lvl: GaugeLevel| match lvl {
            GaugeLevel::Ok => Color::Green,
            GaugeLevel::Warn => Color::DarkYellow,
            GaugeLevel::Critical => Color::Red,
        };
        let paint = |c: Color, s: &str| match c {
            Color::Green => format!("\x1b[32m{s}\x1b[0m"),
            Color::DarkYellow => format!("\x1b[33m{s}\x1b[0m"),
            Color::Red => format!("\x1b[31m{s}\x1b[0m"),
            _ => s.to_string(),
        };
        let budget = 1_024_000;
        println!("\n  context-budget gauge — fraction form (live header):");
        for used in [102_000u32, 512_000, 800_000, 972_000, 1_010_000] {
            let lvl = gauge_level(used, budget);
            let g = fmt_token_gauge(used, budget);
            println!("    {:<14} {:?}", paint(color(lvl), &g), lvl);
        }
        println!("\n  compact budget form (1M = 1024k):");
        for n in [899_000u32, 1_024_000, 1_536_000, 2_048_000] {
            println!("    {n:>9} → {}", fmt_tokens_compact(n));
        }
        println!(
            "\n  mock header:\n    [2026-06-22 14:32:01] vi --INSERT-- nemotron @ REDACTED-HOST   {}\n",
            paint(color(gauge_level(972_000, budget)), &fmt_token_gauge(972_000, budget)),
        );
    }

    /// The narrator + list-item printers write to stdout (hard to capture here),
    /// so this just exercises every branch — color/no-color × active/inactive ×
    /// verbose — to keep them from rotting and to cover them for the gate.
    #[test]
    fn printers_cover_every_branch_without_panicking() {
        for color in [true, false] {
            for verbose in [true, false] {
                print_newt("narrator line", color, verbose);
            }
            print_list_item("name · ollama · model @ url", true, color);
            print_list_item("name · ollama · model @ url", false, color);
            print_harness_notice(
                "over budget — dispatching and letting the backend decide",
                color,
            );
        }
    }

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

    /// #1663 review F4: the collapse predicate spends WRAPPED rows exactly like
    /// the excerpt path (#1433) — a result of few logical lines but heavy
    /// wrapping collapses in summary mode instead of falling back to a
    /// truncated excerpt.
    #[test]
    fn collapse_uses_wrapped_row_accounting_like_the_excerpt() {
        // 2 logical lines, but the first wraps to many rows at 20 columns.
        let long = format!("{}\nshort tail\n", "x".repeat(200));
        assert!(super::spills_past(&long, 3, 20), "wrapped rows spill");
        assert!(
            super::spill_summary_line(&long, 3, 20).is_some(),
            "summary engages on wrapped spill (logical-line count would say no)"
        );
        // Parity with the excerpt: what the excerpt truncates, summary collapses.
        let excerpt = super::spill_view_lines(&long, 3, 20);
        assert!(
            excerpt[0].starts_with('▲'),
            "excerpt path truncates the same input: {excerpt:?}"
        );
        // And a genuinely fitting result engages neither.
        let fits = "a\nb\n";
        assert!(!super::spills_past(fits, 3, 80));
        assert!(super::spill_summary_line(fits, 3, 80).is_none());
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
}
