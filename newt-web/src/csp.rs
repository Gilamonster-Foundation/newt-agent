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
        format!("style-src 'nonce-{n}'"),
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
