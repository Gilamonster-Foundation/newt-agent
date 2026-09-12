//! Terminal output helpers for the agentic loop.
//!
//! Moved verbatim from `newt-tui` in Step 9.7 so the loop and the inline
//! progress it prints (tool calls, retries, trim notices) stay together.
//! Includes pure formatting and cadence helpers as well as stdout emission.
//! Headless callers (Step 9.8's ACP worker) run with `color: false` and
//! capture/ignore the stream.

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
    write_harness_notice(io::stdout(), msg, color);
}

/// [`print_harness_notice`] onto a caller-supplied sink.
///
/// Same bytes, one implementation. The streamed-answer path (#123) takes its
/// output sink as a parameter so a test can assert what actually reached the
/// terminal, and its cut-stream notice belongs on that same sink — a notice
/// nothing can observe is a print site that can be deleted with the suite
/// staying green.
pub fn write_harness_notice(mut out: impl Write, msg: &str, color: bool) {
    if color {
        execute!(
            out,
            SetForegroundColor(CtColor::DarkYellow),
            Print(format!("⚠  newt: {msg}\n")),
            ResetColor,
        )
        .ok();
    } else {
        writeln!(out, "⚠  newt: {msg}").ok();
    }
    out.flush().ok();
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
        Some(ErrorClass::ContextExceeded) => "backend rejected the context size",
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

/// Which interval boundary `elapsed` falls in — the ONE statement of "how
/// often is often enough", shared by the turn heartbeat and the transcript's
/// time markers.
///
/// Integer division on purpose: it makes the cadence **non-catch-up**. A turn
/// that blocks for an hour inside one tool call crosses one boundary, not
/// twelve, so the caller emits a single line when it returns rather than a wall
/// of them at exactly the moment the operator is trying to read what happened.
///
/// Pure — it reads no clock. Production hands it an `elapsed`; a test states
/// the time. Two callers keep their own "have I announced this boundary yet"
/// because their lifetimes differ (one per turn, one per process), but neither
/// gets to have its own opinion about where the boundaries are.
pub(crate) fn cadence_boundary(
    elapsed: std::time::Duration,
    interval: std::time::Duration,
) -> Option<u64> {
    if interval.is_zero() {
        return None;
    }
    Some(elapsed.as_secs() / interval.as_secs().max(1))
}

/// How much wall-clock passes between turn heartbeats (#1965).
///
/// Five minutes: long enough that an ordinary turn emits none at all, short
/// enough that a turn on its way to 1954 seconds says something four times
/// before it lands. The evidenced 32-minute turn emitted no intermediate
/// signal of any kind — `mod.rs` has per-tool `Instant` timers and nothing
/// that watches the turn as a whole.
const TURN_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether a turn heartbeat is due, and the bookkeeping to keep them bounded.
///
/// **Pure over a supplied `elapsed`** — it reads no clock. Production hands it
/// `turn_start.elapsed()`; a test states the time. That is the difference
/// between a test that asserts a schedule and one that sleeps, and this box
/// saturates badly enough that a wall-clock assertion here would be a flake
/// generator rather than a check.
#[derive(Debug, Default)]
pub(super) struct TurnHeartbeat {
    /// Interval boundaries already announced. Bounded by construction: it
    /// counts, so a turn of any length holds one integer.
    emitted: u64,
}

impl TurnHeartbeat {
    /// Consume a due heartbeat and build its message without reading a clock.
    pub(super) fn notice(
        &mut self,
        elapsed: std::time::Duration,
        round: usize,
        limit: usize,
    ) -> Option<String> {
        self.due(elapsed, TURN_HEARTBEAT_INTERVAL)
            .then(|| turn_heartbeat_line(elapsed, round, limit))
    }

    /// Consume the heartbeat due at `elapsed`, if one is.
    ///
    /// At most one line per interval however long the gap: a turn that blocks
    /// for an hour inside a single tool call emits ONE line when it returns,
    /// not twelve. Catching up would turn a quiet signal into a wall of text
    /// at exactly the moment the operator is trying to read what happened.
    pub(super) fn due(
        &mut self,
        elapsed: std::time::Duration,
        interval: std::time::Duration,
    ) -> bool {
        // The boundary rule is `display::cadence_boundary` — shared with the
        // transcript's time markers so the two cannot drift into two different
        // ideas of "often enough". What stays here is only this heartbeat's own
        // record of what it has already said, which is per-turn where the
        // marker's is per-process.
        match cadence_boundary(elapsed, interval) {
            Some(boundary) if boundary > self.emitted => {
                self.emitted = boundary;
                true
            }
            _ => false,
        }
    }
}

/// The heartbeat line. Pure, no ANSI, no I/O — the same split
/// `Notice::line`/`Notice::emit` uses.
///
/// It reports the two facts missing from the evidenced 32-minute turn: how long
/// it has been running, and how far into the ROUND budget it is — against the
/// EFFECTIVE cap, so an escalated turn shows "round 210 of 10000" rather than
/// looking like it has overrun a limit of 40.
pub(super) fn turn_heartbeat_line(
    elapsed: std::time::Duration,
    round: usize,
    limit: usize,
) -> String {
    let mins = elapsed.as_secs() / 60;
    format!("still working — {mins}m elapsed, round {round} of {limit}")
}

/// Seconds between committed time markers; 0 = off.
///
/// **Off by default, seeded per turn by the surface** — the same shape as
/// `SPILL_SUMMARY` above, and for the same reason. A wall clock in stdout makes
/// byte-exact capture unstable, and the piped / headless / `newt solve` paths
/// are exactly where that matters, so those keep today's output unchanged while
/// an interactive operator gets the markers.
static TIME_MARKER_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The last boundary a marker was committed for. Process-wide, because
/// `ToolDisplay` is constructed fresh for every tool call and BOTH emitters
/// (the real dispatcher and the synthetic-result path) must share one cadence
/// or they interleave into a stutter.
static TIME_MARKER_EMITTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Set the time-marker cadence, from the surface that knows whether its output
/// is a transcript a human reads or bytes something else will diff.
pub fn set_time_marker_secs(secs: u64) {
    TIME_MARKER_SECS.store(secs, std::sync::atomic::Ordering::Relaxed);
    // Seed the boundary to NOW rather than to zero. This runs once per turn,
    // and the turn echo has just committed its own `[YYYY-MM-DD HH:MM:SS]` a
    // row or two above — so resetting to zero would fire a marker on the first
    // tool call of every turn, restating a time the operator can still see.
    // What is worth marking is an interval that passes INSIDE a turn.
    let seeded = cadence_boundary(
        marker_epoch().elapsed(),
        std::time::Duration::from_secs(secs),
    )
    .unwrap_or(0);
    TIME_MARKER_EMITTED.store(seeded, std::sync::atomic::Ordering::Relaxed);
}

/// When this process started, for the elapsed the cadence is measured against.
fn marker_epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

/// Claim the marker due at `elapsed`, if one is — at most once per boundary
/// however many callers race for it.
///
/// Pure over `elapsed` and `interval`; the atomic is the only state, and the
/// compare-exchange is what makes "at most one" true rather than merely likely.
fn claim_time_marker(elapsed: std::time::Duration, interval: std::time::Duration) -> bool {
    let Some(boundary) = cadence_boundary(elapsed, interval) else {
        return false;
    };
    let mut seen = TIME_MARKER_EMITTED.load(std::sync::atomic::Ordering::Relaxed);
    while boundary > seen {
        match TIME_MARKER_EMITTED.compare_exchange_weak(
            seen,
            boundary,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => seen = actual,
        }
    }
    false
}

/// The marker itself, given an already-formatted wall clock.
///
/// `[14:32]` and not a rule, for three reasons: the bracketed stamp is already
/// this transcript's vocabulary (the turn echo commits
/// `[YYYY-MM-DD HH:MM:SS]`), brackets still read as a marker with `color:
/// false` — which is the piped case, where a bare grey `14:32` would look like
/// output — and a full-width rule is a word-wrap hazard that says nothing the
/// brackets do not. The date is already on the turn's own row, so inside a turn
/// `%H:%M` is the right register.
pub(crate) fn time_marker_line(hhmm: &str) -> String {
    format!("[{hhmm}]")
}

/// A reasoning block, bounded the way a tool result is.
///
/// The problem it solves is the one `SPILL_SUMMARY` already names for tools: a
/// wall of grey buries the conversation spine. A thinking model that reasons
/// for four hundred lines committed four hundred dim lines to scrollback, and
/// then the answer arrived somewhere below the fold of the operator's screen.
///
/// So reasoning gets the treatment tool output gets, from the same parts: the
/// first `[tui] spill_lines` rows commit as before, the rest are retained
/// instead of printed, and one closing line names how long the model thought
/// and how much is behind the fold — through [`Fold`], so it cannot become a
/// sixth phrasing, and through the archive, so `/spill open <id>` is a real
/// offer rather than a gesture.
#[derive(Debug, Default)]
pub struct ThinkingFold {
    /// Rows already committed to scrollback, against the `spill_lines` budget.
    shown: usize,
    /// Everything the model said, for the archive. Only grows past the budget
    /// — under it the line is already in scrollback and this is the copy that
    /// makes the whole block reopenable.
    body: String,
    /// Lines held back, i.e. what the fold is hiding.
    hidden: usize,
}

impl ThinkingFold {
    /// Offer one completed reasoning line. Returns whether the caller should
    /// PRINT it — false once the budget is spent, at which point the line has
    /// been retained instead.
    pub fn offer(&mut self, line: &str, budget: usize) -> bool {
        self.body.push_str(line);
        self.body.push('\n');
        // `spill_lines == 0` is "unbounded" everywhere else in this file, and
        // it means the same here: nothing is ever held back.
        if budget == 0 || self.shown < budget {
            self.shown += 1;
            return true;
        }
        self.hidden += 1;
        false
    }

    /// Everything the model said, for retention.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Whether anything was said at all.
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    /// The closing line, or `None` when the model did not reason.
    ///
    /// `Thought for 41s` alone when nothing was held back — there is no fold to
    /// announce, and offering a handle that opens what is already on screen is
    /// noise. With a fold, the count and the handle follow it.
    pub fn closing_line(
        &self,
        elapsed: std::time::Duration,
        recovery: Recovery<'_>,
    ) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let head = format!("Thought for {}", fmt_duration(elapsed));
        if self.hidden == 0 {
            return Some(head);
        }
        Some(format!("{head} · {}", Fold::lines(self.hidden, recovery)))
    }
}

