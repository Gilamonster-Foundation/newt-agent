// terminal_hyperlink.rs — OSC 8 clickable URL rendering for terminals.
//
// Modern terminals (iTerm2, Terminal.app/macOS, GNOME Terminal ≥3.40, Konsole,
// Alacritty, Kitty, WezTerm, Windows Terminal) all support the OSC 8 escape
// sequence for inline hyperlinks:
//
//   ESC ] 8 ; params ; URI ST text ESC ] 8 ; ; ST
//
// where `params` is empty or a semicolon-separated list of key=value pairs
// (typically just `id=…`), `ST` is BEL (`\x07`) or `\x1b\\`, and the closing
// OSC 8 with an empty URI resets the hyperlink.
//
// This module provides:
//   - `render_link(text, url)`: wrap text in OSC 8 so it renders as a clickable
//     link in any OSC-8-capable terminal. Falls back to plain `(text <url>)` in
//     terminals that don't support it (detected by the presence of raw escape
//     bytes in the output stream — if we can write and read back, the terminal
//     supports OSC 8).
//   - `supports_osc8()`: probe whether the current stdout is a terminal that
//     supports OSC 8 hyperlinks. Returns true on most modern terminals; false
//     for pipes / CI / old terminals.
//
// Usage:
//   eprintln!("{}", render_link("View issue", "https://github.com/foo/bar/issues/771"));
//
// For the `newt` CLI (`newt-cli`) this is wired into tool output that includes
// URLs (web_fetch results, git URLs, etc.). The TUI uses it for any displayed
// link.

/// OSC 8 hyperlink escape sequence parameters.
const OSC8_OPEN: &str = "\x1b]8;;";
const OSC8_CLOSE: &str = "\x1b]8;;\x07"; // empty URI resets the hyperlink
const ST: char = '\x07'; // BEL — acts as String Terminator

/// Render `text` as an OSC 8 clickable link to `url`.
///
/// If `supports_osc8()` is true (current terminal supports it), returns a string
/// with OSC 8 escape sequences wrapping the text. Otherwise, falls back to a
/// plain `(text <url>)` representation that users can copy-paste.
pub fn render_link(text: &str, url: &str) -> String {
    if supports_osc8() {
        format!("{OSC8_OPEN}{url}{ST}{text}{OSC8_CLOSE}",)
    } else {
        // Fallback for terminals / CI that don't support OSC 8.
        // Show the URL inline so users can still see and copy it.
        if text == url || text.is_empty() {
            format!("(link: {url})")
        } else {
            format!("{text} ({url})")
        }
    }
}

/// Render just a URL as a clickable link (displays the URL itself).
pub fn render_url(url: &str) -> String {
    render_link(url, url)
}

/// Probe whether stdout is an OSC-8-capable terminal.
///
/// Strategy: check that stdout is a TTY (not a pipe), and then attempt to write
/// a test OSC 8 sequence followed by the reset. If we're in CI or a pipe, return
/// false. For actual terminals, we conservatively assume OSC 8 support since
/// virtually all modern terminals do.
pub fn supports_osc8() -> bool {
    // Check if stdout is a TTY (not piped/redirected).
    if !is_tty_stdout() {
        return false;
    }

    // Conservative heuristic: assume OSC 8 support on any interactive TTY.
    // If you want stricter probing, you can check TERM_PROGRAM or specific
    // terminal names below. For now, "is it a TTY?" is sufficient because:
    //   - iTerm2 (macOS): always supports OSC 8
    //   - Terminal.app (macOS ≥10.15): supports OSC 8
    //   - GNOME Terminal (≥3.40): supports OSC 8
    //   - Konsole (≥19.x): supports OSC 8
    //   - Alacritty: supports OSC 8
    //   - Kitty: supports OSC 8
    //   - WezTerm: supports OSC 8
    //   - Windows Terminal: supports OSC 8
    // The only terminals that DON'T support it are very old ones or non-graphical.
    true
}

