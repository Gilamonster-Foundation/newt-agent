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

/// Print a newt response line.
/// Color: orange ▸ (matches the logo).  No-color: >.
pub fn print_newt(msg: &str, color: bool, verbose: bool) {
    if color {
        let prefix = if verbose { "newt ▸  " } else { "▸  " };
        execute!(
            io::stdout(),
            SetForegroundColor(NEWT_ORANGE_CT),
            Print(prefix),
            ResetColor,
            Print(msg),
            Print("\n"),
        )
        .ok();
    } else {
        let prefix = if verbose { "newt >  " } else { ">  " };
        println!("{prefix}{msg}");
    }
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
    if color {
        execute!(
            io::stdout(),
            SetForegroundColor(NEWT_ORANGE_CT),
            Print(format!("⚙  {name}")),
            ResetColor,
            SetForegroundColor(CtColor::DarkGrey),
            Print(format!(": {detail}\n")),
            ResetColor,
        )
        .ok();
    } else {
        println!("⚙  {name}: {detail}");
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
    use super::fmt_tokens;

    #[test]
    fn fmt_tokens_inserts_thousands_separators() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1,000");
        assert_eq!(fmt_tokens(1_234_567), "1,234,567");
    }
}
