//! Content-Security-Policy nonces and Subresource Integrity for the cockpit.
//!
//! The enrollment page handles a WebAuthn ceremony, so script injection there
//! is not a defacement risk — it is a credential-theft risk. Injected script
//! could stage a candidate for an attacker's authenticator while showing the
//! operator the words for their own.
//!
//! Two mechanisms, aimed at different attackers:
//!
//! * **CSP nonce** stops script the *page* did not author. A fresh nonce per
//!   response means an injected `<script>` has no way to carry a valid one, and
//!   `script-src` names no host, scheme, or `'unsafe-inline'` to fall back on.
//! * **SRI** stops a *served asset* that changed under us — a tampered
//!   `htmx.min.js` on disk or in a cache. The nonce would happily admit it,
//!   because the tag really is the page's own.
//!
//! Neither substitutes for the other, which is why #1372 requires both.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use sha2::{Digest, Sha384};

/// A per-response CSP nonce.
///
/// There is no way to build one from a caller-supplied string: a nonce that an
/// attacker can predict or replay is not a nonce, and the surest way to prevent
/// a reused constant is to make reuse unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nonce(String);

impl Nonce {
    /// Mint a fresh nonce.
    ///
    /// Reuses newt-core's unguessable id generator — the same source the
    /// permission-request nonces already trust — rather than adding a second
    /// randomness dependency with its own failure modes.
    #[must_use]
    pub fn fresh() -> Self {
        Self(newt_core::new_conversation_id())
    }

    /// The token, for the `nonce=` attribute and the header.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The Subresource Integrity attribute value for `bytes` — `sha384-<base64>`.
///
/// SHA-384 because SRI fixes the algorithm set; this is not a place the
/// multihash preference applies.
#[must_use]
pub fn sri(bytes: &[u8]) -> String {
    format!("sha384-{}", B64.encode(Sha384::digest(bytes)))
}

/// The `Content-Security-Policy` header value for a page carrying `nonce`.
///
/// `default-src 'none'` is the point: every fetch directive that is not named
/// below is denied, so a directive we forgot fails closed instead of inheriting
/// something permissive. `script-src` carries only the nonce — no host, no
/// scheme, no `'unsafe-inline'`, no `'unsafe-eval'` — so there is no fallback
/// for injected script to reach for.
#[must_use]
pub fn policy(nonce: &Nonce) -> String {
    let n = nonce.as_str();
    [
        "default-src 'none'".to_string(),
        format!("script-src 'nonce-{n}'"),
        // CSP3 splits style into ELEMENT and ATTRIBUTE sources, and newt needs
        // them split. Measured on the real page (49 `style-src-attr` blocks vs
        // 4 `style-src-elem`): Mermaid styles the SVG it generates almost
        // entirely through per-node `style=` attributes.
        //
        // `style-src-elem` stays strict — a `<style>` element or a stylesheet
        // is where style injection has teeth, and untrusted markup reaching
        // one would be a real finding.
        //
        // `style-src-attr 'unsafe-inline'` grants an attacker NOTHING here,
        // and that is a fact about the sanitizer rather than a hope: ammonia's
        // defaults are `generic_attributes = {"lang","title"}` and
        // `clean_content_tags = {"script","style"}`, so `style` is not an
        // allowed attribute on any tag and untrusted content cannot emit one.
        // The only inline style attributes on the page are the ones our own
        // pinned, SRI-bound Mermaid bundle generates. A style attribute also
        // cannot execute, and `img-src 'self' data:` closes the CSS
        // exfiltration channel that would otherwise make one interesting.
        //
        // `style-src` remains as the fallback for a browser that implements
        // neither, where it fails CLOSED (attributes blocked, diagrams
        // degrade) rather than open.
        format!("style-src 'nonce-{n}'"),
        format!("style-src-elem 'nonce-{n}'"),
        "style-src-attr 'unsafe-inline'".to_string(),
        // The cockpit talks only to itself: htmx posts and the SSE stream.
        "connect-src 'self'".to_string(),
        "img-src 'self' data:".to_string(),
        "font-src 'none'".to_string(),
        // No <base> rewriting the meaning of every relative URL on the page.
        "base-uri 'none'".to_string(),
        "form-action 'self'".to_string(),
        "frame-ancestors 'none'".to_string(),
        "object-src 'none'".to_string(),
    ]
    .join("; ")
}

/// Whether `policy` permits an inline `<style>` ELEMENT to apply.
///
/// Derived from the policy TEXT rather than restated as a constant, so the
/// answer cannot drift from the header actually sent — it reads the same bytes.
///
/// The page passes this to the client as data. The alternative, letting the
/// browser feature-detect by injecting a probe `<style>`, works but is
/// self-defeating: the probe is itself an inline style element, so it TRIPS
/// the very violation it is testing for, and a page that reports its own
/// violations can then never assert it has none. The server knows its own
/// policy; asking the browser is indirection with a side effect.
#[must_use]
pub fn permits_inline_style_elements(policy: &str) -> bool {
    let directive = policy
        .split(';')
        .map(str::trim)
        .find(|d| d.split_whitespace().next() == Some("style-src-elem"))
        .or_else(|| {
            policy
                .split(';')
                .map(str::trim)
                .find(|d| d.split_whitespace().next() == Some("style-src"))
        });
    directive.is_some_and(|d| d.contains("'unsafe-inline'"))
}

/// The vendored htmx bundle, so both the route and its SRI digest read the
/// same bytes. A digest computed over anything else is a lie.
pub const HTMX_JS: &str = include_str!("../assets/htmx.min.js");

/// The passkey ceremony script.
pub const WEBAUTHN_JS: &str = include_str!("../assets/webauthn.js");

/// The Markdown progressive-enhancement adapter.
pub const MARKDOWN_JS: &str = include_str!("../assets/markdown.js");

/// The live-transcript attachment script.
///
/// It exists as a FILE rather than inline markup for a CSP reason (#1854):
/// the panel it drives is an HTMX fragment, and a fragment cannot carry a
/// nonce. Kept beside the other served scripts so a tag and its SRI digest
/// always read the same bytes.
pub const PANEL_JS: &str = include_str!("../assets/panel.js");

/// The vendored Mermaid runtime.
pub const MERMAID_JS: &str = include_str!("../assets/mermaid.min.js");

/// Security headers every cockpit page carries, alongside the CSP.
#[must_use]
pub fn hardening_headers() -> [(&'static str, &'static str); 4] {
    [
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        ("cross-origin-opener-policy", "same-origin"),
    ]
}
