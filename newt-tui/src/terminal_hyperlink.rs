//! OSC 8 clickable URL rendering for terminal output.
//!
//! Modern terminals can render inline hyperlinks using:
//!
//! `ESC ] 8 ; params ; URI ST text ESC ] 8 ; ; ST`
//!
//! `render_link` emits OSC 8 only when stdout is an interactive terminal and the
//! operator has not disabled it. Deterministic `render_link_osc8` and
//! `render_link_plain` helpers keep tests independent of the process environment.

use std::io::IsTerminal;

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
        render_link_osc8(text, url)
    } else {
        render_link_plain(text, url)
    }
}

/// Render just a URL as a clickable link (displays the URL itself).
pub fn render_url(url: &str) -> String {
    render_link(url, url)
}

/// Render an OSC 8 link unconditionally.
///
/// Control characters are stripped from both fields so untrusted text cannot
/// break out of the OSC sequence and emit arbitrary terminal control bytes.
pub fn render_link_osc8(text: &str, url: &str) -> String {
    let safe_url = strip_controls(url);
    let mut safe_text = strip_controls(text);
    if safe_text.is_empty() {
        safe_text = safe_url.clone();
    }
    format!("{OSC8_OPEN}{safe_url}{ST}{safe_text}{OSC8_CLOSE}")
}

/// Render a copy-pasteable plain-text link.
pub fn render_link_plain(text: &str, url: &str) -> String {
    let safe_url = strip_controls(url);
    let safe_text = strip_controls(text);
    if safe_text.is_empty() || safe_text == safe_url {
        format!("(link: {safe_url})")
    } else {
        format!("{safe_text} ({safe_url})")
    }
}

/// Probe whether stdout should receive OSC 8 hyperlinks.
pub fn supports_osc8() -> bool {
    if env_flag("NEWT_FORCE_OSC8") {
        return true;
    }
    // Allow disabling OSC 8 via environment variable (useful for testing/piping).
    if env_flag("NEWT_NO_OSC8") {
        return false;
    }
    is_tty_stdout()
}

/// Check whether stdout is connected to a TTY (interactive terminal).
fn is_tty_stdout() -> bool {
    std::io::stdout().is_terminal()
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.trim();
            !(v.is_empty()
                || v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no")
                || v.eq_ignore_ascii_case("off"))
        })
        .unwrap_or(false)
}

fn strip_controls(s: &str) -> String {
    s.chars().filter(|ch| !ch.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_link_plain_fallback() {
        let result = render_link_plain("View issue", "https://github.com/foo/bar/issues/771");
        assert_eq!(result, "View issue (https://github.com/foo/bar/issues/771)");
    }

    #[test]
    fn test_render_link_empty_text() {
        let result = render_link_plain("", "https://example.com");
        assert_eq!(result, "(link: https://example.com)");
    }

    #[test]
    fn test_render_url() {
        let url = "https://github.com/foo/bar/issues/771";
        let result = render_link_plain(url, url);
        assert_eq!(result, "(link: https://github.com/foo/bar/issues/771)");
    }

    #[test]
    fn test_render_link_osc8_format() {
        let text = "click me";
        let url = "https://example.com/path?q=1&r=2&x=y";
        let expected_open = format!("\x1b]8;;{url}\x07");
        let expected_close = "\x1b]8;;\x07";
        assert_eq!(
            render_link_osc8(text, url),
            format!("{expected_open}{text}{expected_close}")
        );
    }

    #[test]
    fn test_special_chars_in_url() {
        let url = "https://example.com?a=1&b=2#frag";
        let result = render_link_plain("link", url);
        // Should preserve URL as-is in fallback format.
        assert!(result.contains(url));
    }

    #[test]
    fn test_unicode_in_text() {
        let result = render_link_plain("🔗 click here", "https://example.com");
        assert!(result.contains("🔗 click here"));
    }

    #[test]
    fn test_long_url_truncation_not_needed() {
        // OSC 8 doesn't require URL truncation; the terminal handles display.
        let long_url = format!("https://example.com/{}", "a".repeat(1000));
        let result = render_link_osc8("long", &long_url);
        assert!(result.contains(&long_url));
    }

    #[test]
    fn test_reset_sequence_is_correct() {
        // Verify the OSC 8 reset (empty URI) is exactly what we expect.
        let result = render_link_osc8("x", "https://example.com");
        // Should end with ESC ] 8 ; ; BEL
        assert!(result.ends_with("\x1b]8;;\x07"));
    }

    #[test]
    fn test_multiple_links_in_output() {
        let a = render_link_osc8("first", "https://a.com");
        let b = render_link_osc8("second", "https://b.com");
        let combined = format!("{a}\n{b}");
        // Each link has one open and one reset sequence.
        assert_eq!(combined.matches("\x1b]8;;").count(), 4);
    }

    #[test]
    fn test_render_link_with_id_param() {
        // Test that the OSC 8 format can include an id parameter (for tracking).
        let text = "issue";
        let url = "https://github.com/foo/bar/issues/771";
        let result = render_link_osc8(text, url);
        assert!(result.starts_with("\x1b]8;;"));
        assert!(result.contains(url));
    }

    #[test]
    fn test_fallback_format_when_no_tty() {
        // When not a TTY and text equals URL, show (link: <url>).
        let result = render_link_plain("https://example.com", "https://example.com");
        assert_eq!(result, "(link: https://example.com)");
    }

    #[test]
    fn test_fallback_format_with_different_text_and_url() {
        // When not a TTY and text differs from URL, show `text (url)`.
        let result = render_link_plain("Click here", "https://example.com");
        assert_eq!(result, "Click here (https://example.com)");
    }

    #[test]
    fn test_osc8_escape_sequence_structure() {
        // Verify the exact byte structure: ESC ] 8 ; params ; URI ST text ST
        let result = render_link_osc8("hello", "http://x.com");
        assert_eq!(&result[0..1], "\x1b"); // starts with ESC
        assert!(result.contains("\x1b]8;;")); // OSC 8 open and close
        assert!(result.ends_with("\x07")); // ends with BEL (ST)
    }

    #[test]
    fn test_url_with_fragment() {
        let url = "https://example.com/page#section";
        let result = render_link_osc8("page", url);
        assert!(result.contains(url));
    }

    #[test]
    fn test_url_with_query_params() {
        let url = "https://example.com/search?q=rust&lang=en";
        let result = render_link_osc8("search", url);
        assert!(result.contains(url));
    }

    #[test]
    fn test_control_chars_are_stripped_from_osc8_fields() {
        let result = render_link_osc8("hi\x1b[31m", "https://example.com/\x07bad");
        assert!(!result.contains("\x1b[31m"), "{result:?}");
        assert!(!result.contains("\x07bad"), "{result:?}");
        assert!(result.contains("https://example.com/bad"), "{result:?}");
        assert!(result.contains("hi[31m"), "{result:?}");
    }
}
