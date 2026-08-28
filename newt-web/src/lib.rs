//! Library face of the newt-web cockpit.
//!
//! The binary keeps its own route modules; this exists so the security-bearing
//! pieces are importable — by the binary, and by suites in `tests/` that would
//! otherwise have to live inside the source files they exercise.

pub mod csp;
pub mod csrf;
pub mod enroll;
pub mod origin;
pub mod webauthn;

/// Minimal HTML escape for text nodes and attribute values.
///
/// Moved here in C3b so the binary's renderers and the library's form helpers
/// share ONE implementation. A second copy is how two escapers drift and one
/// of them stops covering a character.
///
/// `'` is escaped alongside `"`. The attribute values this now produces are
/// written into single-quoted attributes as well as double-quoted ones
/// (HTMX's `hx-vals='{…}'` is single-quoted), and an escaper that covers only
/// one quote style is a break-out waiting for the first value that contains
/// the other. Nothing in the current surface can reach it — ids are minted
/// from a fixed alphabet — which is precisely why it should be closed now,
/// while it is cheap, rather than after a value with an apostrophe arrives.
#[must_use]
pub fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod escape_tests {
    use super::escape_attr;

    #[test]
    fn every_markup_significant_character_is_escaped() {
        assert_eq!(
            escape_attr(r#"&<>"'"#),
            "&amp;&lt;&gt;&quot;&#39;",
            "ampersand must be escaped FIRST or it double-escapes the rest"
        );
        assert_eq!(escape_attr("plain"), "plain");
    }

    /// The single quote is the character the pre-C3b escaper missed, and the
    /// one a single-quoted attribute needs.
    #[test]
    fn a_single_quote_cannot_close_a_single_quoted_attribute() {
        let value = escape_attr("a' onload='alert(1)");
        assert!(!value.contains('\''), "escaped: {value}");
        let attr = format!("<b data-x='{value}'>");
        assert!(!attr.contains("onload='"), "no break-out: {attr}");
    }
}
