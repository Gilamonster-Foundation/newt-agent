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

/// The newt logo orange as a crossterm color (matches the TUI splash).
pub const NEWT_ORANGE_CT: CtColor = CtColor::Rgb {
    r: 220,
    g: 60,
    b: 20,
};

/// Dimmer-than-DarkGrey hue for the soft "fade" tail on a truncated status
/// line — the last couple of cells before the `…` dissolve toward the
/// background so the cut reads as "there's more here", not a hard chop.
pub(crate) const FADE_CT: CtColor = CtColor::Rgb {
    r: 90,
    g: 90,
    b: 90,
};

/// Current terminal width in columns. Falls back to 80 when stdout isn't a tty
/// (headless/piped) — callers only truncate single ephemeral status lines, so a
/// conservative default is harmless.
pub(crate) fn term_cols() -> usize {
    crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .max(8)
}

/// A single status/spinner line fitted to the terminal width.
///
/// When the source overflows `max_cols` it is cut to fit with a trailing `…`,
/// and the last couple of visible cells are split off into `fade` so the caller
/// can render them dimmer (the soft fade-out). When it already fits, `fade` and
/// `ellipsis` are empty and `head` is the whole string. Width is counted in
/// `char`s — good enough for the braille spinner + ASCII status text these
/// lines carry; no CJK in this path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FittedLine {
    pub head: String,
    pub fade: String,
    pub ellipsis: &'static str,
}

/// Fit `s` into `max_cols` columns (see [`FittedLine`]).
pub(crate) fn fit_line(s: &str, max_cols: usize) -> FittedLine {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_cols {
        return FittedLine {
            head: s.to_string(),
            fade: String::new(),
            ellipsis: "",
        };
    }
    // Reserve one column for the ellipsis; keep at least one visible char.
    let budget = max_cols.saturating_sub(1).max(1);
    let kept = &chars[..budget];
    let fade_n = 2.min(kept.len());
    let split = kept.len() - fade_n;
    FittedLine {
        head: kept[..split].iter().collect(),
        fade: kept[split..].iter().collect(),
        ellipsis: "…",
    }
}

/// Word-wrap `s` into lines no wider than `width` columns (#1153): the tool-call
/// display must show the FULL command/path so the operator can audit exactly
/// what ran — truncating a `grep … | grep …` with `…` hid it. Wraps on
/// whitespace when possible; a single token longer than `width` is hard-split
/// so nothing is ever dropped. Width counted in `char`s (this path is ASCII
/// commands/paths). Returns at least one line (possibly empty for empty input).
pub(crate) fn wrap_to_width(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in s.split('\n') {
        let mut cur = String::new();
        let mut cur_len = 0usize;
        for word in logical.split_inclusive(' ') {
            let wlen = word.chars().count();
            if cur_len + wlen > width && cur_len > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            // A single word wider than the line: hard-split it so nothing is lost.
            if wlen > width {
                for ch in word.chars() {
                    if cur_len == width {
                        lines.push(std::mem::take(&mut cur));
                        cur_len = 0;
                    }
                    cur.push(ch);
                    cur_len += 1;
                }
            } else {
                cur.push_str(word);
                cur_len += wlen;
            }
        }
        lines.push(cur);
    }
    lines
}

/// Print a newt narrator line.
///
/// The `▸` marker stays the **default text color**: a colored sigil on every
/// narrator line reads as noise, and the saturated logo orange is exactly the
/// hue that's hard to parse on this operator's display (accessibility note —
/// never lean on a deep saturated color for anything readable). No-color: `>`.
pub fn print_newt(msg: &str, color: bool, verbose: bool) {
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
    println!("{prefix}{msg}");
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
pub(crate) fn print_retry_indicator(attempt: u32, delay: std::time::Duration, color: bool) {
    let delay_s = delay.as_secs_f32();
    let msg = format!("  ↻ connection lost — retrying in {delay_s:.1}s (attempt {attempt})…\n");
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::Rgb {
                r: 200,
                g: 140,
                b: 0
            }),
            Print(&msg),
            ResetColor,
        )
        .ok();
    } else {
        print!("{msg}");
    }
    io::stdout().flush().ok();
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

/// #1235: the SPILL VIEW — a bounded, TAIL-biased rendering of completed
/// tool output. Pure: returns the exact lines to print (gutter glyphs
/// included) so the unit tier tests the geometry without a terminal.
///
/// Shape (per the issue sketch): when the output fits in `view` lines it is
/// shown whole with the `▒` gutter and the `…` end-of-output marker; when it
/// overflows, the LAST `view` lines are shown (the tail is where grep hits
/// and errors live), the `▲` boundary line carries the hidden count, and the
/// `▓` thumb marks the tail position. `view == 0` means unbounded (no gutter
/// — the raw historical behavior). This is the completion-time foundation;
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