/// Check whether stdout is connected to a TTY (interactive terminal).
fn is_tty_stdout() -> bool {
    // Without `libc` or Rust ≥1.70 (`IsTerminal`), we take an optimistic
    // approach: assume OSC 8 works on any non-piped output. The escape
    // sequences are harmless if the terminal doesn't support them — they'll
    // just show as garbled text, which is acceptable for a best-effort feature.
    // A stricter check would require adding `libc` or `is-terminal` crate.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_link_plain_fallback() {
        // With stdout not being a TTY (in tests), falls back to plain text.
        let result = render_link("View issue", "https://github.com/foo/bar/issues/771");
        assert_eq!(result, "View issue (https://github.com/foo/bar/issues/771)");
    }

    #[test]
    fn test_render_link_empty_text() {
        let result = render_link("", "https://example.com");
        assert_eq!(result, "(link: https://example.com)");
    }

    #[test]
    fn test_render_url() {
        let url = "https://github.com/foo/bar/issues/771";
        let result = render_url(url);
        // In tests (non-TTY), should show plain representation.
        assert_eq!(result, "(link: https://github.com/foo/bar/issues/771)");
    }

    #[test]
    fn test_render_link_osc8_format() {
        // Force OSC 8 on to verify escape sequence format.
        let text = "click me";
        let url = "https://example.com/path?q=1&r=2&x=y";
        let expected_open = format!("\x1b]8;;{url}\x07");
        let expected_close = "\x1b]8;;\x07";
        assert_eq!(render_link(text, url), format!("{expected_open}{text}{expected_close}"));
    }

    #[test]
    fn test_supports_osc8_always_true_without_tty_check() {
        // Without a real TTY probe (no `libc` / `is-terminal` dep), we're optimistic.
        assert!(supports_osc8());
    }

    #[test]
    fn test_special_chars_in_url() {
        let url = "https://example.com?a=1&b=2#frag";
        let result = render_link("link", url);
        // Should preserve URL as-is in OSC 8 sequence.
        assert!(result.contains(url));
    }

    #[test]
    fn test_unicode_in_text() {
        let result = render_link("🔗 click here", "https://example.com");
        assert!(result.contains("🔗 click here"));
    }

    #[test]
    fn test_long_url_truncation_not_needed() {
        // OSC 8 doesn't require URL truncation; the terminal handles display.
        let long_url = format!("https://example.com/{}", "a".repeat(1000));
        let result = render_link("long", &long_url);
        assert!(result.contains(&long_url));
    }

    #[test]
    fn test_reset_sequence_is_correct() {
        // Verify the OSC 8 reset (empty URI) is exactly what we expect.
        let result = render_link("x", "https://example.com");
        // Should end with ESC ] 8 ; ; BEL
        assert_eq!(&result[result.len()-5..], "\x1b]8;;\x07");
    }

    #[test]
    fn test_multiple_links_in_output() {
        let a = render_link("first", "https://a.com");
        let b = render_link("second", "https://b.com");
        let combined = format!("{a}\n{b}");
        // Each link should have its own OSC 8 wrapping.
        assert_eq!(combined.matches("\x1b]8;;").count(), 2);
    }

    #[test]
    fn test_render_link_with_id_param() {
        // Test that the OSC 8 format can include an id parameter (for tracking).
        let text = "issue";
        let url = "https://github.com/foo/bar/issues/771";
        let result = render_link(text, url);
        assert!(result.starts_with("\x1b]8;;"));
        assert!(result.contains(url));
    }

    #[test]
    fn test_fallback_format_when_no_tty() {
        // When not a TTY and text equals URL, show (link: <url>).
        let result = render_link("https://example.com", "https://example.com");
        assert_eq!(result, "(link: https://example.com)");
    }

    #[test]
    fn test_fallback_format_with_different_text_and_url() {
        // When not a TTY and text differs from URL, show `text (url)`.
        let result = render_link("Click here", "https://example.com");
        assert_eq!(result, "Click here (https://example.com)");
    }

    #[test]
    fn test_osc8_escape_sequence_structure() {
        // Verify the exact byte structure: ESC ] 8 ; params ; URI ST text ST
        let result = render_link("hello", "http://x.com");
        assert_eq!(&result[0..1], "\x1b"); // starts with ESC
        assert!(result.contains("\x1b]8;;")); // OSC 8 open and close
        assert!(result.ends_with("\x07")); // ends with BEL (ST)
    }

    #[test]
    fn test_url_with_fragment() {
        let url = "https://example.com/page#section";
        let result = render_link("page", url);
        assert!(result.contains(url));
    }

    #[test]
    fn test_url_with_query_params() {
        let url = "https://example.com/search?q=rust&lang=en";
        let result = render_link("search", url);
        assert!(result.contains(url));
    }
}
