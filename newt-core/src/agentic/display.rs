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
use std::io::{self, Write as _};

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
pub(crate) fn emit_compression_notice(color: bool, before: usize, after: usize, how: &str) {
    let msg = format!(
        "⧉  context compressed: ~{} → ~{} est. tokens ({how})",
        fmt_tokens(before.min(u32::MAX as usize) as u32),
        fmt_tokens(after.min(u32::MAX as usize) as u32),
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

/// Print a tool-call header so the user can see what the agent is doing.
pub(crate) fn print_tool_call(name: &str, detail: &str, color: bool) {
    // Keep the "⚙  {name}: " prefix whole (it's short) and fit only the detail
    // into the cells that remain, so a long path/command can't wrap the line.
    let cols = term_cols();
    let prefix_w = 3 + name.chars().count() + 2; // "⚙  " + name + ": "
    let fitted = fit_line(detail, cols.saturating_sub(prefix_w));
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(NEWT_ORANGE_CT),
            Print(format!("⚙  {name}")),
            ResetColor,
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!(": {}", fitted.head)),
            SetForegroundColor(FADE_CT),
            Print(&fitted.fade),
            Print(fitted.ellipsis),
            ResetColor,
            Print("\n"),
        )
        .ok();
    } else {
        println!(
            "⚙  {name}: {}{}{}",
            fitted.head, fitted.fade, fitted.ellipsis
        );
    }
    io::stdout().flush().ok();
}

/// Print tool output truncated to the configured line limit.
/// The model always receives the full content regardless.
pub(crate) fn print_tool_output(output: &str, max_lines: usize, color: bool) {
    if output.is_empty() {
        return;
    }
    let max = max_lines;
    let lines: Vec<&str> = output.lines().collect();
    let shown = if max == 0 {
        lines.len()
    } else {
        lines.len().min(max)
    };
    let hidden = lines.len().saturating_sub(shown);

    let display = lines[..shown].join("\n");

    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!("{display}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("{display}");
    }

    if hidden > 0 {
        // Just print the count and keep going — no blocking prompt.
        // The user can scroll back; the model always gets the full content.
        if color {
            execute!(
                io::stdout(),
                SetForegroundColor(CtColor::DarkGrey),
                Print(format!("  … ({hidden} more lines hidden)\n")),
                ResetColor,
            )
            .ok();
        } else {
            println!("  … ({hidden} more lines hidden)");
        }
    }
    io::stdout().flush().ok();
}

/// Print a capability-denial notice to the user.
pub(crate) fn print_denied(axis: &str, target: &str, color: bool) {
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!(
                "⊘  capability denied: {axis} does not permit '{target}'\n"
            )),
            ResetColor,
        )
        .ok();
    } else {
        println!("⊘  capability denied: {axis} does not permit '{target}'");
    }
    io::stdout().flush().ok();
}

#[cfg(test)]
mod tests {
    use super::{fit_line, fmt_tokens, print_harness_notice, print_list_item, print_newt};

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