pub(crate) fn spill_view_lines(output: &str, view: usize) -> Vec<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    if view == 0 {
        return lines.iter().map(|l| l.to_string()).collect();
    }
    let mut out = Vec::new();
    if lines.len() <= view {
        for l in &lines {
            out.push(format!("▒ {l}"));
        }
        out.push("…".to_string());
        return out;
    }
    let hidden = lines.len() - view;
    out.push(format!("▲ {hidden} more lines above"));
    let tail = &lines[hidden..];
    for (i, l) in tail.iter().enumerate() {
        let glyph = if i + 1 == tail.len() { '▓' } else { '▒' };
        out.push(format!("{glyph} {l}"));
    }
    out.push("…".to_string());
    out
}

/// Injected writer for one tool's operator-facing audit block. Production uses
/// stdout; tests use a `Vec<u8>` so dispatcher routing can be verified without
/// process-wide fd redirection.
pub(crate) struct ToolDisplay<W: Write> {
    writer: W,
    color: bool,
    cols: usize,
    spill_lines: usize,
    result_override: Option<String>,
}

impl<W: Write> ToolDisplay<W> {
    pub(crate) fn new(writer: W, color: bool, cols: usize, spill_lines: usize) -> Self {
        Self {
            writer,
            color,
            cols,
            spill_lines,
            result_override: None,
        }
    }

    pub(crate) fn call(&mut self, name: &str, detail: &str) {
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
        let rendered = spill_view_lines(output, self.spill_lines).join("\n");
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
            rendered.push_str(&format!("  … ({hidden} more lines hidden)"));
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
    ToolDisplay::new(io::stdout(), color, term_cols(), spill_lines()).call(name, detail);
}

/// Print completed tool output using the universal #1235 spill height. The
/// legacy `tool_output_lines` argument remains in compatibility signatures but
/// no longer overrides `[tui].spill_lines`.
#[cfg(test)]
pub(crate) fn print_tool_output(output: &str, _tool_output_lines: usize, color: bool) {
    ToolDisplay::new(io::stdout(), color, term_cols(), spill_lines()).result(output);
}

#[cfg(test)]
mod tests {
    use super::{
        fit_line, fmt_tokens, print_harness_notice, print_list_item, print_newt, spill_view_lines,
        tool_call_lines,
    };

    /// #1235: the spill view is TAIL-biased with the issue's gutter glyphs —
    /// small outputs show whole (▒ gutter + … end marker), overflow shows the
    /// LAST `view` lines with the ▲ hidden-count boundary and the ▓ thumb on
    /// the tail line. view=0 = unbounded raw (historical behavior).
    #[test]
    fn spill_view_is_tail_biased_with_gutter_glyphs() {
        // Fits: whole output, ▒ gutter, end marker.
        let small = spill_view_lines("a\nb\nc", 3);
        assert_eq!(small, vec!["▒ a", "▒ b", "▒ c", "…"]);

        // Overflows: LAST view lines (tail is where errors/hits live),
        // ▲ carries the hidden count, ▓ thumbs the tail.
        let big = spill_view_lines("l1\nl2\nl3\nl4\nl5", 3);
        assert_eq!(
            big,
            vec!["▲ 2 more lines above", "▒ l3", "▒ l4", "▓ l5", "…"]
        );

        // Unbounded: raw lines, no gutter.
        assert_eq!(spill_view_lines("x\ny", 0), vec!["x", "y"]);
        // Empty: nothing.
        assert!(spill_view_lines("", 3).is_empty());
    }

    #[test]
    fn completed_tool_output_uses_the_spill_view() {
        let output = "l1\nl2\nl3\nl4\nl5";

        assert_eq!(
            spill_view_lines(output, 3),
            vec!["▲ 2 more lines above", "▒ l3", "▒ l4", "▓ l5", "…"]
        );
        let raw: Vec<String> = output.lines().map(str::to_string).collect();
        assert_eq!(spill_view_lines(output, 0), raw);
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
    fn fit_line_passes_through_when_it_fits() {
        let f = fit_line("hello", 10);
        assert_eq!(f.head, "hello");
        assert_eq!(f.fade, "");
        assert_eq!(f.ellipsis, "");
        // Exact fit is not an overflow.
        let exact = fit_line("hello", 5);
        assert_eq!(exact.ellipsis, "");
        assert_eq!(exact.head, "hello");
    }

    #[test]
    fn fit_line_truncates_with_faded_tail_and_ellipsis() {
        // 11 chars into 6 cols: 5 visible + "…"; last 2 visible cells fade.
        let f = fit_line("abcdefghijk", 6);
        assert_eq!(f.ellipsis, "…");
        assert_eq!(f.head, "abc");
        assert_eq!(f.fade, "de");
        // Reassembled visible width (head + fade + the single ellipsis cell)
        // never exceeds the budget.
        assert!(f.head.chars().count() + f.fade.chars().count() < 6);
    }

    #[test]
    fn fit_line_handles_tiny_budgets() {
        // One column of room still yields a single visible char + ellipsis,
        // never a panic or an empty line.
        let f = fit_line("abcdef", 1);
        assert_eq!(f.ellipsis, "…");
        assert_eq!(f.head, "");
        assert_eq!(f.fade, "a");
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
}