/// `41s`, `2m 5s`, `1h 3m` — the register the operator reads elapsed in.
fn fmt_duration(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    match (secs / 3600, (secs % 3600) / 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
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
        // A dim wall-clock marker, at most once per cadence interval, so a long
        // transcript is navigable by time without a stamp on every row.
        //
        // AFTER the erase above and BEFORE the header: the erase rewinds
        // relative to the cursor, so a committed line underneath it breaks that
        // math — the same ordering the header itself already depends on.
        self.time_marker();
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

    /// Commit the time marker if the cadence says one is due.
    ///
    /// Writes through `self.writer`, NOT through `Notice::emit`, which returns
    /// zero bytes at `LineCaps::None` — every piped / headless run, which is
    /// the one place a transcript most needs to say when things happened. This
    /// gives the marker exactly the header's own reach and exactly the header's
    /// own protocol-mode protection.
    fn time_marker(&mut self) {
        let secs = TIME_MARKER_SECS.load(std::sync::atomic::Ordering::Relaxed);
        if !claim_time_marker(
            marker_epoch().elapsed(),
            std::time::Duration::from_secs(secs),
        ) {
            return;
        }
        let line = time_marker_line(&chrono::Local::now().format("%H:%M").to_string());
        if self.color {
            execute!(
                &mut self.writer,
                SetForegroundColor(CtColor::DarkGrey),
                Print(format!("{line}\n")),
                ResetColor,
            )
            .ok();
        } else {
            writeln!(&mut self.writer, "{line}").ok();
        }
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
#[path = "display_tests/mod.rs"]
mod display_tests;
